//! Media inspection: ffprobe-based demux/inspect of video/audio/subs/attachments.
//!
//! The GUI + pipeline layer uses [`MediaFile`] as the single source of truth
//! about an input. It is deliberately richer than the original
//! `{vcodec,width,height,duration,acodec}` struct so preflight, audio and
//! scaling decisions can be made without re-probing.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One audio track (primary track is index 0).
#[derive(Clone, Debug, Default)]
pub struct AudioTrack {
    pub codec: String,
    /// bits per second, if reported.
    pub bitrate: Option<u64>,
    pub channels: u8,
    pub sample_rate: Option<u32>,
    /// ffprobe `channel_layout`, e.g. "stereo", "5.1", "5.1(side)".
    /// "" when unreported. Decides Opus-vs-AAC: Opus mapping families
    /// reject exotic layouts like 5.1(side).
    pub layout: String,
}

/// Full inspection result for one input file.
#[derive(Clone, Debug)]
pub struct MediaFile {
    pub path: PathBuf,
    pub bytes: u64,
    // video (primary video stream)
    pub vcodec: String,
    pub width: u32,
    pub height: u32,
    /// frames per second (0.0 when unknown).
    pub fps: f64,
    pub pix_fmt: String,
    /// 8 or 10 (defaults to 8 when unknown).
    pub bit_depth: u8,
    /// Color tags from ffprobe ("" when unknown): color_space like
    /// "bt709"/"bt2020nc", primaries like "bt709"/"bt2020", transfer like
    /// "bt709"/"smpte2084"/"arib-std-b67", range "tv"/"pc".
    pub color_space: String,
    pub color_primaries: String,
    pub color_transfer: String,
    pub color_range: String,
    /// video stream bitrate in bps, if reported.
    pub vbitrate: Option<u64>,
    /// container bitrate in bps, if reported.
    pub format_bitrate: Option<u64>,
    pub duration_s: f64,
    // audio
    pub audio: Vec<AudioTrack>,
    /// primary audio codec ("none" when the file has no audio).
    pub acodec: String,
    // other streams
    pub video_stream_count: usize,
    pub audio_stream_count: usize,
    pub sub_count: usize,
    pub attach_count: usize,
    pub has_chapters: bool,
}

impl MediaFile {
    /// Short `1920x1080`-style label (falls back to `?` when unknown).
    pub fn res_label(&self) -> String {
        if self.width > 0 && self.height > 0 {
            format!("{}x{}", self.width, self.height)
        } else {
            "?".to_string()
        }
    }

    /// Estimated total frames (fps × duration, 24 fps fallback).
    pub fn total_frames(&self) -> f64 {
        let fps = if self.fps > 0.1 { self.fps } else { 24.0 };
        let dur = if self.duration_s > 0.0 {
            self.duration_s
        } else {
            22.0 * 60.0
        };
        (fps * dur).max(1.0)
    }

    pub fn primary_audio(&self) -> Option<&AudioTrack> {
        self.audio.first()
    }

    /// True for HDR sources (PQ/ST2084, HLG, or BT.2020 10-bit).
    /// The encoder path must re-assert color tags for these — global
    /// `-map_metadata -1` plus NVENC defaults otherwise wash them out.
    pub fn is_hdr(&self) -> bool {
        let t = self.color_transfer.to_lowercase();
        t == "smpte2084"
            || t == "smpte-st-2084"
            || t == "arib-std-b67"
            || (self.color_primaries.eq_ignore_ascii_case("bt2020") && self.bit_depth >= 10)
    }
}

// ── ffprobe JSON ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProbeJson {
    #[serde(default)]
    streams: Vec<StreamJson>,
    #[serde(default)]
    format: Option<FormatJson>,
    #[serde(default)]
    chapters: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct StreamJson {
    #[serde(default)]
    codec_name: Option<String>,
    #[serde(default)]
    codec_type: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    avg_frame_rate: Option<String>,
    #[serde(default)]
    r_frame_rate: Option<String>,
    #[serde(default)]
    pix_fmt: Option<String>,
    #[serde(default)]
    bits_per_raw_sample: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    color_space: Option<String>,
    #[serde(default)]
    color_transfer: Option<String>,
    #[serde(default)]
    color_primaries: Option<String>,
    #[serde(default)]
    color_range: Option<String>,
    #[serde(default)]
    bit_rate: Option<String>,
    #[serde(default)]
    channels: Option<u8>,
    #[serde(default)]
    channel_layout: Option<String>,
    #[serde(default)]
    sample_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FormatJson {
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    bit_rate: Option<String>,
}

fn parse_rate(s: &str) -> f64 {
    if let Some((n, d)) = s.split_once('/') {
        let n: f64 = n.parse().unwrap_or(0.0);
        let d: f64 = d.parse().unwrap_or(0.0);
        if d > 0.0 && n > 0.0 {
            return n / d;
        }
        return 0.0;
    }
    s.parse().unwrap_or(0.0)
}

fn parse_u64_opt(s: Option<&String>) -> Option<u64> {
    s.and_then(|v| v.parse::<u64>().ok())
}

fn bit_depth_of(s: &StreamJson) -> u8 {
    if let Some(raw) = s.bits_per_raw_sample.as_deref() {
        if let Ok(b) = raw.parse::<u8>() {
            if b >= 8 {
                return b.min(16);
            }
        }
    }
    let pix = s.pix_fmt.as_deref().unwrap_or("");
    let profile = s.profile.as_deref().unwrap_or("").to_lowercase();
    if pix.contains("10")
        || pix.contains("p010")
        || profile.contains("main 10")
        || profile.contains("main10")
    {
        return 10;
    }
    if pix.contains("12") || pix.contains("16") {
        return 10; // treat >8 as 10 for encoder-profile purposes
    }
    8
}

/// Probe one file with `ffprobe -show_streams -show_format -of json`.
pub fn probe_file(path: &Path) -> Result<MediaFile> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let out = crate::process::cmd("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .context("ffprobe failed — is ffmpeg installed and on PATH?")?;
    if !out.status.success() {
        anyhow::bail!("ffprobe exited with {}", out.status);
    }
    let pj: ProbeJson = serde_json::from_slice(&out.stdout).context("ffprobe json parse")?;

    let mut vcodec = "unknown".to_string();
    let mut w = 0u32;
    let mut h = 0u32;
    let mut fps = 0.0f64;
    let mut pix_fmt = String::new();
    let mut bit_depth = 8u8;
    let mut color_space = String::new();
    let mut color_primaries = String::new();
    let mut color_transfer = String::new();
    let mut color_range = String::new();
    let mut vbitrate: Option<u64> = None;
    let mut video_n = 0usize;
    let mut audio: Vec<AudioTrack> = vec![];
    let mut sub_n = 0usize;
    let mut attach_n = 0usize;
    let mut first_video_seen = false;

    for s in &pj.streams {
        match s.codec_type.as_deref() {
            Some("video") => {
                // Attachments (fonts/images) show up as video streams with
                // codec_name like ttf/otf or disposition attached_pic.
                // ffprobe json here has no disposition field; heuristic:
                let cn = s.codec_name.clone().unwrap_or_default().to_lowercase();
                if matches!(
                    cn.as_str(),
                    "ttf" | "otf" | "woff" | "mjpeg" | "png" | "bmp"
                ) && s.width.is_none()
                {
                    attach_n += 1;
                    continue;
                }
                video_n += 1;
                if !first_video_seen {
                    first_video_seen = true;
                    vcodec = s.codec_name.clone().unwrap_or_else(|| "unknown".into());
                    w = s.width.unwrap_or(0);
                    h = s.height.unwrap_or(0);
                    // prefer avg_frame_rate, fall back to r_frame_rate
                    let a = s.avg_frame_rate.as_deref().unwrap_or("0/0");
                    let r = s.r_frame_rate.as_deref().unwrap_or("0/0");
                    fps = parse_rate(a);
                    if !(fps > 0.1 && fps < 240.0) {
                        fps = parse_rate(r);
                        if !(fps > 0.1 && fps < 240.0) {
                            fps = 0.0;
                        }
                    }
                    pix_fmt = s.pix_fmt.clone().unwrap_or_default();
                    bit_depth = bit_depth_of(s);
                    color_space = s.color_space.clone().unwrap_or_default();
                    color_primaries = s.color_primaries.clone().unwrap_or_default();
                    color_transfer = s.color_transfer.clone().unwrap_or_default();
                    color_range = s.color_range.clone().unwrap_or_default();
                    vbitrate = parse_u64_opt(s.bit_rate.as_ref());
                }
            }
            Some("audio") => {
                audio.push(AudioTrack {
                    codec: s.codec_name.clone().unwrap_or_else(|| "unknown".into()),
                    bitrate: parse_u64_opt(s.bit_rate.as_ref()),
                    channels: s.channels.unwrap_or(0),
                    layout: s.channel_layout.clone().unwrap_or_default(),
                    sample_rate: s.sample_rate.as_deref().and_then(|v| v.parse::<u32>().ok()),
                });
            }
            Some("subtitle") => sub_n += 1,
            Some("attachment") => attach_n += 1,
            _ => {}
        }
    }

    let acodec = audio
        .first()
        .map(|a| a.codec.clone())
        .unwrap_or_else(|| "none".into());
    let audio_n = audio.len();
    let dur = pj
        .format
        .as_ref()
        .and_then(|f| f.duration.as_ref())
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(0.0);
    let format_bitrate = pj
        .format
        .as_ref()
        .and_then(|f| parse_u64_opt(f.bit_rate.as_ref()));
    let has_chapters = pj.chapters.map(|c| !c.is_empty()).unwrap_or(false);

    Ok(MediaFile {
        path: path.to_path_buf(),
        bytes: meta.len(),
        vcodec,
        width: w,
        height: h,
        fps,
        pix_fmt,
        bit_depth,
        color_space,
        color_primaries,
        color_transfer,
        color_range,
        vbitrate,
        format_bitrate,
        duration_s: dur,
        acodec,
        audio,
        video_stream_count: video_n,
        audio_stream_count: audio_n,
        sub_count: sub_n,
        attach_count: attach_n,
        has_chapters,
    })
}

pub fn human_bytes(b: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let f = b as f64;
    if f >= GB {
        format!("{:.2} GB", f / GB)
    } else if f >= MB {
        format!("{:.1} MB", f / MB)
    } else {
        format!("{:.0} KB", f / 1024.0)
    }
}

pub const MEDIA_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "m4v", "ts", "m2ts", "mpg", "mpeg", "webm",
];

/// Worker count for parallel probing. ffprobe is latency-bound
/// (~200–500 ms of process spawn + demux per file), so a handful of
/// workers hides it almost linearly; clamp to stay out of the way.
pub fn probe_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8)
}
