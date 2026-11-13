//! Conservative preservation policy and fail-closed checks for smaller videos.
//!
//! SSIM is a useful rejection gate, not a guarantee of invisible differences.
//! Compare at the chosen output resolution so deliberate downscaling is allowed.

use crate::hw::HwCaps;
use crate::media::MediaFile;
use crate::pipeline::{
    audio_track_codec, effective_scale_target, forces_audio_reencode, AudioCodec, NvencPreset,
    ScalePolicy, VideoBackend,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

pub const VIDEO_CQ: u32 = 18;
/// Highest CQ/CRF the automatic search is allowed to try. The manual slider
/// tops out at the same value, so a found setting is always one the user
/// could have chosen by hand.
pub const MAX_SEARCH_CQ: u32 = 40;
/// Sample gates sit slightly above the full-file gate: a lucky three-second
/// window must not promote a CRF that the complete encode would reject.
const SAMPLE_MIN_MEAN_SSIM: f64 = 0.992;
const SAMPLE_MIN_FRAME_SSIM: f64 = 0.98;
/// Five probes narrow the 18–40 range to the exact point; each probe is
/// one short sample per scene. More would multiply pre-encode time for
/// nothing.
const MAX_SEARCH_PROBES: u32 = 5;
const SAMPLE_SECONDS: f64 = 3.0;
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
        self.verify_with(MIN_MEAN_SSIM, MIN_FRAME_SSIM)
    }

    fn verify_with(&self, min_mean: f64, min_frame: f64) -> Result<f64, String> {
        if self.frames == 0 || self.invalid {
            return Err("video comparison produced no frame scores".into());
        }
        let mean = self.sum / self.frames as f64;
        let worst = self.worst.unwrap_or(0.0);
        if mean < min_mean || worst < min_frame {
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
    let scores = compare_ssim(&source.path, output, result.width, result.height, cancel)?;
    scores.verify()
}

/// SSIM of every output frame against the reference (the source or a
/// lossless sample of it), with the reference scaled to the output size.
fn compare_ssim(
    reference: &Path,
    output: &Path,
    width: u32,
    height: u32,
    cancel: &Arc<AtomicBool>,
) -> Result<Scores, String> {
    let filter = format!(
        "[0:v:0]scale={width}:{height}:flags=bicubic,setsar=1,settb=AVTB,setpts=PTS-STARTPTS[ref];\
         [1:v:0]setsar=1,settb=AVTB,setpts=PTS-STARTPTS[out];\
         [out][ref]ssim=stats_file=-:shortest=1:repeatlast=0[checked]"
    );
    let args = vec![
        "-v".into(),
        "error".into(),
        "-xerror".into(),
        "-i".into(),
        reference.to_string_lossy().into_owned(),
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
    analyse::<Scores>("ffmpeg", &args, cancel, Scores::read)
}

/// Result of the pre-encode quality search: the strongest setting that
/// still passed the sample gate, its weakest sampled SSIM, and how many
/// candidates were tried.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoQuality {
    pub cq: u32,
    pub sample_ssim: f64,
    pub probes: u32,
}

/// One candidate's outcome.
enum Probe {
    Pass(f64),
    Miss,
}

/// Find the highest CQ/CRF that still passes the sample gate. Binary search
/// is valid because quality degrades monotonically with CQ in practice; a
/// non-monotonic fluke can only cost a little size, never the full-file
/// check, which stays authoritative.
fn search_cq(
    mut probe: impl FnMut(u32) -> Result<Probe, String>,
) -> Result<Option<AutoQuality>, String> {
    let mut lo = VIDEO_CQ;
    let mut hi = MAX_SEARCH_CQ;
    let mut best: Option<(u32, f64)> = None;
    let mut probes = 0;
    while lo < hi && probes < MAX_SEARCH_PROBES {
        let cq = lo + (hi - lo).div_ceil(2);
        probes += 1;
        match probe(cq)? {
            Probe::Pass(score) => {
                best = Some((cq, score));
                lo = cq;
            }
            Probe::Miss => hi = cq - 1,
        }
    }
    Ok(best.map(|(cq, sample_ssim)| AutoQuality {
        cq,
        sample_ssim,
        probes,
    }))
}

/// Representative windows for the search: up to three short clips from
/// across the file (more windows multiply encode time; long files stay at
/// three because scene variety matters more than another probe).
fn sample_windows(duration: f64) -> Vec<(f64, f64)> {
    if !duration.is_finite() || duration < 4.0 {
        return vec![];
    }
    let len = (duration / 12.0)
        .clamp(1.5, SAMPLE_SECONDS)
        .min(duration / 2.0);
    let fractions: &[f64] = if duration >= len * 4.0 {
        &[0.15, 0.5, 0.85]
    } else {
        &[0.5]
    };
    fractions
        .iter()
        .map(|f| ((duration * f - len / 2.0).clamp(0.0, duration - len), len))
        .collect()
}

fn sample_media(media: &MediaFile, path: &Path) -> MediaFile {
    let mut sample = media.clone();
    sample.path = path.to_path_buf();
    sample.bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    sample
}

static SEARCH_SEQ: AtomicU64 = AtomicU64::new(0);

fn search_dir(work_dir: &Path) -> PathBuf {
    let seq = SEARCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    work_dir.join(format!("auto-quality-{}-{seq}-{nanos}", std::process::id()))
}

/// Encode short samples at several CQ/CRF values and return the strongest
/// one whose SSIM still clears the sample gate. `None` means "keep the
/// conservative fallback": a very short clip, an unsupported sample copy,
/// or a cancelled/failed probe all degrade to the previous fixed quality,
/// never to a skipped file.
#[allow(clippy::too_many_arguments)]
pub fn auto_cq(
    media: &MediaFile,
    backend: VideoBackend,
    preset: NvencPreset,
    scale: ScalePolicy,
    film_grain: bool,
    caps: &HwCaps,
    work_dir: &Path,
    cancel: &Arc<AtomicBool>,
) -> Option<AutoQuality> {
    let windows = sample_windows(media.duration_s);
    if windows.is_empty() || backend == VideoBackend::CopyVideo || media.width < 16 {
        return None;
    }
    let (width, height) = effective_scale_target(media, scale)
        .unwrap_or((media.width, media.height))
        .max((16, 16));
    let dir = search_dir(work_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let result = search_in(
        media, backend, preset, scale, film_grain, caps, &dir, &windows, width, height, cancel,
    );
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(found) => found,
        Err(e) => {
            if !cancel.load(Ordering::Relaxed) {
                crate::ffmpeg::log::debug_log(&format!(
                    "auto quality: {e}; using conservative CRF"
                ));
            }
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn search_in(
    media: &MediaFile,
    backend: VideoBackend,
    preset: NvencPreset,
    scale: ScalePolicy,
    film_grain: bool,
    caps: &HwCaps,
    dir: &Path,
    windows: &[(f64, f64)],
    width: u32,
    height: u32,
    cancel: &Arc<AtomicBool>,
) -> Result<Option<AutoQuality>, String> {
    let mut references = Vec::with_capacity(windows.len());
    for (i, (start, len)) in windows.iter().enumerate() {
        let path = dir.join(format!("ref-{i}.mkv"));
        crate::ffmpeg::extract_video_sample(media, *start, *len, &path, cancel)?;
        references.push(path);
    }
    let mut samples = Vec::with_capacity(references.len());
    for path in &references {
        samples.push(sample_media(media, path));
    }
    let found = search_cq(|cq| {
        let mut weakest = f64::INFINITY;
        for (i, (sample, path)) in samples.iter().zip(&references).enumerate() {
            let out = dir.join(format!("cq{cq}-{i}.mkv"));
            let encoded = crate::ffmpeg::encode_sample(
                sample, &out, backend, preset, cq, scale, film_grain, caps, cancel,
            );
            if let Err(e) = encoded {
                let _ = std::fs::remove_file(&out);
                return Err(e);
            }
            let score = compare_ssim(path, &out, width, height, cancel)
                .and_then(|s| s.verify_with(SAMPLE_MIN_MEAN_SSIM, SAMPLE_MIN_FRAME_SSIM));
            let _ = std::fs::remove_file(&out);
            match score {
                Ok(mean) => weakest = weakest.min(mean),
                Err(_) if cancel.load(Ordering::Relaxed) => return Err("cancelled".into()),
                Err(_) => return Ok(Probe::Miss),
            }
        }
        if weakest.is_finite() {
            Ok(Probe::Pass(weakest))
        } else {
            Ok(Probe::Miss)
        }
    })?;
    for path in references {
        let _ = std::fs::remove_file(path);
    }
    Ok(found)
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

    #[test]
    fn search_finds_the_highest_cq_that_still_passes() {
        let found = search_cq(|cq| {
            Ok(if cq <= 31 {
                Probe::Pass(0.995)
            } else {
                Probe::Miss
            })
        })
        .unwrap()
        .expect("a passing candidate exists");
        assert_eq!(found.cq, 31);
        assert_eq!(found.probes, MAX_SEARCH_PROBES);
        assert!(found.sample_ssim >= SAMPLE_MIN_MEAN_SSIM);
    }

    #[test]
    fn search_falls_back_when_every_probe_misses_or_a_probe_errors() {
        let none = search_cq(|_| Ok(Probe::Miss)).unwrap();
        assert_eq!(none, None);
        let err = search_cq(|_| Err("encoder unavailable".into())).unwrap_err();
        assert!(err.contains("encoder unavailable"));
    }

    #[test]
    fn sample_windows_stay_inside_the_source_and_skip_tiny_clips() {
        assert!(sample_windows(3.9).is_empty());
        assert!(sample_windows(f64::NAN).is_empty());
        for duration in [4.0, 12.0, 600.0, 3600.0, 10_800.0] {
            let windows = sample_windows(duration);
            assert!(!windows.is_empty(), "{duration}");
            for (start, len) in windows {
                assert!((1.5..=SAMPLE_SECONDS).contains(&len));
                assert!(start >= 0.0);
                assert!(start + len <= duration + 0.001, "{duration}: {start}+{len}");
            }
        }
        // Long films sample three distinct scenes; tiny clips use one.
        assert_eq!(sample_windows(3600.0).len(), 3);
        assert_eq!(sample_windows(4.0).len(), 1);
    }
}
