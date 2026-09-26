//! File conversion: same-kind file-type transcoding + video→audio extraction.
//!
//! Shrink optimises *size* within one output container (MKV). Convert
//! changes the *file type*:
//!
//! * Video → `mp4 | mkv | webm | mov | avi` (ffmpeg)
//! * Video → audio `mp3 | m4a | opus | ogg | flac | wav` (audio extraction)
//! * Audio → `mp3 | m4a | opus | ogg | flac | wav` (ffmpeg)
//! * Image → `jpg | png | webp | gif` (ffmpeg)
//! * Subtitle → `srt | vtt | ass` (ffmpeg)
//! * Document → `pdf | docx | odt | txt | html` (word),
//!   `pdf | xlsx | ods | csv` (spreadsheet),
//!   `pdf | pptx | odp` (presentation) — via LibreOffice headless
//!
//! Strategy per file is copy-first: when the source codecs are legal in
//! the target container we remux (`-c copy` — seconds, ~same bytes).
//! Video→audio extraction maps only the primary audio stream (`-map
//! 0:a:0`): same-codec extractions remux, everything else re-encodes
//! once with the audio defaults below. Documents
//! go through `soffice --headless --convert-to`. A target is only
//! *offered* when this machine can actually write it (ffmpeg muxer +
//! encoder present, or LibreOffice installed for documents) and it
//! differs from the source extension — impossible conversions never
//! appear as options.

use crate::media::{human_bytes, MediaFile};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Media kind: conversion stays within a kind, except video which can
/// additionally be demuxed to audio (video → mp3 and friends).
/// A spreadsheet never becomes a presentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
    Subtitle,
    Document,
}

impl MediaKind {
    pub fn label(&self) -> &'static str {
        match self {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
            MediaKind::Image => "image",
            MediaKind::Subtitle => "subtitle",
            MediaKind::Document => "document",
        }
    }
}

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "m4v", "ts", "m2ts", "mts", "mpg", "mpeg", "webm",
    "ogv", "flv", "f4v", "3gp", "3g2", "vob", "asf", "rm",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "aac", "opus", "ogg", "oga", "flac", "wav", "wma", "aiff", "aif", "mka", "ac3",
    "dts",
];
pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff", "gif", "ico", "avif",
];
pub const SUBTITLE_EXTS: &[&str] = &["srt", "vtt", "ass", "ssa", "sub", "lrc"];
/// Office / text documents convertible via LibreOffice headless.
pub const DOCUMENT_EXTS: &[&str] = &[
    "doc", "docx", "odt", "rtf", "txt", "md", "html", "htm", "pdf", "xls", "xlsx", "ods", "csv",
    "tsv", "ppt", "pptx", "odp",
];

/// Every extension the Convert tab accepts (drop filter + dir walk).
pub const CONVERT_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "m4v", "ts", "m2ts", "mts", "mpg", "mpeg", "webm", "ogv",
    "flv", "f4v", "3gp", "3g2", "vob", "asf", "rm", "mp3", "m4a", "aac", "opus", "ogg", "oga",
    "flac", "wav", "wma", "aiff", "aif", "mka", "ac3", "dts", "jpg", "jpeg", "png", "webp", "bmp",
    "tif", "tiff", "gif", "ico", "avif", "srt", "vtt", "ass", "ssa", "sub", "lrc", "doc", "docx",
    "odt", "rtf", "txt", "md", "html", "htm", "pdf", "xls", "xlsx", "ods", "csv", "tsv", "ppt",
    "pptx", "odp",
];

/// Document sub-family: a spreadsheet converts to spreadsheet formats,
/// never to a presentation. PDF reads as a word document (LibreOffice
/// Draw import) and is a universal *target*.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocFamily {
    Word,
    Calc,
    Impress,
}

pub fn doc_family(ext: &str) -> Option<DocFamily> {
    match ext.to_lowercase().as_str() {
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "html" | "htm" | "pdf" => {
            Some(DocFamily::Word)
        }
        "xls" | "xlsx" | "ods" | "csv" | "tsv" => Some(DocFamily::Calc),
        "ppt" | "pptx" | "odp" => Some(DocFamily::Impress),
        _ => None,
    }
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Kind from the file extension, refined by probe streams when known.
/// Image extensions always win (a `.jpg` probes as one `mjpeg` frame,
/// which would otherwise look like video).
pub fn kind_for(path: &Path, media: Option<&MediaFile>) -> Option<MediaKind> {
    let ext = ext_of(path);
    if DOCUMENT_EXTS.contains(&ext.as_str()) {
        return Some(MediaKind::Document);
    }
    if SUBTITLE_EXTS.contains(&ext.as_str()) {
        return Some(MediaKind::Subtitle);
    }
    if IMAGE_EXTS.contains(&ext.as_str()) {
        return Some(MediaKind::Image);
    }
    if AUDIO_EXTS.contains(&ext.as_str()) {
        // `mp4`/`webm` audio lives in video extensions; pure audio exts
        // with a video stream (cover art) still count as audio.
        if let Some(m) = media {
            if m.video_stream_count == 0 || (m.vcodec == "unknown" && m.width == 0) {
                return Some(MediaKind::Audio);
            }
            // Cover-art video + audio tracks: treat as audio.
            if m.audio_stream_count > 0 && is_cover_art_video(m) {
                return Some(MediaKind::Audio);
            }
        }
        return Some(MediaKind::Audio);
    }
    if VIDEO_EXTS.contains(&ext.as_str()) {
        return Some(MediaKind::Video);
    }
    // Unknown extension: fall back to streams.
    match media {
        Some(m) if m.video_stream_count > 0 && m.vcodec != "unknown" => Some(MediaKind::Video),
        Some(m) if m.audio_stream_count > 0 => Some(MediaKind::Audio),
        _ => None,
    }
}

/// Heuristic for audio files with embedded cover art: tiny/odd video
/// stream next to real audio. Only used to keep `.mp3`+cover in Audio.
fn is_cover_art_video(m: &MediaFile) -> bool {
    m.video_stream_count > 0
        && (m.width < 16
            || m.height < 16
            || matches!(m.vcodec.as_str(), "mjpeg" | "png" | "bmp"))
}

/// One offered output type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvertTarget {
    /// File extension without dot (`"mp4"`).
    pub ext: &'static str,
    /// Human label (`"MP4 (H.264 + AAC)"`).
    pub label: &'static str,
    /// ffmpeg muxer that must be present.
    pub muxer: &'static str,
    /// Encoder that must be present for the re-encode fallback
    /// (empty = stream-copy only, e.g. never for our tables).
    pub encoder: &'static str,
}

pub fn targets_for(kind: MediaKind) -> Vec<ConvertTarget> {
    match kind {
        MediaKind::Video => vec![
            ConvertTarget { ext: "mp4", label: "MP4 (H.264 + AAC)", muxer: "mp4", encoder: "libx264" },
            ConvertTarget { ext: "mkv", label: "MKV (H.264 + Opus)", muxer: "matroska", encoder: "libx264" },
            ConvertTarget { ext: "webm", label: "WebM (VP9 + Opus)", muxer: "webm", encoder: "libvpx-vp9" },
            ConvertTarget { ext: "mov", label: "MOV (H.264 + AAC)", muxer: "mov", encoder: "libx264" },
            ConvertTarget { ext: "avi", label: "AVI (H.264 + MP3)", muxer: "avi", encoder: "libx264" },
        ],
        MediaKind::Audio => audio_targets(),
        MediaKind::Image => vec![
            ConvertTarget { ext: "jpg", label: "JPEG q2", muxer: "image2", encoder: "mjpeg" },
            ConvertTarget { ext: "png", label: "PNG", muxer: "image2", encoder: "png" },
            ConvertTarget { ext: "webp", label: "WebP q85", muxer: "image2", encoder: "libwebp" },
            ConvertTarget { ext: "gif", label: "GIF", muxer: "gif", encoder: "gif" },
        ],
        MediaKind::Subtitle => vec![
            ConvertTarget { ext: "srt", label: "SubRip (.srt)", muxer: "srt", encoder: "srt" },
            ConvertTarget { ext: "vtt", label: "WebVTT (.vtt)", muxer: "webvtt", encoder: "webvtt" },
            ConvertTarget { ext: "ass", label: "ASS (.ass)", muxer: "ass", encoder: "ass" },
        ],
        // Union of all families (probed files are narrowed to their
        // family by `feasible_targets`; this is the unprobed fallback).
        MediaKind::Document => all_doc_targets(),
    }
}

/// Shared audio output table: used for audio→audio conversion and for
/// video→audio extraction (same codecs, bitrates and capability gates).
pub fn audio_targets() -> Vec<ConvertTarget> {
    vec![
        ConvertTarget { ext: "mp3", label: "MP3 192k", muxer: "mp3", encoder: "libmp3lame" },
        ConvertTarget { ext: "m4a", label: "M4A/AAC 128k", muxer: "mp4", encoder: "aac" },
        ConvertTarget { ext: "opus", label: "Opus 96k", muxer: "opus", encoder: "libopus" },
        ConvertTarget { ext: "ogg", label: "Ogg Vorbis q5", muxer: "ogg", encoder: "libvorbis" },
        ConvertTarget { ext: "flac", label: "FLAC (lossless)", muxer: "flac", encoder: "flac" },
        ConvertTarget { ext: "wav", label: "WAV (PCM)", muxer: "wav", encoder: "pcm_s16le" },
    ]
}

/// True when this target demuxes a video file down to an audio-only
/// output (video → mp3 and friends). Extraction maps just the primary
/// audio stream; video/subs/attachments are dropped.
pub fn is_audio_extract(kind: MediaKind, target_ext: &str) -> bool {
    kind == MediaKind::Video
        && AUDIO_EXTS.contains(&target_ext.to_lowercase().as_str())
}

/// Every document target across families (deduplicated by extension).
fn all_doc_targets() -> Vec<ConvertTarget> {
    let mut out: Vec<ConvertTarget> = vec![];
    for t in doc_targets(DocFamily::Word)
        .into_iter()
        .chain(doc_targets(DocFamily::Calc))
        .chain(doc_targets(DocFamily::Impress))
    {
        if !out.iter().any(|x| x.ext == t.ext) {
            out.push(t);
        }
    }
    out
}

/// Document targets for one family. `muxer`/`encoder` carry the
/// [`OFFICE_ENGINE`] token: offered only when LibreOffice is installed.
pub fn doc_targets(family: DocFamily) -> Vec<ConvertTarget> {
    const O: &str = OFFICE_ENGINE;
    match family {
        DocFamily::Word => vec![
            ConvertTarget { ext: "pdf", label: "PDF", muxer: O, encoder: O },
            ConvertTarget { ext: "docx", label: "Word (.docx)", muxer: O, encoder: O },
            ConvertTarget { ext: "odt", label: "ODT", muxer: O, encoder: O },
            ConvertTarget { ext: "txt", label: "Plain text", muxer: O, encoder: O },
            ConvertTarget { ext: "html", label: "HTML", muxer: O, encoder: O },
        ],
        DocFamily::Calc => vec![
            ConvertTarget { ext: "pdf", label: "PDF", muxer: O, encoder: O },
            ConvertTarget { ext: "xlsx", label: "Excel (.xlsx)", muxer: O, encoder: O },
            ConvertTarget { ext: "ods", label: "ODS", muxer: O, encoder: O },
            ConvertTarget { ext: "csv", label: "CSV", muxer: O, encoder: O },
        ],
        DocFamily::Impress => vec![
            ConvertTarget { ext: "pdf", label: "PDF", muxer: O, encoder: O },
            ConvertTarget { ext: "pptx", label: "PowerPoint (.pptx)", muxer: O, encoder: O },
            ConvertTarget { ext: "odp", label: "ODP", muxer: O, encoder: O },
        ],
    }
}

/// LibreOffice `--convert-to` filter per target extension.
pub fn office_filter(target_ext: &str) -> &'static str {
    match target_ext {
        "pdf" => "pdf",
        "docx" => "docx",
        "odt" => "odt",
        "txt" => "txt",
        "html" => "html",
        "xlsx" => "xlsx",
        "ods" => "ods",
        "csv" => "csv",
        "pptx" => "pptx",
        "odp" => "odp",
        _ => "pdf",
    }
}

/// Engine token used in [`ConvertTarget`] for LibreOffice-backed outputs.
pub const OFFICE_ENGINE: &str = "libreoffice";

// ── Capability detection ─────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct ConvertCaps {
    pub muxers: Vec<String>,
    pub encoders: Vec<String>,
    /// LibreOffice available for document conversion.
    pub office: bool,
    /// Resolved `soffice` command (PATH or well-known install dir).
    pub office_cmd: Option<String>,
}

impl ConvertCaps {
    pub fn has_muxer(&self, m: &str) -> bool {
        if m == OFFICE_ENGINE {
            return self.office;
        }
        self.muxers.iter().any(|x| x == m)
    }
    pub fn has_encoder(&self, e: &str) -> bool {
        if e == OFFICE_ENGINE {
            return self.office;
        }
        // Native PCM encoders are built-in but sometimes absent from
        // `-encoders` on minimal builds; treat pcm_* as always present
        // when ffmpeg itself runs.
        if e.starts_with("pcm_") {
            return true;
        }
        self.encoders.iter().any(|x| x == e)
    }
    pub fn has_office(&self) -> bool {
        self.office
    }
    /// Short badge for the Convert header, e.g. `mp4✓ webm✓ docs✗`.
    pub fn summary(&self) -> String {
        let t = |ok: bool| if ok { "✓" } else { "✗" };
        format!(
            "convert: mp4{} mkv{} webm{} mp3{} opus{} flac{} webp{} subs{} docs{}",
            t(self.has_muxer("mp4")),
            t(self.has_muxer("matroska")),
            t(self.has_muxer("webm")),
            t(self.has_encoder("libmp3lame")),
            t(self.has_encoder("libopus")),
            t(self.has_encoder("flac")),
            t(self.has_encoder("libwebp")),
            t(self.has_muxer("srt")),
            t(self.office),
        )
    }
}

static CAPS: OnceLock<ConvertCaps> = OnceLock::new();

/// Cached detection (runs once per process).
pub fn caps() -> &'static ConvertCaps {
    CAPS.get_or_init(detect)
}

pub fn detect() -> ConvertCaps {
    let muxers = cmd_names("ffmpeg", &["-hide_banner", "-muxers"]);
    let encoders = cmd_names("ffmpeg", &["-hide_banner", "-encoders"]);
    let office_cmd = find_office();
    ConvertCaps {
        muxers,
        encoders,
        office: office_cmd.is_some(),
        office_cmd,
    }
}

/// Locate LibreOffice: `SHRINKR_SOFFICE` override first (custom
/// installs, tests), then `soffice` on PATH, else the well-known Windows
/// install dirs (soffice is rarely on PATH there).
fn find_office() -> Option<String> {
    if let Ok(p) = std::env::var("SHRINKR_SOFFICE") {
        if !p.trim().is_empty() {
            return Some(p);
        }
    }
    if crate::process::cmd("soffice")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Some("soffice".into());
    }
    for dir in [
        std::env::var("PROGRAMFILES").ok(),
        std::env::var("PROGRAMFILES(X86)").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let p = PathBuf::from(dir).join("LibreOffice").join("program").join("soffice.exe");
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

/// First whitespace-separated token per line, lowercased
/// (`" E  mp4   MP4 …"` → `"mp4"`).
fn cmd_names(prog: &str, args: &[&str]) -> Vec<String> {
    let out = crate::process::cmd(prog)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let tok = l.split_whitespace().nth(1)?;
            let t = tok.to_lowercase();
            if t.starts_with('-') || t == "----" {
                None
            } else {
                Some(t)
            }
        })
        .collect()
}

/// Offered targets for one file: same-kind list minus the source
/// extension (converting `mp4 → mp4` is a no-op), minus anything this
/// ffmpeg build cannot write. Video files additionally offer audio
/// extraction (`mp3 | m4a | opus | ogg | flac | wav`): those entries map
/// only the audio stream. Empty = "cannot convert this file".
pub fn feasible_targets(
    kind: MediaKind,
    src_ext: &str,
    caps: &ConvertCaps,
) -> Vec<ConvertTarget> {
    let src = src_ext.to_lowercase();
    let norm_src = if src == "jpeg" { "jpg".into() } else { src };
    // Documents narrow to their sub-family (a spreadsheet never offers
    // presentation outputs); video adds audio extraction; everything
    // else uses the kind table.
    let pool = if kind == MediaKind::Document {
        match doc_family(&norm_src) {
            Some(f) => doc_targets(f),
            None => vec![],
        }
    } else if kind == MediaKind::Video {
        targets_for(MediaKind::Video)
            .into_iter()
            .chain(audio_targets())
            .collect()
    } else {
        targets_for(kind)
    };
    pool
        .into_iter()
        .filter(|t| t.ext != norm_src)
        .filter(|t| caps.has_muxer(t.muxer) && caps.has_encoder(t.encoder))
        .collect()
}

// ── Copy compatibility ───────────────────────────────────────

/// True when source codecs are legal in the target container, i.e. a
/// `-c copy` remux should succeed (fast path, ~same bytes).
/// Video→audio extraction remuxes when the primary audio codec already
/// matches the target (e.g. mp4/aac → m4a); the video stream is dropped.
pub fn copy_compatible(m: &MediaFile, kind: MediaKind, target_ext: &str) -> bool {
    if is_audio_extract(kind, target_ext) {
        return audio_remux_compatible(m, target_ext);
    }
    match kind {
        MediaKind::Video => {
            let v = m.vcodec.as_str();
            let a = m.acodec.as_str();
            let audio_ok = |allowed: &[&str]| a == "none" || allowed.contains(&a);
            match target_ext {
                "mp4" | "mov" | "m4a" => {
                    matches!(v, "h264" | "hevc" | "av1" | "mpeg4")
                        && audio_ok(&["aac", "mp3", "ac3", "opus", "none"])
                }
                "mkv" => {
                    matches!(
                        v,
                        "h264" | "hevc" | "av1" | "vp9" | "mpeg4" | "mpeg2video" | "vc1" | "mjpeg"
                    ) && audio_ok(&[
                        "aac", "mp3", "opus", "vorbis", "flac", "ac3", "dts", "pcm_s16le", "none",
                    ])
                }
                "webm" => matches!(v, "vp8" | "vp9" | "av1") && audio_ok(&["opus", "vorbis", "none"]),
                "avi" => {
                    matches!(v, "h264" | "mpeg4" | "mjpeg" | "msmpeg4" | "xvid")
                        && audio_ok(&["mp3", "ac3", "pcm_s16le", "none"])
                }
                _ => false,
            }
        }
        MediaKind::Audio => audio_remux_compatible(m, target_ext),
        // Different image extensions always re-encode (tiny anyway).
        MediaKind::Image => false,
        MediaKind::Subtitle => {
            // Same-codec remuxes (only same-extension, excluded upstream
            // — subtitle conversion always re-encodes the text stream).
            let src = m
                .path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            match target_ext {
                "srt" => src == "srt",
                "vtt" => src == "vtt",
                "ass" => src == "ass" || src == "ssa",
                _ => false,
            }
        }
        // LibreOffice always rewrites the file.
        MediaKind::Document => false,
    }
}

/// Same-codec audio remux check shared by audio→audio conversion and
/// video→audio extraction (different containers count as conversion).
/// Needs a real audio stream — silent videos never remux to audio.
fn audio_remux_compatible(m: &MediaFile, target_ext: &str) -> bool {
    if m.audio_stream_count == 0 {
        return false;
    }
    let a = m.primary_audio().map(|t| t.codec.as_str()).unwrap_or("none");
    match target_ext.to_lowercase().as_str() {
        "m4a" => matches!(a, "aac" | "alac"),
        "opus" => a == "opus",
        "ogg" => a == "vorbis",
        "mp3" => a == "mp3",
        "flac" => a == "flac",
        "wav" => a.starts_with("pcm"),
        _ => false,
    }
}

// ── Size estimate ────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConvertMode {
    /// Stream-copy remux: same bytes, container overhead only.
    Copy,
    /// Re-encode with the target defaults.
    Reencode,
}

#[derive(Clone, Debug)]
pub struct ConvertEstimate {
    pub new_bytes: u64,
    pub ratio: f64,
    pub mode: ConvertMode,
    /// Short note for the row, e.g. `"remux"`, `"re-encode"`, `"wav is huge"`.
    pub note: &'static str,
}

/// Rough output-size estimate so the row can show `→ 12.1 MB (−8%)`.
/// Copy paths are exact-ish (±2%); re-encodes are honest approximations
/// (high-quality defaults, not shrink settings) — always labelled `~`.
/// Video→audio extraction estimates from the audio bitrate × duration
/// (audio is typically a few % of a video file).
pub fn estimate(m: &MediaFile, kind: MediaKind, target_ext: &str) -> ConvertEstimate {
    let extract = is_audio_extract(kind, target_ext);
    if copy_compatible(m, kind, target_ext) {
        return ConvertEstimate {
            new_bytes: m.bytes,
            ratio: 1.0,
            mode: ConvertMode::Copy,
            note: if extract { "extract (remux)" } else { "remux" },
        };
    }
    if extract {
        return estimate_audio_bytes(m, target_ext, extract_note(target_ext));
    }
    match kind {
        MediaKind::Video => {
            // High-quality re-encode of an already-compressed source:
            // near the source size (this is conversion, not shrinking).
            let factor = match target_ext {
                "webm" => 0.85,
                _ => 0.95,
            };
            let new_bytes = (m.bytes as f64 * factor) as u64;
            ConvertEstimate {
                new_bytes,
                ratio: factor,
                mode: ConvertMode::Reencode,
                note: "re-encode",
            }
        }
        MediaKind::Audio => estimate_audio_bytes(m, target_ext, reencode_note(target_ext)),
        MediaKind::Image => {
            let factor = match target_ext {
                "jpg" => 0.55,
                "webp" => 0.5,
                "png" => 1.4,
                _ => 1.0,
            };
            ConvertEstimate {
                new_bytes: (m.bytes as f64 * factor) as u64,
                ratio: factor,
                mode: ConvertMode::Reencode,
                note: "re-encode",
            }
        }
        // Text subtitle streams rewrite to ~the same bytes.
        MediaKind::Subtitle => ConvertEstimate {
            new_bytes: m.bytes,
            ratio: 1.0,
            mode: ConvertMode::Reencode,
            note: "convert",
        },
        // Office conversions preserve content; container sizes land near
        // the source (PDF export can swing either way — labelled ~).
        MediaKind::Document => ConvertEstimate {
            new_bytes: m.bytes,
            ratio: 1.0,
            mode: ConvertMode::Reencode,
            note: "convert",
        },
    }
}

/// Estimate without a probe (documents ffprobe cannot read): same
/// content, same order of magnitude — always labelled `~`.
/// Video→audio extraction without a duration still re-encodes, so it
/// never reports a misleading copy.
pub fn estimate_for_bytes(bytes: u64, kind: MediaKind, target_ext: &str) -> ConvertEstimate {
    if is_audio_extract(kind, target_ext) {
        return ConvertEstimate {
            new_bytes: bytes,
            ratio: 1.0,
            mode: ConvertMode::Reencode,
            note: "extract audio",
        };
    }
    ConvertEstimate {
        new_bytes: bytes,
        ratio: 1.0,
        mode: if kind == MediaKind::Document {
            ConvertMode::Reencode
        } else {
            ConvertMode::Copy
        },
        note: "convert",
    }
}

/// Shared audio-size estimate (audio→audio + video→audio extraction):
/// `bitrate × duration`, falling back to codec-character scaling when
/// the duration is unknown.
fn estimate_audio_bytes(m: &MediaFile, target_ext: &str, note: &'static str) -> ConvertEstimate {
    let dur = m.duration_s;
    let target_bps = audio_target_bps(target_ext);
    match (target_bps, dur) {
        (Some(bps), d) if d > 0.0 => {
            // +1% container overhead.
            let new_bytes = (bps as f64 * d / 8.0 * 1.01) as u64;
            ConvertEstimate {
                new_bytes,
                ratio: new_bytes as f64 / m.bytes.max(1) as f64,
                mode: ConvertMode::Reencode,
                note,
            }
        }
        _ => {
            // Lossless/unknown duration: scale by codec character.
            let factor = match target_ext.to_lowercase().as_str() {
                "flac" => 0.65,
                "wav" => 8.0,
                _ => 0.8,
            };
            ConvertEstimate {
                new_bytes: (m.bytes as f64 * factor) as u64,
                ratio: factor,
                mode: ConvertMode::Reencode,
                note,
            }
        }
    }
}

/// Target audio bitrate in bps (`None` = lossless/variable: FLAC, WAV).
fn audio_target_bps(target_ext: &str) -> Option<u64> {
    match target_ext.to_lowercase().as_str() {
        "mp3" => Some(192_000),
        "m4a" => Some(128_000),
        "opus" => Some(96_000),
        "ogg" => Some(192_000),
        _ => None,
    }
}

fn reencode_note(target_ext: &str) -> &'static str {
    match target_ext {
        "wav" => "pcm is huge",
        "flac" => "lossless",
        _ => "re-encode",
    }
}

/// Estimate note for video→audio extraction (same audio character,
/// prefixed so rows read as extraction rather than plain re-encode).
fn extract_note(target_ext: &str) -> &'static str {
    match target_ext.to_lowercase().as_str() {
        "wav" => "extract audio · pcm is huge",
        "flac" => "extract audio · lossless",
        _ => "extract audio",
    }
}

/// One-line estimate label for the Convert table rows (probed files).
pub fn estimate_label(m: &MediaFile, kind: MediaKind, target_ext: &str) -> String {
    let e = estimate(m, kind, target_ext);
    let pct = (e.ratio - 1.0) * 100.0;
    let delta = if e.mode == ConvertMode::Copy {
        format!("{} · ~same", e.note)
    } else if pct >= 0.5 {
        format!("~{} (+{:.0}%)", human_bytes(e.new_bytes), pct)
    } else if pct <= -0.5 {
        format!("~{} (−{:.0}%)", human_bytes(e.new_bytes), -pct)
    } else {
        format!("~{} (~same)", human_bytes(e.new_bytes))
    };
    format!("{} → {} · {}", human_bytes(m.bytes), delta, e.note)
}

/// Estimate label without a probe (documents): `1.2 MB → ~same · convert`.
pub fn estimate_label_for_bytes(bytes: u64, kind: MediaKind, target_ext: &str) -> String {
    let e = estimate_for_bytes(bytes, kind, target_ext);
    let delta = if (e.ratio - 1.0).abs() < 0.005 {
        "~same".to_string()
    } else {
        format!("~{}", human_bytes(e.new_bytes))
    };
    format!("{} → {} · {}", human_bytes(bytes), delta, e.note)
}

// ── Command builders ─────────────────────────────────────────

/// Fast path: stream-copy remux. No quality flags — bytes pass through.
/// Audio extraction (`video → mp3`…) maps only the primary audio stream;
/// pass the target so the mapping is correct.
pub fn build_copy_args(
    input: &Path,
    kind: MediaKind,
    target_ext: &str,
    out_path: &Path,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-map".into(),
    ];
    match kind {
        // Subtitles: only the subtitle stream (never drag cover art along).
        MediaKind::Subtitle => a.push("0:s:0?".into()),
        // Audio-only outputs (audio→audio, video→audio extraction).
        MediaKind::Audio => a.push("0:a:0".into()),
        MediaKind::Video if is_audio_extract(kind, target_ext) => a.push("0:a:0".into()),
        _ => a.push("0".into()),
    }
    a.push("-c".into());
    a.push("copy".into());
    if kind == MediaKind::Image {
        a.push("-frames:v".into());
        a.push("1".into());
    }
    a.push(out_path.to_string_lossy().into_owned());
    a
}

/// Audio encoder flags shared by audio→audio and video→audio paths.
fn push_audio_encode_args(a: &mut Vec<String>, target_ext: &str) {
    match target_ext.to_lowercase().as_str() {
        "mp3" => {
            a.extend(["-c:a".into(), "libmp3lame".into(), "-b:a".into(), "192k".into()]);
        }
        "m4a" => {
            a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "128k".into()]);
        }
        "opus" => {
            a.extend(["-c:a".into(), "libopus".into(), "-b:a".into(), "96k".into()]);
        }
        "ogg" => {
            a.extend(["-c:a".into(), "libvorbis".into(), "-q:a".into(), "5".into()]);
        }
        "flac" => {
            a.extend(["-c:a".into(), "flac".into()]);
        }
        "wav" => {
            a.extend(["-c:a".into(), "pcm_s16le".into()]);
        }
        _ => {}
    }
}

/// Fallback: re-encode with transparent-ish defaults for the target.
pub fn build_reencode_args(
    m: &MediaFile,
    kind: MediaKind,
    target_ext: &str,
    out_path: &Path,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-i".into(),
        m.path.to_string_lossy().into_owned(),
    ];
    match kind {
        MediaKind::Video if is_audio_extract(kind, target_ext) => {
            // Video→audio extraction: drop video/subs, encode audio only
            // with the same defaults as audio→audio.
            a.push("-map".into());
            a.push("0:a:0".into());
            a.push("-map_metadata".into());
            a.push("-1".into());
            a.push("-dn".into());
            a.push("-vn".into());
            push_audio_encode_args(&mut a, target_ext);
        }
        MediaKind::Video => {
            a.push("-map".into());
            a.push("0:v:0".into());
            if m.audio_stream_count > 0 {
                a.push("-map".into());
                a.push("0:a:0".into());
            }
            if m.sub_count > 0 && matches!(target_ext, "mkv" | "mp4" | "mov" | "avi") {
                a.push("-map".into());
                a.push("0:s?".into());
                a.push("-c:s".into());
                a.push("copy".into());
            }
            a.push("-map_metadata".into());
            a.push("-1".into());
            a.push("-dn".into());
            if target_ext == "webm" {
                a.extend([
                    "-c:v".into(), "libvpx-vp9".into(),
                    "-crf".into(), "32".into(),
                    "-b:v".into(), "0".into(),
                    "-c:a".into(), "libopus".into(),
                    "-b:a".into(), "96k".into(),
                ]);
            } else {
                a.extend([
                    "-c:v".into(), "libx264".into(),
                    "-crf".into(), "20".into(),
                    "-preset".into(), "veryfast".into(),
                ]);
                let (acodec, abitrate) = match target_ext {
                    "avi" => ("libmp3lame", "192k"),
                    "mkv" | "webm" => ("libopus", "96k"),
                    _ => ("aac", "128k"),
                };
                a.push("-c:a".into());
                a.push(acodec.into());
                a.push("-b:a".into());
                a.push(abitrate.into());
            }
        }
        MediaKind::Audio => {
            a.push("-map".into());
            a.push("0:a:0".into());
            a.push("-map_metadata".into());
            a.push("-1".into());
            a.push("-dn".into());
            push_audio_encode_args(&mut a, target_ext);
        }
        MediaKind::Image => {
            a.push("-frames:v".into());
            a.push("1".into());
            match target_ext {
                "jpg" => {
                    a.extend(["-q:v".into(), "2".into()]);
                }
                "webp" => {
                    a.extend(["-quality".into(), "85".into()]);
                }
                _ => {}
            }
        }
        MediaKind::Subtitle => {
            a.push("-map".into());
            a.push("0:s:0".into());
            a.push("-c:s".into());
            a.push(
                match target_ext {
                    "vtt" => "webvtt",
                    "ass" => "ass",
                    _ => "srt",
                }
                .into(),
            );
        }
        // Documents never reach the ffmpeg builders (LibreOffice path).
        MediaKind::Document => {}
    }
    a.push(out_path.to_string_lossy().into_owned());
    a
}

// ── Runner ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ConvertResult {
    pub output_bytes: u64,
    pub elapsed_s: f64,
    pub copied: bool,
}

/// Convert one file: copy-first when the codecs fit, else one
/// re-encode. Video→audio targets extract the primary audio stream.
/// Progress reports `0.0–1.0` fractions (`-1.0` = unknown).
pub fn run_convert(
    m: &MediaFile,
    kind: MediaKind,
    target_ext: &str,
    out_path: &Path,
    progress: &dyn Fn(f64),
    cancel: &Arc<AtomicBool>,
) -> Result<ConvertResult, String> {
    if is_audio_extract(kind, target_ext) && m.audio_stream_count == 0 {
        return Err("no audio track to extract — this video is silent".into());
    }
    let copy_ok = copy_compatible(m, kind, target_ext);
    if copy_ok {
        let args = build_copy_args(&m.path, kind, target_ext, out_path);
        match run_once(&args, m.duration_s, progress, cancel) {
            Ok((bytes, elapsed)) => {
                verify_output(out_path, kind, target_ext)?;
                return Ok(ConvertResult { output_bytes: bytes, elapsed_s: elapsed, copied: true });
            }
            Err(e) => {
                let _ = std::fs::remove_file(out_path);
                if cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                crate::ffmpeg::log::debug_log(&format!(
                    "convert copy → {target_ext} failed ({e}); trying re-encode"
                ));
            }
        }
    }
    let args = build_reencode_args(m, kind, target_ext, out_path);
    let (bytes, elapsed) = run_once(&args, m.duration_s, progress, cancel)?;
    verify_output(out_path, kind, target_ext)?;
    Ok(ConvertResult { output_bytes: bytes, elapsed_s: elapsed, copied: false })
}

// ── LibreOffice document runner ──────────────────────────────

static OFFICE_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

/// Convert an office document via `soffice --headless --convert-to`.
/// Needs no probe (ffprobe cannot read office files): `src_bytes` feeds
/// the result metrics. LibreOffice is single-instance per profile, so
/// conversions serialize on a process-wide lock.
pub fn run_document_convert(
    src: &Path,
    target_ext: &str,
    out_path: &Path,
    progress: &dyn Fn(f64),
    cancel: &Arc<AtomicBool>,
) -> Result<ConvertResult, String> {
    let cmd = caps()
        .office_cmd
        .clone()
        .filter(|_| caps().office)
        .ok_or_else(|| {
            "LibreOffice not found — install it (libreoffice.org) and restart for Word/Excel/PowerPoint/PDF conversion".to_string()
        })?;
    let work = std::env::temp_dir().join(format!(
        "shrinkr-doc-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&work).map_err(|e| format!("tmp dir: {e}"))?;
    let t0 = std::time::Instant::now();
    progress(0.0);
    let _guard = OFFICE_LOCK.get_or_init(|| std::sync::Mutex::new(())).lock();
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let mut child = crate::process::cmd(&cmd)
        .args([
            "--headless",
            "--convert-to",
            office_filter(target_ext),
            "--outdir",
            &work.to_string_lossy(),
            &src.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn libreoffice: {e}"))?;
    // Bound the run: poll so Cancel kills promptly (5 min per file).
    let deadline = t0 + std::time::Duration::from_secs(300);
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait libreoffice: {e}"))? {
            Some(st) => break st,
            None => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    return Err("cancelled".into());
                }
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err("libreoffice timed out (5 min) — file may be too complex".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    };
    if !status.success() {
        return Err(format!("libreoffice exited with {status}"));
    }
    // LibreOffice names the output `<stem>.<ext>` inside outdir.
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("converted")
        .to_lowercase();
    let mut produced: Option<PathBuf> = None;
    if let Ok(rd) = std::fs::read_dir(&work) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let same_stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.to_lowercase() == stem);
            let same_ext = p
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case(target_ext));
            if same_stem && same_ext {
                produced = Some(p);
                break;
            }
        }
    }
    let produced =
        produced.ok_or_else(|| "libreoffice produced no output — format may be unsupported".to_string())?;
    std::fs::rename(&produced, out_path)
        .or_else(|_| {
            std::fs::copy(&produced, out_path).map(|_| ()).and_then(|()| {
                std::fs::remove_file(&produced).map(|_| ()).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("tmp cleanup: {e}"),
                    )
                })
            })
        })
        .map_err(|e| format!("collect libreoffice output: {e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    verify_output(out_path, MediaKind::Document, target_ext)?;
    let bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    progress(1.0);
    Ok(ConvertResult {
        output_bytes: bytes,
        elapsed_s: t0.elapsed().as_secs_f64().max(0.01),
        copied: false,
    })
}

fn run_once(
    args: &[String],
    duration_s: f64,
    progress: &dyn Fn(f64),
    cancel: &Arc<AtomicBool>,
) -> Result<(u64, f64), String> {
    use std::io::{BufRead, BufReader};
    let t0 = std::time::Instant::now();
    let total_us = (duration_s.max(1.0) * 1_000_000.0) as i64;
    let mut child = crate::process::cmd("ffmpeg")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;
    let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
    let tail2 = stderr_tail.clone();
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            let r = BufReader::new(err);
            let mut tail: Vec<String> = vec![];
            for line in r.lines().filter_map(|l| l.ok()) {
                tail.push(line);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
            if let Ok(mut g) = tail2.lock() {
                *g = tail.join("\n");
            }
        });
    }
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().filter_map(|l| l.ok()) {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                return Err("cancelled".into());
            }
            let line = line.trim().to_string();
            if let Some(v) = line.strip_prefix("out_time_ms=") {
                if let Ok(us) = v.parse::<i64>() {
                    progress((us as f64 / total_us as f64).clamp(0.0, 1.0));
                }
            }
            if line == "progress=end" {
                break;
            }
        }
    }
    let st = child.wait().map_err(|e| format!("wait ffmpeg: {e}"))?;
    if !st.success() {
        let tail = stderr_tail.lock().map(|g| g.clone()).unwrap_or_default();
        let last = tail.lines().last().unwrap_or("ffmpeg failed").to_string();
        return Err(format!("ffmpeg failed: {}", last.chars().take(300).collect::<String>()));
    }
    let out_path = PathBuf::from(args.last().cloned().unwrap_or_default());
    let bytes = std::fs::metadata(&out_path).map(|x| x.len()).unwrap_or(0);
    // Subtitle conversions are legitimately tiny (a few cue lines).
    let floor = if out_path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| SUBTITLE_EXTS.contains(&e.to_lowercase().as_str()))
    {
        16
    } else {
        1024
    };
    if bytes < floor {
        return Err(format!("output too small ({bytes} bytes) — likely failed encode"));
    }
    Ok((bytes, t0.elapsed().as_secs_f64().max(0.01)))
}

fn verify_output(out_path: &Path, kind: MediaKind, target_ext: &str) -> Result<(), String> {
    // Office outputs aren't ffprobe-readable: size is the check.
    if kind == MediaKind::Document {
        let bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
        return if bytes > 512 {
            Ok(())
        } else {
            Err(format!("output too small ({bytes} bytes) — conversion failed"))
        };
    }
    // Video→audio extraction yields an audio-only file.
    if is_audio_extract(kind, target_ext) {
        let probed =
            crate::media::probe_file(out_path).map_err(|e| format!("verify probe: {e:#}"))?;
        return if probed.audio_stream_count > 0 {
            Ok(())
        } else {
            Err("output has no audio stream — extraction failed".into())
        };
    }
    let probed = crate::media::probe_file(out_path).map_err(|e| format!("verify probe: {e:#}"))?;
    let ok = match kind {
        MediaKind::Video => probed.video_stream_count > 0,
        MediaKind::Audio => probed.audio_stream_count > 0,
        MediaKind::Image => probed.video_stream_count > 0 || probed.bytes > 0,
        MediaKind::Subtitle => probed.sub_count > 0 || probed.bytes > 0,
        MediaKind::Document => true, // handled above
    };
    if ok {
        Ok(())
    } else {
        Err("output has no playable stream — conversion failed".into())
    }
}

/// Destination for a converted file.
/// * `replace=true`: `<stem>.<target-ext>` next to the original, original
///   deleted only after the new file is in place.
/// * `replace=false`: same, but never overwrites — collides resolve to
///   `<stem>-converted.<ext>`, `<stem>-converted-1.<ext>`, …
/// Returns `(bytes saved vs original, final dest)`.
pub fn place_converted_output(
    orig: &Path,
    tmp_out: &Path,
    target_ext: &str,
    replace: bool,
) -> Result<(i64, PathBuf), String> {
    let orig_len = std::fs::metadata(orig).map(|m| m.len()).unwrap_or(0) as i64;
    let new_len = std::fs::metadata(tmp_out).map(|m| m.len()).unwrap_or(0) as i64;
    let parent = orig.parent();
    let stem = orig
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("converted");
    let join = |name: String| match parent {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    };
    let mut dest = join(format!("{stem}.{target_ext}"));
    if dest.exists() {
        if replace && dest == orig.with_extension(target_ext) && orig.extension().and_then(|s| s.to_str()) == Some(target_ext) {
            // Same name only when extensions already match — excluded
            // upstream, but never clobber blindly.
        }
        let mut n = 0u32;
        while dest.exists() {
            let name = if n == 0 {
                format!("{stem}-converted.{target_ext}")
            } else {
                format!("{stem}-converted-{n}.{target_ext}")
            };
            dest = join(name);
            n += 1;
            if n > 200 {
                return Err("too many name collisions next to the original".into());
            }
        }
    }
    // Move into place (copy+delete fallback lives in shrink's mover —
    // reuse the same robust path via rename-then-copy here).
    let moved = std::fs::rename(tmp_out, &dest).is_ok() || {
        match std::fs::copy(tmp_out, &dest) {
            Ok(_) => std::fs::remove_file(tmp_out).is_ok(),
            Err(_) => false,
        }
    };
    if !moved {
        return Err(format!("move {} into place failed", dest.display()));
    }
    let saved = if replace {
        if orig != dest {
            match std::fs::remove_file(orig) {
                Ok(()) => (orig_len - new_len).max(0),
                Err(e) => {
                    return Err(format!(
                        "converted to {} but could not delete original {}: {e} (delete it by hand)",
                        dest.display(),
                        orig.display()
                    ));
                }
            }
        } else {
            (orig_len - new_len).max(0)
        }
    } else {
        0
    };
    Ok((saved, dest))
}

/// Collect convertible files under `arg` (file, dir, or a dir tree).
pub fn collect_inputs(arg: &Path) -> Vec<PathBuf> {
    if arg.is_file() {
        return vec![arg.to_path_buf()];
    }
    let mut hits = vec![];
    for e in walkdir::WalkDir::new(arg).into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        if let Some(ext) = e.path().extension().and_then(|s| s.to_str()) {
            if CONVERT_EXTS.contains(&ext.to_lowercase().as_str()) {
                hits.push(e.path().to_path_buf());
            }
        }
    }
    hits.sort();
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_all() -> ConvertCaps {
        ConvertCaps {
            muxers: vec![
                "mp4", "matroska", "webm", "mov", "avi", "mp3", "opus", "ogg", "flac", "wav",
                "image2", "oga", "gif", "srt", "webvtt", "ass",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            encoders: vec![
                "libx264", "libvpx-vp9", "libmp3lame", "aac", "libopus", "libvorbis", "flac",
                "mjpeg", "png", "libwebp", "gif", "srt", "webvtt", "ass",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            office: true,
            office_cmd: Some("soffice".into()),
        }
    }

    fn video_media(vcodec: &str, acodec: &str) -> MediaFile {
        MediaFile {
            path: PathBuf::from("m.mp4"),
            bytes: 1_000_000_000,
            vcodec: vcodec.into(),
            width: 1920,
            height: 1080,
            fps: 30.0,
            pix_fmt: "yuv420p".into(),
            bit_depth: 8,
            color_space: String::new(),
            color_primaries: String::new(),
            color_transfer: String::new(),
            color_range: String::new(),
            vbitrate: Some(8_000_000),
            format_bitrate: None,
            duration_s: 600.0,
            audio: vec![],
            acodec: acodec.into(),
            video_stream_count: 1,
            audio_stream_count: if acodec == "none" { 0 } else { 1 },
            sub_count: 0,
            attach_count: 0,
            has_chapters: false,
        }
    }

    #[test]
    fn kind_from_extension() {
        assert_eq!(
            kind_for(Path::new("a.mp4"), None),
            Some(MediaKind::Video)
        );
        assert_eq!(
            kind_for(Path::new("a.MP3"), None),
            Some(MediaKind::Audio)
        );
        assert_eq!(
            kind_for(Path::new("a.jpeg"), None),
            Some(MediaKind::Image)
        );
        assert_eq!(kind_for(Path::new("a.txt"), None), Some(MediaKind::Document));
        assert_eq!(kind_for(Path::new("a.xyz"), None), None);
    }

    #[test]
    fn infeasible_targets_hidden() {
        let caps = caps_all();
        // Same-extension excluded.
        let v: Vec<String> = feasible_targets(MediaKind::Video, "mp4", &caps)
            .into_iter()
            .map(|t| t.ext.to_string())
            .collect();
        assert!(!v.contains(&"mp4".to_string()));
        assert!(v.contains(&"mkv".to_string()));
        assert!(v.contains(&"webm".to_string()));
        // Video offers audio extraction with a full variety of types.
        for ext in ["mp3", "m4a", "opus", "ogg", "flac", "wav"] {
            assert!(v.contains(&ext.to_string()), "video should offer {ext}");
        }
        // Audio stays audio-only (never offers video containers).
        let a: Vec<String> = feasible_targets(MediaKind::Audio, "mp3", &caps)
            .into_iter()
            .map(|t| t.ext.to_string())
            .collect();
        assert!(!a.contains(&"mp4".to_string()));
        assert!(a.contains(&"opus".to_string()));
        // Missing encoder hides the target (no libwebp → no webp,
        // no libmp3lame → no mp3 extraction either).
        let mut no_webp = caps.clone();
        no_webp.encoders.retain(|e| e != "libwebp");
        let img = feasible_targets(MediaKind::Image, "png", &no_webp);
        assert!(!img.iter().any(|t| t.ext == "webp"));
        assert!(img.iter().any(|t| t.ext == "jpg"));
        let mut no_mp3 = caps.clone();
        no_mp3.encoders.retain(|e| e != "libmp3lame");
        let v2 = feasible_targets(MediaKind::Video, "mp4", &no_mp3);
        assert!(!v2.iter().any(|t| t.ext == "mp3"));
        assert!(v2.iter().any(|t| t.ext == "opus"));
    }

    #[test]
    fn copy_matrix() {
        // h264+aac fits everywhere except webm.
        let m = video_media("h264", "aac");
        assert!(copy_compatible(&m, MediaKind::Video, "mp4"));
        assert!(copy_compatible(&m, MediaKind::Video, "mkv"));
        assert!(!copy_compatible(&m, MediaKind::Video, "webm"));
        // vp9+opus fits webm and mkv, not mp4-avi.
        let w = video_media("vp9", "opus");
        assert!(copy_compatible(&w, MediaKind::Video, "webm"));
        assert!(copy_compatible(&w, MediaKind::Video, "mkv"));
        assert!(!copy_compatible(&w, MediaKind::Video, "avi"));
    }

    #[test]
    fn video_audio_extraction_matrix() {
        assert!(is_audio_extract(MediaKind::Video, "mp3"));
        assert!(is_audio_extract(MediaKind::Video, "wav"));
        assert!(!is_audio_extract(MediaKind::Video, "mp4"));
        assert!(!is_audio_extract(MediaKind::Audio, "mp3"));
        assert!(!is_audio_extract(MediaKind::Image, "mp3"));

        // Extraction remuxes on same-codec audio, re-encodes otherwise.
        let mut m = video_media("h264", "aac");
        m.audio = vec![crate::media::AudioTrack {
            codec: "aac".into(),
            bitrate: None,
            channels: 2,
            sample_rate: Some(48000),
            layout: "stereo".into(),
        }];
        m.audio_stream_count = 1;
        assert!(copy_compatible(&m, MediaKind::Video, "m4a"));
        assert!(!copy_compatible(&m, MediaKind::Video, "mp3"));

        let mut m2 = video_media("h264", "mp3");
        m2.audio = vec![crate::media::AudioTrack {
            codec: "mp3".into(),
            bitrate: None,
            channels: 2,
            sample_rate: Some(44100),
            layout: "stereo".into(),
        }];
        m2.audio_stream_count = 1;
        assert!(copy_compatible(&m2, MediaKind::Video, "mp3"));

        // Silent video never remuxes to audio.
        let silent = video_media("h264", "none");
        assert!(!copy_compatible(&silent, MediaKind::Video, "mp3"));
        assert!(!copy_compatible(&silent, MediaKind::Video, "m4a"));

        // Estimates shrink (audio is a fraction of the video file).
        let e = estimate(&m, MediaKind::Video, "mp3");
        assert_eq!(e.mode, ConvertMode::Reencode);
        assert!(e.new_bytes < m.bytes, "mp3 extract must shrink video");
        assert!(e.note.contains("extract audio"), "got {}", e.note);
        let e_copy = estimate(&m, MediaKind::Video, "m4a");
        assert_eq!(e_copy.mode, ConvertMode::Copy);
        assert!(e_copy.note.contains("extract"), "got {}", e_copy.note);

        // Command builders map audio-only for extraction.
        let out = Path::new("out.mp3");
        let copy_args = build_copy_args(Path::new("in.mp4"), MediaKind::Video, "mp3", out);
        let joined = copy_args.join(" ");
        assert!(joined.contains("0:a:0"), "{joined}");
        assert!(!joined.contains("-map 0 "), "{joined}");
        let re_args =
            build_reencode_args(&m, MediaKind::Video, "mp3", out);
        let rj = re_args.join(" ");
        assert!(rj.contains("libmp3lame"), "{rj}");
        assert!(rj.contains("-vn"), "{rj}");
        // Plain video re-encode still maps video.
        let rv =
            build_reencode_args(&m, MediaKind::Video, "mp4", Path::new("out.mp4"));
        assert!(rv.join(" ").contains("0:v:0"));
    }

    #[test]
    fn estimates_are_sane() {
        let m = video_media("h264", "aac");
        let e = estimate(&m, MediaKind::Video, "mkv");
        assert_eq!(e.mode, ConvertMode::Copy);
        assert_eq!(e.new_bytes, m.bytes);
        let e2 = estimate(&m, MediaKind::Video, "webm");
        assert_eq!(e2.mode, ConvertMode::Reencode);
        assert!(e2.new_bytes < m.bytes);
    }

    #[test]
    fn wav_estimate_warns_huge() {
        let mut m = video_media("unknown", "mp3");
        m.video_stream_count = 0;
        m.audio_stream_count = 1;
        m.bytes = 10_000_000;
        m.duration_s = 200.0;
        let e = estimate(&m, MediaKind::Audio, "wav");
        assert!(e.ratio > 5.0, "wav from mp3 must blow up, got {}", e.ratio);
        assert_eq!(e.note, "pcm is huge");
    }

    #[test]
    fn doc_families_gate_targets() {
        assert_eq!(doc_family("docx"), Some(DocFamily::Word));
        assert_eq!(doc_family("xlsx"), Some(DocFamily::Calc));
        assert_eq!(doc_family("pptx"), Some(DocFamily::Impress));
        // PDF reads as a word document.
        assert_eq!(doc_family("pdf"), Some(DocFamily::Word));
        assert_eq!(doc_family("mp4"), None);
        let caps = caps_all();
        // Spreadsheet: pdf/xlsx/ods/csv — never docx/pptx.
        let calc: Vec<String> = feasible_targets(MediaKind::Document, "xlsx", &caps)
            .into_iter()
            .map(|t| t.ext.to_string())
            .collect();
        assert!(calc.contains(&"pdf".to_string()));
        assert!(calc.contains(&"csv".to_string()));
        assert!(!calc.contains(&"xlsx".to_string()));
        assert!(!calc.contains(&"docx".to_string()));
        assert!(!calc.contains(&"pptx".to_string()));
        // No LibreOffice → no document targets at all.
        let mut no_office = caps.clone();
        no_office.office = false;
        assert!(feasible_targets(MediaKind::Document, "docx", &no_office).is_empty());
        assert!(feasible_targets(MediaKind::Document, "xlsx", &no_office).is_empty());
        // …while ffmpeg targets are unaffected.
        assert!(!feasible_targets(MediaKind::Video, "mp4", &no_office).is_empty());
    }

    #[test]
    fn subtitle_targets_stay_in_kind() {
        let caps = caps_all();
        let subs: Vec<String> = feasible_targets(MediaKind::Subtitle, "srt", &caps)
            .into_iter()
            .map(|t| t.ext.to_string())
            .collect();
        assert!(subs.contains(&"vtt".to_string()));
        assert!(subs.contains(&"ass".to_string()));
        assert!(!subs.contains(&"srt".to_string()));
        assert!(!subs.contains(&"mp4".to_string()));
        // New kinds resolve by extension without any probe.
        assert_eq!(kind_for(Path::new("a.srt"), None), Some(MediaKind::Subtitle));
        assert_eq!(kind_for(Path::new("report.DOCX"), None), Some(MediaKind::Document));
        assert_eq!(kind_for(Path::new("sheet.xlsx"), None), Some(MediaKind::Document));
        assert_eq!(kind_for(Path::new("clip.flv"), None), Some(MediaKind::Video));
    }

    #[test]
    fn office_override_and_graceful_absence() {
        // Explicit override wins (custom installs, tests).
        std::env::set_var("SHRINKR_SOFFICE", "C:\\fake\\soffice.exe");
        assert_eq!(
            find_office(),
            Some("C:\\fake\\soffice.exe".to_string())
        );
        std::env::remove_var("SHRINKR_SOFFICE");
        // On machines without LibreOffice the document runner fails
        // loudly instead of spawning garbage.
        if !caps().office {
            let r = run_document_convert(
                Path::new("report.docx"),
                "pdf",
                &Path::new("out.pdf").to_path_buf(),
                &|_| {},
                &Arc::new(AtomicBool::new(false)),
            );
            assert!(r.is_err());
            assert!(r.unwrap_err().contains("LibreOffice not found"));
        }
    }

    #[test]
    fn doc_estimate_needs_no_probe() {
        let e = estimate_for_bytes(2_000_000, MediaKind::Document, "pdf");
        assert_eq!(e.new_bytes, 2_000_000);
        assert_eq!(e.note, "convert");
        let label = estimate_label_for_bytes(2_000_000, MediaKind::Document, "pdf");
        assert!(label.contains("~same"), "{label}");
    }

    #[test]
    fn place_never_clobbers() {
        let d = std::env::temp_dir().join(format!(
            "shrinkr-conv-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let orig = d.join("clip.avi");
        let other = d.join("clip.mp4");
        let tmp = d.join("tmp-out.mp4");
        std::fs::write(&orig, b"orig-avi").unwrap();
        std::fs::write(&other, b"unrelated").unwrap();
        std::fs::write(&tmp, b"converted").unwrap();
        let (saved, dest) = place_converted_output(&orig, &tmp, "mp4", false).unwrap();
        assert_eq!(saved, 0);
        assert_eq!(dest, d.join("clip-converted.mp4"));
        assert_eq!(std::fs::read(&other).unwrap(), b"unrelated");
        assert!(orig.exists(), "keep-both must not delete the original");
        std::fs::remove_dir_all(&d).ok();
    }
}
