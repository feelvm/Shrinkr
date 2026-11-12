//! Conservative preservation policy and fail-closed checks for smaller videos.
//!
//! SSIM is a useful rejection gate, not a guarantee of invisible differences.
//! Compare at the chosen output resolution so deliberate downscaling is allowed.

use crate::media::MediaFile;
use crate::pipeline::{audio_track_codec, forces_audio_reencode, AudioCodec, VideoBackend};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const VIDEO_CQ: u32 = 18;
const MIN_MEAN_SSIM: f64 = 0.99;
const MIN_FRAME_SSIM: f64 = 0.97;

/// Use the requested audio target only when every track can retain at least
/// 90% of its bitrate. Unknown rates and lossless audio stay untouched. A single
/// unsafe track makes the whole file copy audio; this keeps the shared encoder
/// and estimator policy consistent without silently sacrificing another track.
pub fn audio_target(m: &MediaFile, requested: Option<u32>, backend: VideoBackend) -> Option<u32> {
    let target = requested?;
    let force = forces_audio_reencode(backend, requested);
    for track in &m.audio {
        if matches!(track.codec.as_str(), "flac" | "alac" | "wavpack" | "truehd")
            || track.codec.starts_with("pcm_")
        {
            return None;
        }
        let source = track.bitrate.filter(|&b| b > 0)?;
        if force && u64::from(target) >= source {
            return None;
        }
        let codec = audio_track_codec(track, requested, false, force);
        let fallback = audio_track_codec(track, requested, true, force);
        for codec in [codec, fallback] {
            let rate = match codec {
                AudioCodec::Copy => continue,
                AudioCodec::Opus(bps) | AudioCodec::Aac(bps) => u64::from(bps),
            };
            if rate.saturating_mul(10) < source.saturating_mul(9) {
                return None;
            }
        }
    }
    Some(target)
}

#[derive(Default, Debug)]
struct Scores {
    frames: u64,
    sum: f64,
    worst: Option<f64>,
    invalid: bool,
}

impl Scores {
    fn read(&mut self, line: &str) {
        let Some(raw) = line
            .split_whitespace()
            .find_map(|part| part.strip_prefix("All:"))
        else {
            return;
        };
        let score = raw.parse::<f64>().ok();
        if let Some(score) = score.filter(|s| s.is_finite() && (-1.0..=1.0).contains(s)) {
            self.frames += 1;
            self.sum += score;
            self.worst = Some(self.worst.map_or(score, |old| old.min(score)));
        } else {
            self.invalid = true;
        }
    }

    fn verify(&self) -> Result<f64, String> {
        if self.frames == 0 || self.invalid {
            return Err("video comparison produced no frame scores".into());
        }
        let mean = self.sum / self.frames as f64;
        let worst = self.worst.unwrap_or(0.0);
        if mean < MIN_MEAN_SSIM || worst < MIN_FRAME_SSIM {
            return Err(format!(
                "video differs too much (SSIM average {mean:.4}, lowest frame {worst:.4})"
            ));
        }
        Ok(mean)
    }
}

/// Decode both complete videos, reject damaged streams, and compare every
/// output frame to the source resized to the requested resolution. Audio packet
/// totals additionally enforce the 10% limit on actual re-encoded audio, rather
/// than trusting encoder targets (which can undershoot, especially with VBR).
pub fn verify(source: &MediaFile, output: &Path, cancel: &Arc<AtomicBool>) -> Result<f64, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let result = crate::media::probe_file(output).map_err(|e| format!("quality probe: {e:#}"))?;
    if result.bytes >= source.bytes {
        return Err("output is not smaller".into());
    }
    let tolerance = (1.0 / source.fps.max(1.0)).clamp(0.02, 0.1);
    if source.duration_s <= 0.0
        || result.duration_s <= 0.0
        || (result.duration_s - source.duration_s).abs() > tolerance
    {
        return Err("duration changed or could not be verified".into());
    }
    if result.audio.len() != source.audio.len() || result.sub_count != source.sub_count {
        return Err("an audio or subtitle track is missing".into());
    }
    if source.fps > 0.0 && (result.fps - source.fps).abs() > source.fps * 0.001 {
        return Err("video frame rate changed".into());
    }
    if result.bit_depth < source.bit_depth {
        return Err("video bit depth was reduced".into());
    }
    if source.is_hdr()
        && (result.color_primaries != source.color_primaries
            || result.color_transfer != source.color_transfer
            || result.color_space != source.color_space)
    {
        return Err("HDR color information changed".into());
    }
    for (input, output) in source.audio.iter().zip(&result.audio) {
        if input.channels != output.channels || input.sample_rate != output.sample_rate {
            return Err("audio channels or sample rate changed".into());
        }
    }
    // The protected encoder only copies audio or converts to Opus/AAC. Compare
    // packet totals even for same-codec re-encodes so those cannot evade the cap.
    if !source.audio.is_empty() {
        let before = audio_packet_bytes(&source.path, cancel)?;
        let after = audio_packet_bytes(output, cancel)?;
        if before.len() != source.audio.len() || after.len() != source.audio.len() {
            return Err("audio bitrate could not be verified".into());
        }
        for (src, dst) in before.iter().zip(after.iter()) {
            // Streams have the same duration (checked above), so packet-byte
            // ratios measure the bitrate reduction without container overhead.
            if *src == 0 || (*dst as u128) * 10 < (*src as u128) * 9 {
                return Err("audio bitrate fell by more than 10%".into());
            }
        }
    }
    let filter = format!(
        "[0:v:0]scale={}:{}:flags=bicubic,setsar=1,settb=AVTB,setpts=PTS-STARTPTS[ref];\
         [1:v:0]setsar=1,settb=AVTB,setpts=PTS-STARTPTS[out];\
         [out][ref]ssim=stats_file=-:shortest=1:repeatlast=0[checked]",
        result.width, result.height
    );
    let args = vec![
        "-v".into(),
        "error".into(),
        "-xerror".into(),
        "-i".into(),
        source.path.to_string_lossy().into_owned(),
        "-xerror".into(),
        "-i".into(),
        output.to_string_lossy().into_owned(),
        "-filter_complex".into(),
        filter,
        "-map".into(),
        "[checked]".into(),
        "-an".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ];
    let scores = analyse::<Scores>("ffmpeg", &args, cancel, Scores::read)?;
    scores.verify()
}

#[derive(Default)]
struct AudioBytes(std::collections::BTreeMap<usize, u64>);

impl AudioBytes {
    fn read(&mut self, line: &str) {
        let mut parts = line.split(',');
        if let (Some(stream), Some(bytes)) = (parts.next(), parts.next()) {
            if let (Ok(stream), Ok(bytes)) = (stream.parse::<usize>(), bytes.parse::<u64>()) {
                *self.0.entry(stream).or_default() += bytes;
            }
        }
    }
}

fn audio_packet_bytes(path: &Path, cancel: &Arc<AtomicBool>) -> Result<Vec<u64>, String> {
    let args: Vec<String> = [
        "-v",
        "error",
        "-select_streams",
        "a",
        "-show_packets",
        "-show_entries",
        "packet=stream_index,size",
        "-of",
        "csv=p=0",
    ]
    .into_iter()
    .map(String::from)
    .chain([path.to_string_lossy().into_owned()])
    .collect();
    let bytes = analyse::<AudioBytes>("ffprobe", &args, cancel, AudioBytes::read)?;
    Ok(bytes.0.into_values().collect())
}

/// Read analysis output on workers and poll the process independently of its
/// output, so Cancel also works during a stalled decode. Reap on every path.
fn analyse<T: Default + Send + 'static>(
    program: &str,
    args: &[String],
    cancel: &Arc<AtomicBool>,
    read: fn(&mut T, &str),
) -> Result<T, String> {
    let mut value = T::default();
    let outcome = crate::process::run_with_progress(
        crate::process::cmd(program).args(args),
        cancel,
        true,
        |line| read(&mut value, line),
    )?;
    if !outcome.status.success() {
        return Err(format!("quality check failed: {}", outcome.stderr_tail));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> MediaFile {
        MediaFile {
            path: "test.mkv".into(),
            bytes: 1_000_000,
            vcodec: "h264".into(),
            width: 320,
            height: 240,
            fps: 30.0,
            pix_fmt: "yuv420p".into(),
            bit_depth: 8,
            color_space: String::new(),
            color_primaries: String::new(),
            color_transfer: String::new(),
            color_range: String::new(),
            vbitrate: None,
            format_bitrate: None,
            duration_s: 10.0,
            audio: vec![crate::media::AudioTrack {
                codec: "aac".into(),
                bitrate: Some(100_000),
                channels: 2,
                sample_rate: Some(48_000),
                layout: "stereo".into(),
            }],
            acodec: "aac".into(),
            video_stream_count: 1,
            audio_stream_count: 1,
            sub_count: 0,
            attach_count: 0,
            has_chapters: false,
        }
    }

    #[test]
    fn audio_limit_includes_the_ten_percent_boundary() {
        let m = source();
        assert_eq!(
            audio_target(&m, Some(90_000), VideoBackend::Auto),
            Some(90_000)
        );
        assert_eq!(audio_target(&m, Some(89_000), VideoBackend::Auto), None);
        assert_eq!(audio_target(&m, None, VideoBackend::Auto), None);
    }

    #[test]
    fn unsafe_secondary_unknown_lossless_and_clamped_audio_are_copied() {
        let mut m = source();
        m.audio.push(m.audio[0].clone());
        m.audio[1].bitrate = Some(200_000);
        assert_eq!(audio_target(&m, Some(90_000), VideoBackend::Auto), None);
        m.audio.pop();
        m.audio[0].bitrate = None;
        assert_eq!(audio_target(&m, Some(90_000), VideoBackend::Auto), None);
        m.audio[0].bitrate = Some(100_000);
        m.audio[0].codec = "flac".into();
        assert_eq!(audio_target(&m, Some(90_000), VideoBackend::Auto), None);
        m.audio[0].codec = "aac".into();
        m.audio[0].channels = 1;
        m.audio[0].bitrate = Some(600_000);
        assert_eq!(audio_target(&m, Some(512_000), VideoBackend::Auto), None);
    }

    #[test]
    fn audio_only_can_reduce_slightly_but_never_upscale() {
        let m = source();
        assert_eq!(
            audio_target(&m, Some(90_000), VideoBackend::CopyVideo),
            Some(90_000)
        );
        assert_eq!(
            audio_target(&m, Some(192_000), VideoBackend::CopyVideo),
            None
        );
    }

    #[test]
    fn quality_gate_rejects_a_bad_frame_even_with_a_high_average() {
        let mut scores = Scores::default();
        for _ in 0..999 {
            scores.read("n:1 Y:1 U:1 V:1 All:1.000000 (inf)");
        }
        scores.read("n:1000 All:0.95");
        assert!(scores.verify().is_err());
        let mut inverted = Scores::default();
        inverted.read("n:1 All:-0.5");
        assert!(inverted.verify().unwrap_err().contains("video differs"));
    }

    #[test]
    fn quality_gate_fails_closed_and_accepts_close_frames() {
        assert!(Scores::default().verify().is_err());
        let mut scores = Scores::default();
        scores.read("n:1 All:NaN");
        assert!(scores.verify().is_err());
        let mut scores = Scores::default();
        scores.read("n:1 All:0.995");
        scores.read("n:2 All:0.998");
        assert!(scores.verify().is_ok());
    }
}
