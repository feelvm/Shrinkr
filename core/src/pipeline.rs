//! Pipeline selection: preflight, resolution policy and fallback hierarchy.
//!
//! Fallback hierarchy (runtime capability driven):
//!
//! 1. NVDEC/CUDA hardware decode + HEVC NVENC (zero-copy GPU path)
//! 2. Hardware decode + H.264 NVENC (HEVC unavailable)
//! 3. CPU decode + NVENC (HW decode failed, encoder works)
//! 4. CPU x264 (final fallback; x265 kept as legacy option)
//!
//! The [`VideoBackend`] / [`NvencPreset`] / [`ScalePolicy`] types are the
//! knobs the UI and benchmark matrix drive. [`select_levels`] turns a mode
//! plus [`HwCaps`] into the ordered level list to attempt.

use crate::hw::HwCaps;
use crate::media::MediaFile;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoBackend {
    /// Ordered fallback L1 → L4 driven by [`HwCaps`].
    Auto,
    HevcNvenc,
    H264Nvenc,
    /// Baseline for benchmarks: libx264 CRF 28.
    CpuX264,
    /// Legacy CPU path (kept working, not the default).
    CpuX265,
    /// Max-shrink CPU path: SVT-AV1 preset 8, CRF = the CQ slider value.
    /// Software encode on the CPU (Ryzen path); needs libsvtav1 in ffmpeg.
    CpuAv1,
    /// Audio-only pass: video stream-copied untouched (any codec), audio
    /// converted per the audio setting, subs copied, everything else
    /// dropped. No quality slider, preset, or resolution applies — speed
    /// of a remux, savings purely from audio (+ dropped streams).
    CopyVideo,
}

impl VideoBackend {
    pub fn label(&self) -> &'static str {
        match self {
            VideoBackend::Auto => "Auto (NVDEC+HEVC NVENC → H.264 → CPU)",
            VideoBackend::HevcNvenc => "NVENC HEVC (prefer GPU decode)",
            VideoBackend::H264Nvenc => "NVENC H.264 (compat/fallback)",
            VideoBackend::CpuX264 => "CPU x264 (CRF = quality slider)",
            VideoBackend::CpuX265 => "CPU x265 slow (CRF = quality slider)",
            VideoBackend::CpuAv1 => "CPU AV1 SVT (max shrink, slow)",
            VideoBackend::CopyVideo => "Copy video (audio/subs only, fastest)",
        }
    }

    /// True when the CQ slider means SVT-AV1 CRF rather than NVENC CQ.
    pub fn cq_is_av1_crf(&self) -> bool {
        matches!(self, VideoBackend::CpuAv1)
    }
}

/// NVENC P-series presets. P3–P6 are the benchmark matrix; P1/P2/P7 are
/// accepted for completeness but not benchmarked by default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NvencPreset {
    P3,
    P4,
    P5,
    P6,
}

impl NvencPreset {
    pub fn all_bench() -> [NvencPreset; 4] {
        [
            NvencPreset::P3,
            NvencPreset::P4,
            NvencPreset::P5,
            NvencPreset::P6,
        ]
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            NvencPreset::P3 => "p3",
            NvencPreset::P4 => "p4",
            NvencPreset::P5 => "p5",
            NvencPreset::P6 => "p6",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            NvencPreset::P3 => "P3 (fast)",
            NvencPreset::P4 => "P4 (medium, NVENC default)",
            NvencPreset::P5 => "P5 (slow, good quality)",
            NvencPreset::P6 => "P6 (slower, better quality)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScalePolicy {
    /// Never resize (default).
    Preserve,
    /// Downscale any larger source to 1080p. Never upscales.
    Force1080p,
    /// Downscale any larger source to 720p. Never upscales.
    Force720p,
    /// Downscale any larger source to 480p. Never upscales.
    Force480p,
    /// Fit inside a user-set `WxH` box: downscale-only, aspect preserved,
    /// never upscales. Each side is its own cap — landscape sources hit
    /// the width limit, portrait ones the height limit.
    Custom(u32, u32),
}

impl ScalePolicy {
    pub fn label(&self) -> String {
        match self {
            ScalePolicy::Preserve => "Preserve (never resize)".into(),
            ScalePolicy::Force1080p => "Force max 1080p (downscale only)".into(),
            ScalePolicy::Force720p => "Force max 720p (downscale only)".into(),
            ScalePolicy::Force480p => "Force max 480p (downscale only)".into(),
            ScalePolicy::Custom(w, h) => format!("Fit within {w}x{h} (downscale only)"),
        }
    }

    /// Parse a policy value: `preserve|1080p|720p|480p` or a `WxH` box
    /// (`1920x1080`) for [`ScalePolicy::Custom`]. CLI/GUI share this so
    /// custom sizes parse identically everywhere.
    pub fn parse(s: &str) -> Option<ScalePolicy> {
        match s.to_lowercase().as_str() {
            "preserve" | "keep" | "none" => Some(ScalePolicy::Preserve),
            "1080p" => Some(ScalePolicy::Force1080p),
            "720p" => Some(ScalePolicy::Force720p),
            "480p" => Some(ScalePolicy::Force480p),
            other => {
                let (w, h) = other.split_once('x')?;
                let w = w.trim().parse::<u32>().ok()?;
                let h = h.trim().parse::<u32>().ok()?;
                Some(ScalePolicy::Custom(w, h))
            }
        }
    }
}

/// Attempt level for the FFmpeg backend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// `-hwaccel cuda -hwaccel_output_format cuda` + hevc_nvenc.
    HwHevc,
    /// Same decode path + h264_nvenc.
    HwH264,
    /// CPU decode + hevc_nvenc upload.
    CpuDecodeHevcNvenc,
    /// CPU decode + h264_nvenc upload.
    CpuDecodeH264Nvenc,
    /// libx264 fallback.
    CpuX264,
    /// libx265 legacy.
    CpuX265,
    /// SVT-AV1 software encode (preset 8, CRF = CQ slider).
    CpuAv1,
    /// Stream-copy video untouched (any input codec).
    CopyVideo,
}

impl Level {
    pub fn encoder_name(&self) -> &'static str {
        match self {
            Level::HwHevc | Level::CpuDecodeHevcNvenc => "hevc_nvenc",
            Level::HwH264 | Level::CpuDecodeH264Nvenc => "h264_nvenc",
            Level::CpuX264 => "libx264",
            Level::CpuX265 => "libx265",
            Level::CpuAv1 => "libsvtav1",
            Level::CopyVideo => "copy",
        }
    }
    pub fn uses_hw_decode(&self) -> bool {
        matches!(self, Level::HwHevc | Level::HwH264)
    }
    pub fn uses_nvenc(&self) -> bool {
        matches!(
            self,
            Level::HwHevc | Level::HwH264 | Level::CpuDecodeHevcNvenc | Level::CpuDecodeH264Nvenc
        )
    }
}

/// Ordered attempt list for a mode + detected caps.
pub fn select_levels(mode: VideoBackend, caps: &HwCaps) -> Vec<Level> {
    match mode {
        VideoBackend::Auto => {
            let mut v = vec![];
            if caps.cuda_hwaccel && caps.hevc_nvenc {
                v.push(Level::HwHevc);
            }
            if caps.cuda_hwaccel && caps.h264_nvenc {
                v.push(Level::HwH264);
            }
            if caps.hevc_nvenc {
                v.push(Level::CpuDecodeHevcNvenc);
            }
            if caps.h264_nvenc {
                v.push(Level::CpuDecodeH264Nvenc);
            }
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::HevcNvenc => {
            let mut v = vec![];
            if caps.cuda_hwaccel && caps.hevc_nvenc {
                v.push(Level::HwHevc);
            }
            if caps.hevc_nvenc {
                v.push(Level::CpuDecodeHevcNvenc);
            }
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::H264Nvenc => {
            let mut v = vec![];
            if caps.cuda_hwaccel && caps.h264_nvenc {
                v.push(Level::HwH264);
            }
            if caps.h264_nvenc {
                v.push(Level::CpuDecodeH264Nvenc);
            }
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::CpuX264 => vec![Level::CpuX264],
        VideoBackend::CpuX265 => vec![Level::CpuX265, Level::CpuX264],
        VideoBackend::CopyVideo => vec![Level::CopyVideo],
        VideoBackend::CpuAv1 => {
            // Explicit max-shrink choice: SVT first, x264 only if libsvtav1
            // is missing from this ffmpeg build.
            if caps.svt_av1 {
                vec![Level::CpuAv1]
            } else {
                vec![Level::CpuAv1, Level::CpuX264]
            }
        }
    }
}

/// True when CUDA hardware decode is worth attempting for this file.
///
/// A failed HW-decode attempt costs a full wasted encode before the
/// fallback fires, so skip levels we can prove will fail from probe data:
/// * 4:4:4 chroma — the CUDA path rejects it
///   ("Failed setup for format cuda", falls back to software anyway).
/// * codecs with no matching `*_cuvid` NVDEC decoder in this ffmpeg build
///   (e.g. AV1 on pre-Ada GPUs).
///
/// Unknown codecs / empty decoder lists stay permissive: trying HW once is
/// cheaper than wrongly forcing every file onto CPU decode.
pub fn hw_decode_viable(m: &MediaFile, caps: &HwCaps) -> bool {
    if !caps.cuda_hwaccel {
        return false;
    }
    if m.pix_fmt.to_lowercase().contains("444") {
        return false;
    }
    let need = match m.vcodec.as_str() {
        "h264" => "h264_cuvid",
        "hevc" => "hevc_cuvid",
        "mpeg4" | "msmpeg4" | "xvid" => "mpeg4_cuvid",
        "mpeg2video" => "mpeg2_cuvid",
        "vp9" => "vp9_cuvid",
        "av1" => "av1_cuvid",
        "vc1" => "vc1_cuvid",
        _ => return true,
    };
    if caps.cuvid_decoders.is_empty() {
        return true;
    }
    caps.cuvid_decoders.iter().any(|d| d == need)
}

/// Like [`select_levels`] but drops the HW-decode levels when
/// [`hw_decode_viable`] says they cannot succeed for `media`.
/// Use this wherever a real file is in hand (encode + dry-run echo);
/// the bare `select_levels` remains for capability-only display.
pub fn select_levels_for_media(mode: VideoBackend, caps: &HwCaps, media: &MediaFile) -> Vec<Level> {
    let levels = select_levels(mode, caps);
    if hw_decode_viable(media, caps) {
        levels
    } else {
        levels.into_iter().filter(|l| !l.uses_hw_decode()).collect()
    }
}

// ── Preflight (intelligent early exit) ───────────────────────

#[derive(Clone, Debug)]
pub enum Preflight {
    Shrink { reason: String },
    Skip { reason: String },
}

/// Expected output/input byte ratio for the NVENC HEVC CQ28 path.
/// Initial estimates only — the benchmark mode exists to replace these
/// with measured values. NVENC CQ is NOT assumed equal to x264 CRF.
pub fn nvenc_hevc_ratio_for(vcodec: &str) -> f64 {
    match vcodec {
        "mpeg4" | "msmpeg4" | "xvid" => 0.52,
        "h264" => 0.75,
        "hevc" => 0.95,
        "av1" => 1.0,
        "mpeg2video" => 0.60,
        "vp9" => 0.95,
        _ => 0.80,
    }
}

pub fn x264_ratio_for(vcodec: &str) -> f64 {
    match vcodec {
        "mpeg4" | "msmpeg4" | "xvid" => 0.35,
        "h264" => 0.62,
        "hevc" => 0.95,
        "av1" => 1.0,
        "mpeg2video" => 0.45,
        "vp9" => 0.92,
        _ => 0.70,
    }
}

// ── Combined size estimator ────────────────────────────────
///
/// Every factor chips in: codec base ratio × CQ × NVENC preset × resolution
/// scale × audio plan. `EstParams` mirrors the GUI/CLI settings so the
/// estimate always describes the job that would actually run.

#[derive(Clone, Copy, Debug)]
pub struct EstParams {
    pub backend: VideoBackend,
    pub preset: NvencPreset,
    pub cq: u32,
    pub scale: ScalePolicy,
    /// Opus target in bps (`Some(64000)` default). `None` = Off: keep
    /// every audio track as-is (stream copy, never re-encoded).
    pub opus_bps: Option<u32>,
    /// Keep every audio track (dual-audio releases). Default false:
    /// primary track only, the rest dropped.
    pub all_audio: bool,
    /// Still-image quality on the same 18–40 CQ scale, kept separate from
    /// the video slider: an image batch has no use for NVENC CQ, and a
    /// video batch shouldn't drag image quality along.
    pub image_cq: u32,
    /// Still-image resolution policy, separate from the video one for the
    /// same reason.
    pub image_scale: ScalePolicy,
    /// Preserve still-image formats: outputs keep their source extension
    /// (png→png, tiff→tiff) using format-preserving techniques (alpha-plane
    /// drop, near-lossless 256-color palettes, TIFF deflate, max re-encode) and
    /// format conversions (png→jpeg, alpha→webp) are suppressed — a file
    /// that can't win as its own format skips instead of converting.
    pub image_preserve_format: bool,
}

/// Default Opus target: transparent-ish stereo, lean 5.1.
pub const DEFAULT_OPUS_BPS: u32 = 64_000;

/// AAC fallback bitrate for a given Opus target: AAC needs roughly twice
/// the bits for similar quality (floored/ceilinged to sane bounds).
pub fn aac_bps_for(opus_bps: u32) -> u32 {
    (opus_bps * 2).clamp(96_000, 320_000)
}

#[derive(Clone, Debug)]
pub struct FileEstimate {
    /// combined output/input byte ratio (video + audio).
    pub ratio: f64,
    pub new_bytes: u64,
    /// video-only factor chain, for transparency/debugging.
    pub video_factor: f64,
    pub base_ratio: f64,
    pub cq_factor: f64,
    pub preset_factor: f64,
    pub scale_factor: f64,
    /// source density vs typical (1.0 = typical; <1 = dense/grainy).
    pub density: f64,
    pub scaled_to: Option<(u32, u32)>,
    pub audio_mode: String, // "copy" | "opus64k" | "aac128k" | "none" (+bitrates)
    pub new_audio_bytes: u64,
}

/// CQ multiplier around the CQ28 reference: ≈7% per point.
/// Conservative on purpose — real footage varies, and under-promising
/// beats over-promising for a shrink planner.
pub fn cq_factor(cq: u32) -> f64 {
    0.93f64.powf(cq as f64 - 28.0)
}

/// Preset multiplier relative to P5 (the default). Fast presets spend
/// more bits for the same CQ; P6 squeezes a little more.
pub fn preset_factor(backend: VideoBackend, preset: NvencPreset) -> f64 {
    match backend {
        VideoBackend::CpuX264
        | VideoBackend::CpuX265
        | VideoBackend::CpuAv1
        | VideoBackend::CopyVideo => 1.0,
        _ => match preset {
            NvencPreset::P3 => 1.12,
            NvencPreset::P4 => 1.04,
            NvencPreset::P5 => 1.0,
            NvencPreset::P6 => 0.96,
        },
    }
}

/// Resolution multiplier from pixel counts. Bitrate tracks pixels
/// sub-linearly (less detail per frame after downscale), hence ^0.85.
pub fn scale_factor(m: &MediaFile, scale: ScalePolicy) -> f64 {
    match effective_scale_target(m, scale) {
        None => 1.0,
        Some((w, h)) => {
            let inp = (m.width as f64 * m.height as f64).max(1.0);
            let out = (w as f64 * h as f64).max(1.0);
            (out / inp).powf(0.85)
        }
    }
}

fn quality_label(p: &EstParams) -> String {
    match p.backend {
        VideoBackend::CpuX264 => format!("x264 crf{}", p.cq),
        VideoBackend::CpuX265 => format!("x265 slow crf{}", p.cq),
        VideoBackend::CpuAv1 => format!("svt-av1 p8 crf{}", p.cq),
        VideoBackend::CopyVideo => "copy video".into(),
        _ => format!("cq{} {}", p.cq, p.preset.as_str()),
    }
}

/// Full-file estimate: video part (factored) + audio part (copy or 64k
/// Opus), split via stream bitrate when known.
pub fn estimate_file(m: &MediaFile, p: &EstParams) -> FileEstimate {
    let density = source_density(m);
    // CQ-mode output size is content-driven: grainy/dense sources cost
    // nearly as many bits in HEVC as they did in H.264, while clean
    // sources collapse. The base table assumes typical density, so scale
    // the *saving* by observed density (measured: 8 Mbps noisy 1080p saved
    // ~1% at CQ28 where the raw table predicted 25%).
    let base = 1.0 - (1.0 - est_ratio(&m.vcodec, p.backend)) * density;
    // CQ/CRF curve around the 28 reference. Tables above are calibrated at
    // 28 for every backend (NVENC CQ, x264/x265 CRF, SVT CRF), so the same
    // ≈7%-per-point factor applies to all — the shared quality slider is
    // meaningful on CPU backends too.
    let cf = cq_factor(p.cq);
    let pf = preset_factor(p.backend, p.preset);
    let sf = scale_factor(m, p.scale);
    let video_factor = base * cf * pf * sf;
    let scaled_to = effective_scale_target(m, p.scale);
    let plan = plan_streams(m, true, p.opus_bps, p.all_audio);
    let dur = m.duration_s;
    let (audio_mode, new_audio_bytes): (String, u64) = if !plan.has_audio || dur <= 0.0 {
        ("none".into(), 0)
    } else {
        let items = planned_audio(m, &plan, dur);
        (
            audio_mode_label(m, &plan),
            items.iter().map(|&(_, b)| b).sum(),
        )
    };
    let new_bytes = if p.backend == VideoBackend::CopyVideo {
        // Video passes through byte-identical: source video bytes plus the
        // planned audio. Source video = stream rate when known, else total
        // minus ALL source audio (kept or dropped — dropped tracks vanish
        // from the output too).
        let audio_src_total: u64 = m
            .audio
            .iter()
            .map(|t| {
                if dur > 0.0 {
                    audio_src_bytes(t, dur)
                } else {
                    0
                }
            })
            .sum();
        let vbytes = match m.vbitrate {
            Some(vbr) if dur > 0.0 => (vbr as f64 * dur / 8.0) as u64,
            _ => m.bytes.saturating_sub(audio_src_total),
        };
        vbytes.saturating_add(new_audio_bytes)
    } else if dur > 0.0 {
        let vbytes = match m.vbitrate {
            Some(vbr) => vbr as f64 * dur / 8.0,
            None => m.bytes as f64 * 0.9, // ≈90% video when stream rate unknown
        };
        (vbytes * video_factor + new_audio_bytes as f64) as u64
    } else {
        (m.bytes as f64 * video_factor) as u64
    };
    // Copy-video passes video through: factor chain is decorative (1.0).
    let copy = p.backend == VideoBackend::CopyVideo;
    FileEstimate {
        ratio: new_bytes as f64 / m.bytes.max(1) as f64,
        new_bytes,
        video_factor: if copy { 1.0 } else { video_factor },
        base_ratio: if copy { 1.0 } else { base },
        cq_factor: if copy { 1.0 } else { cf },
        preset_factor: if copy { 1.0 } else { pf },
        scale_factor: if copy { 1.0 } else { sf },
        density,
        scaled_to: if copy { None } else { scaled_to },
        audio_mode,
        new_audio_bytes,
    }
}

/// Guess at a source audio bitrate when ffprobe reports none (common in
/// MKV): typical release bitrates by channel count. Used for the copy
/// branch, the copy-video video split, and the container-bitrate fallback.
/// Conservative by design — underestimating source audio under-promises
/// savings, never over-promises them... except it can still miss fat
/// tracks, which is why this is a guess, not a measurement.
pub fn guessed_audio_bps(channels: u8) -> u64 {
    match channels {
        0 | 1 => 64_000,
        2 => 128_000,
        3..=6 => 384_000,
        _ => 768_000,
    }
}
/// Effective video bitrate in bps: the stream rate when ffprobe reports
/// one, otherwise the container rate minus the primary audio share.
/// Stream `bit_rate` is frequently `N/A` for MKV, so without this fallback
/// the density model silently disables itself (returns 1.0) exactly for
/// the noisy-remux files that need the correction most.
pub fn effective_vbitrate(m: &MediaFile) -> Option<u64> {
    if let Some(b) = m.vbitrate {
        if b > 0 {
            return Some(b);
        }
    }
    let total = m.format_bitrate?;
    if total == 0 {
        return None;
    }
    let audio_bps = match m.primary_audio() {
        Some(a) => a.bitrate.unwrap_or_else(|| guessed_audio_bps(a.channels)),
        None => 0,
    };
    total.checked_sub(audio_bps).filter(|&v| v > 0)
}

/// Source density: observed bits-per-pixel vs typical streaming density
/// for the source codec. 1.0 = typical (no correction); below = dense
/// (less saving); above (lean) is capped — the base table already assumes
/// reasonably efficient sources, so leanness only buys a little extra.
fn source_density(m: &MediaFile) -> f64 {
    let fps = if m.fps > 0.1 { m.fps } else { return 1.0 };
    let vbr = match effective_vbitrate(m) {
        Some(b) if b > 0 => b as f64,
        _ => return 1.0,
    };
    let px = (m.width as f64 * m.height as f64).max(1.0);
    let bpp = vbr / (px * fps);
    let typical = match m.vcodec.as_str() {
        "mpeg4" | "msmpeg4" | "xvid" => 0.12,
        "h264" => 0.07,
        "hevc" => 0.045,
        "av1" => 0.04,
        "vp9" => 0.045,
        "mpeg2video" => 0.20,
        _ => 0.07,
    };
    (typical / bpp).clamp(0.35, 1.15)
}

/// Decide shrink vs skip.
///
/// * `min_saving_pct` is the configurable threshold (e.g. 10 → skip when
///   the estimated saving is below 10%).
pub fn preflight(m: &MediaFile, p: &EstParams, min_saving_pct: f64) -> Preflight {
    use crate::media::human_bytes;
    // Corrupt / undecodable inputs are rejected by probe already; a missing
    // video stream here means skip, not crash.
    if m.vcodec == "unknown" || m.width == 0 {
        return Preflight::Skip {
            reason: "no video stream found".into(),
        };
    }
    // Already-efficient fast path: only proceed when the bitrate suggests
    // headroom (bitrate-aware, not codec-alone). When there is no headroom
    // the estimator still gets the final word — with SVT-AV1 or downscaling
    // selected, re-crunching AV1/HEVC/VP9 can pay off hugely, and an early
    // return here would discard that (once showed 53% as "negligible").
    // Copy-video bypasses video-codec gating entirely: only the audio side
    // can save anything, and the estimator below prices exactly that.
    if p.backend != VideoBackend::CopyVideo && matches!(m.vcodec.as_str(), "av1" | "hevc" | "vp9") {
        if !bitrate_suggests_headroom(m) {
            // Still allow the estimator below to overrule when the file is
            // huge (e.g. remuxed Blu-ray HEVC at high bitrate).
            let est = estimate_file(m, p);
            let saving = (1.0 - est.ratio) * 100.0;
            if saving < min_saving_pct {
                return Preflight::Skip {
                    reason: format!(
                        "already {}{} — negligible gain expected (est. {} → {}, {:.0}%)",
                        m.vcodec,
                        bitrate_suffix(m),
                        human_bytes(m.bytes),
                        human_bytes(est.new_bytes),
                        saving,
                    ),
                };
            }
        }
    }
    let est = estimate_file(m, p);
    let saving = (1.0 - est.ratio) * 100.0;
    // Copy-video keeps resolution no matter the policy — say so instead of
    // implying a resize that will never happen.
    let res_note = if p.backend == VideoBackend::CopyVideo && p.scale != ScalePolicy::Preserve {
        format!(
            "{} (resolution setting ignored in copy mode)",
            m.res_label()
        )
    } else {
        match est.scaled_to {
            Some((w, h)) => format!("{}→{}x{}", m.res_label(), w, h),
            None => m.res_label(),
        }
    };
    let dense_note = if est.density < 0.7 {
        "; dense/grainy source"
    } else {
        ""
    };
    if saving < min_saving_pct {
        Preflight::Skip {
            reason: format!(
                "est. {} → {} ({:.0}% < {:.0}% threshold; {} {}, {}, audio {}{})",
                human_bytes(m.bytes),
                human_bytes(est.new_bytes),
                saving,
                min_saving_pct,
                m.vcodec,
                res_note,
                quality_label(p),
                est.audio_mode,
                dense_note
            ),
        }
    } else {
        Preflight::Shrink {
            reason: format!(
                "est. {} → {} ({:.0}% off; {} {}, {}, audio {}{})",
                human_bytes(m.bytes),
                human_bytes(est.new_bytes),
                saving,
                m.vcodec,
                res_note,
                quality_label(p),
                est.audio_mode,
                dense_note
            ),
        }
    }
}

/// Preflight for any probed input: still images route to the image
/// model with their own quality/resolution settings
/// ([`crate::images::preflight_image`]), everything else to the video
/// pipeline above. The GUI/CLI layers call this instead of [`preflight`]
/// so mixed video+image batches plan correctly.
pub fn preflight_auto(m: &MediaFile, p: &EstParams, min_saving_pct: f64) -> Preflight {
    if crate::images::is_shrinkable_image(&m.path) {
        crate::images::preflight_image(
            m,
            p.image_cq,
            p.image_scale,
            p.image_preserve_format,
            min_saving_pct,
        )
    } else {
        preflight(m, p, min_saving_pct)
    }
}

fn est_ratio(vcodec: &str, backend: VideoBackend) -> f64 {
    match backend {
        VideoBackend::CpuX264 => x264_ratio_for(vcodec),
        VideoBackend::CpuX265 => match vcodec {
            "mpeg4" | "msmpeg4" | "xvid" => 0.23,
            "h264" => 0.55,
            "hevc" => 0.85,
            "av1" => 0.97,
            "mpeg2video" => 0.35,
            "vp9" => 0.85,
            _ => 0.60,
        },
        // SVT-AV1 preset 8 at CRF≈28: roughly 3/4 the bits of x265 slow
        // at the same CRF. Replace with bench-measured values when available
        // (same deal as the NVENC table above).
        VideoBackend::CpuAv1 => match vcodec {
            "mpeg4" | "msmpeg4" | "xvid" => 0.17,
            "h264" => 0.42,
            "hevc" => 0.68,
            "av1" => 0.95,
            "mpeg2video" => 0.27,
            "vp9" => 0.68,
            _ => 0.47,
        },
        // Copy-video never re-encodes: video ratio is exactly 1.0
        // (estimate_file shortcuts before using this, belt and braces).
        VideoBackend::CopyVideo => 1.0,
        _ => nvenc_hevc_ratio_for(vcodec),
    }
}

fn bitrate_suffix(m: &MediaFile) -> String {
    match effective_vbitrate(m) {
        Some(b) => format!(", {:.1} Mbps", b as f64 / 1e6),
        None => String::new(),
    }
}

/// True when the (video) bitrate is high enough that re-encoding may still
/// pay off even for efficient codecs. Tiers key off the SHORT side so
/// landscape 1920×1080 and portrait 1080×1920 both land in the 1080p tier.
fn bitrate_suggests_headroom(m: &MediaFile) -> bool {
    let b = match effective_vbitrate(m) {
        Some(b) => b as f64,
        None => return true, // unknown → don't block
    };
    let short = m.width.min(m.height).max(1);
    let threshold = if short >= 2000 {
        12e6
    } else if short >= 1300 {
        7e6
    } else if short >= 900 {
        4e6
    } else {
        2e6
    };
    b > threshold
}

// ── Resolution policy ────────────────────────────────────────

/// Returns `(w,h)` to scale to (even numbers, aspect preserved), or `None`
/// to keep the source resolution. Never upscales.
///
/// The preset caps apply to the SHORT side so landscape `1920x1080` and
/// portrait `1080x1920` are treated identically (both are "1080p-class"),
/// matching the bitrate-headroom tiers. Keying off height alone would
/// mangle portrait sources while leaving their landscape twins untouched.
/// `Custom(w, h)` instead fits inside the box: each side is its own cap,
/// so landscape sources hit the width limit and portrait ones the height.
pub fn scale_target(m: &MediaFile, policy: ScalePolicy) -> Option<(u32, u32)> {
    if m.width < 16 || m.height < 16 {
        return None;
    }
    // Uniform shrink factor (< 1.0), or None to keep the source
    // resolution. Never upscales: sources already inside the cap/box
    // stay untouched.
    let k = match policy {
        ScalePolicy::Preserve => return None,
        ScalePolicy::Force1080p => short_side_k(m, 1080)?,
        ScalePolicy::Force720p => short_side_k(m, 720)?,
        ScalePolicy::Force480p => short_side_k(m, 480)?,
        ScalePolicy::Custom(bw, bh) => {
            let (bw, bh) = (bw.max(2), bh.max(2));
            if m.width <= bw && m.height <= bh {
                return None;
            }
            (bw as f64 / m.width as f64).min(bh as f64 / m.height as f64)
        }
    };
    let even = |v: u32| (((v as f64 * k) / 2.0).round() as u32 * 2).max(2);
    Some((even(m.width), even(m.height)))
}

/// Shrink factor for a short-side cap, or `None` when the source already
/// fits (short side at/below the cap).
fn short_side_k(m: &MediaFile, cap: u32) -> Option<f64> {
    let short = m.width.min(m.height);
    if short <= cap {
        return None;
    }
    Some(cap as f64 / short as f64)
}

/// [`scale_target`] plus the mandatory even-dimensions fix: NVENC,
/// x264/x265 (yuv420p) and SVT-AV1 all reject odd widths/heights
/// ("not divisible by 2"), and old AVIs/DivX files carry them often.
/// With `Preserve` on odd input this yields a 1px shave instead of a
/// guaranteed failure on every fallback level.
pub fn effective_scale_target(m: &MediaFile, policy: ScalePolicy) -> Option<(u32, u32)> {
    if let Some(t) = scale_target(m, policy) {
        return Some(t);
    }
    if m.width >= 2 && m.height >= 2 && (m.width % 2 == 1 || m.height % 2 == 1) {
        Some((m.width - m.width % 2, m.height - m.height % 2))
    } else {
        None
    }
}

// ── Stream strategy ──────────────────────────────────────────

/// Which streams to map for a shrinking job:
/// primary video + primary audio + (optionally) all subtitles.
/// Secondary video, extra audio, attachments and global metadata are
/// deliberately dropped — that is the point of shrinking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    /// Stream-copy (Off, or source already at/below target).
    Copy,
    /// Opus re-encode at the chosen bitrate, channels preserved.
    Opus(u32),
    /// Exotic channel layouts Opus mapping families reject
    /// (5.1(side), ambisonics, >8ch): native AAC instead, ~2x the bits.
    Aac(u32),
}

impl AudioCodec {
    /// ffmpeg `-b:a` value, e.g. "64k".
    pub fn bitrate_arg(&self) -> String {
        match self {
            AudioCodec::Copy => "copy".into(),
            AudioCodec::Opus(b) | AudioCodec::Aac(b) => format!("{}k", b / 1000),
        }
    }

    /// Log/estimate label, e.g. "opus64k".
    pub fn label(&self) -> String {
        match self {
            AudioCodec::Copy => "copy".into(),
            AudioCodec::Opus(b) => format!("opus{}k", b / 1000),
            AudioCodec::Aac(b) => format!("aac{}k", b / 1000),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamPlan {
    pub has_audio: bool,
    pub copy_subs: bool,
    pub audio_copy_eligible: bool,
    /// Codec decision for the primary track (also the single track when
    /// `!all_audio`). Secondary tracks are decided per-track the same way.
    pub audio_codec: AudioCodec,
    pub opus_bps: Option<u32>,
    pub all_audio: bool,
    /// Set by the AAC retry: every re-encoded track goes AAC from here.
    pub force_aac: bool,
    /// Set when the plan had to dodge an Opus-hostile layout; surfaced in
    /// logs so "audio aac128k" never surprises.
    pub audio_layout_note: &'static str,
}

/// Codec decision for one audio track.
pub fn audio_track_codec(
    t: &crate::media::AudioTrack,
    opus_bps: Option<u32>,
    force_aac: bool,
) -> AudioCodec {
    // Off (None) keeps everything as-is however big.
    let Some(target) = opus_bps else {
        return AudioCodec::Copy;
    };
    if audio_copy_eligible_for_track(t, target) {
        return AudioCodec::Copy;
    }
    if force_aac || !opus_layout_ok(t) {
        return AudioCodec::Aac(aac_bps_for(target));
    }
    AudioCodec::Opus(target)
}

pub fn plan_streams(
    m: &MediaFile,
    keep_subs: bool,
    opus_bps: Option<u32>,
    all_audio: bool,
) -> StreamPlan {
    let has_audio = !m.audio.is_empty() && m.acodec != "none";
    // Off (None) keeps every audio track as-is: always copy, no bitrate
    // math, no layout checks. (A previous version fell through to the
    // re-encode branch for unknown-bitrate tracks — Off silently did
    // nothing. Regression test below pins this.)
    if opus_bps.is_none() {
        return StreamPlan {
            has_audio,
            copy_subs: keep_subs && m.sub_count > 0,
            audio_copy_eligible: has_audio,
            audio_codec: AudioCodec::Copy,
            opus_bps,
            all_audio,
            force_aac: false,
            audio_layout_note: "",
        };
    }
    let primary = m.primary_audio();
    let eligible = primary
        .map(|a| audio_copy_eligible_for_track(a, opus_bps.unwrap_or(DEFAULT_OPUS_BPS)))
        .unwrap_or(false);
    let codec = primary
        .map(|a| audio_track_codec(a, opus_bps, false))
        .unwrap_or(AudioCodec::Copy);
    let mut note = "";
    if !eligible {
        if let Some(a) = primary {
            if !opus_layout_ok(a) && !a.layout.is_empty() {
                note = "exotic layout→aac";
            }
        }
    }
    StreamPlan {
        has_audio,
        copy_subs: keep_subs && m.sub_count > 0,
        audio_copy_eligible: eligible,
        audio_codec: codec,
        opus_bps,
        all_audio,
        force_aac: false,
        audio_layout_note: note,
    }
}

/// True when libopus can encode this track: standard layouts, or unknown
/// (assume standard — the AAC retry below covers a wrong guess). Anything
/// with side/ambisonic channels or more than 8 channels goes AAC.
pub fn opus_layout_ok(a: &crate::media::AudioTrack) -> bool {
    if a.channels > 8 {
        return false;
    }
    let l = a.layout.to_lowercase();
    if l.is_empty() || l == "unknown" || l == "unspecified" {
        return true;
    }
    !(l.contains("(side)")
        || l.contains("ambisonic")
        || l.contains("22.2")
        || l.contains("hexadecagonal"))
}

/// Output audio tracks in mapping order: just the primary, or all of them
/// with `all_audio` (dual-audio releases).
pub fn selected_audio_tracks<'a>(
    m: &'a MediaFile,
    plan: &StreamPlan,
) -> Vec<&'a crate::media::AudioTrack> {
    if !plan.has_audio {
        return vec![];
    }
    if plan.all_audio {
        m.audio.iter().collect()
    } else {
        m.primary_audio().into_iter().collect()
    }
}

/// Source bytes for one audio track: stream rate when known, else a
/// channel-count guess (see guessed_audio_bps).
pub fn audio_src_bytes(t: &crate::media::AudioTrack, dur: f64) -> u64 {
    let bps = t.bitrate.unwrap_or_else(|| guessed_audio_bps(t.channels));
    (bps as f64 * dur / 8.0) as u64
}

/// Planned output: `(codec decision, NEW bytes)` per output track.
/// `Copy` resolves to the source bytes via [`audio_src_bytes`].
pub fn planned_audio(m: &MediaFile, plan: &StreamPlan, dur: f64) -> Vec<(AudioCodec, u64)> {
    selected_audio_tracks(m, plan)
        .into_iter()
        .map(|t| {
            let c = audio_track_codec(t, plan.opus_bps, plan.force_aac);
            let b = match c {
                AudioCodec::Copy => audio_src_bytes(t, dur),
                AudioCodec::Opus(b) | AudioCodec::Aac(b) => (b as f64 * dur / 8.0) as u64,
            };
            (c, b)
        })
        .collect()
}

/// Short label for logs/estimates, e.g. "opus64k", "copy+opus64k".
/// Duplicate decisions collapse ("opus64k+opus64k" → "opus64k").
pub fn audio_mode_label(m: &MediaFile, plan: &StreamPlan) -> String {
    if !plan.has_audio {
        return "none".into();
    }
    let mut modes: Vec<String> = vec![];
    for t in selected_audio_tracks(m, plan) {
        let l = audio_track_codec(t, plan.opus_bps, plan.force_aac).label();
        if !modes.iter().any(|x| x == &l) {
            modes.push(l);
        }
    }
    if modes.is_empty() {
        "none".into()
    } else {
        modes.join("+")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{AudioTrack, MediaFile};

    fn file(vcodec: &str, w: u32, h: u32, bytes: u64, vbr: Option<u64>) -> MediaFile {
        MediaFile {
            path: std::path::PathBuf::from("t.mp4"),
            bytes,
            vcodec: vcodec.into(),
            width: w,
            height: h,
            fps: 30.0,
            pix_fmt: "yuv420p".into(),
            bit_depth: 8,
            color_space: String::new(),
            color_primaries: String::new(),
            color_transfer: String::new(),
            color_range: String::new(),
            vbitrate: vbr,
            format_bitrate: None,
            duration_s: 600.0,
            audio: vec![AudioTrack {
                codec: "aac".into(),
                bitrate: Some(128_000),
                channels: 2,
                layout: "stereo".into(),
                sample_rate: Some(48000),
            }],
            acodec: "aac".into(),
            video_stream_count: 1,
            audio_stream_count: 1,
            sub_count: 0,
            attach_count: 0,
            has_chapters: false,
        }
    }

    fn params() -> EstParams {
        EstParams {
            backend: VideoBackend::Auto,
            preset: NvencPreset::P5,
            cq: 28,
            scale: ScalePolicy::Preserve,
            opus_bps: Some(DEFAULT_OPUS_BPS),
            all_audio: false,
            image_cq: 28,
            image_scale: ScalePolicy::Preserve,
            image_preserve_format: false,
        }
    }

    fn caps_with(cuvid: &[&str]) -> HwCaps {
        HwCaps {
            cuda_hwaccel: true,
            hevc_nvenc: true,
            h264_nvenc: true,
            av1_nvenc_listed: false,
            av1_nvenc_usable: false,
            svt_av1: true,
            cuvid_decoders: cuvid.iter().map(|s| s.to_string()).collect(),
            gpu_label: "test".into(),
        }
    }

    #[test]
    fn hw_decode_skips_proven_bad_inputs() {
        let caps = caps_with(&["h264_cuvid", "hevc_cuvid"]);
        // Normal H.264: HW path viable, HwHevc first.
        let m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        assert!(hw_decode_viable(&m, &caps));
        let levels = select_levels_for_media(VideoBackend::Auto, &caps, &m);
        assert_eq!(levels[0], Level::HwHevc);
        // 4:4:4 chroma: CUDA rejects it — start at CPU-decode NVENC.
        let mut yuv444 = m.clone();
        yuv444.pix_fmt = "yuv444p".into();
        assert!(!hw_decode_viable(&yuv444, &caps));
        let levels = select_levels_for_media(VideoBackend::Auto, &caps, &yuv444);
        assert!(levels.iter().all(|l| !l.uses_hw_decode()));
        assert_eq!(levels[0], Level::CpuDecodeHevcNvenc);
        // AV1 without av1_cuvid: same skip.
        let av1 = file("av1", 1920, 1080, 1_000_000_000, Some(8_000_000));
        assert!(!hw_decode_viable(&av1, &caps));
        // Unknown codec stays permissive (try HW, fall back on failure).
        let odd = file("prores", 1920, 1080, 1_000_000_000, Some(50_000_000));
        assert!(hw_decode_viable(&odd, &caps));
    }

    #[test]
    fn av1_backend_selects_svt_and_estimates_below_x265() {
        let caps = caps_with(&["h264_cuvid", "hevc_cuvid", "av1_cuvid"]);
        let m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        let levels = select_levels_for_media(VideoBackend::CpuAv1, &caps, &m);
        assert_eq!(levels, vec![Level::CpuAv1]);
        assert_eq!(levels[0].encoder_name(), "libsvtav1");
        // Without SVT in the build, x264 is the honest fallback.
        let mut no_svt = caps.clone();
        no_svt.svt_av1 = false;
        let levels = select_levels_for_media(VideoBackend::CpuAv1, &no_svt, &m);
        assert_eq!(levels, vec![Level::CpuAv1, Level::CpuX264]);
        let av1_params = EstParams {
            backend: VideoBackend::CpuAv1,
            preset: NvencPreset::P5,
            cq: 28,
            scale: ScalePolicy::Preserve,
            opus_bps: Some(DEFAULT_OPUS_BPS),
            all_audio: false,
            image_cq: 28,
            image_scale: ScalePolicy::Preserve,
            image_preserve_format: false,
        };
        let x265_params = EstParams {
            backend: VideoBackend::CpuX265,
            ..av1_params
        };
        let e_av1 = estimate_file(&m, &av1_params);
        let e_x265 = estimate_file(&m, &x265_params);
        assert!(
            e_av1.new_bytes < e_x265.new_bytes,
            "av1 {} vs x265 {}",
            e_av1.new_bytes,
            e_x265.new_bytes
        );
    }

    #[test]
    fn cq_reference_is_neutral() {
        assert!((cq_factor(28) - 1.0).abs() < 1e-9);
        assert!(cq_factor(32) < 1.0 && cq_factor(32) > 0.6);
        assert!(cq_factor(24) > 1.0);
    }

    #[test]
    fn resolution_flows_into_estimate() {
        let m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        let base = estimate_file(&m, &params());
        let mut p = params();
        p.scale = ScalePolicy::Force720p;
        let small = estimate_file(&m, &p);
        assert_eq!(small.scaled_to, Some((1280, 720)));
        assert!(small.new_bytes < base.new_bytes);
        // 1080p→720p pixel ratio 0.444^0.85 ≈ 0.50 on the video part
        assert!((small.scale_factor - 0.50).abs() < 0.05);
    }

    #[test]
    fn cpu_backends_honour_the_quality_slider() {
        // The shared CQ/CRF slider must move CPU estimates too (x264/x265
        // used to ignore it, pinning cf=1.0).
        let m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        for backend in [
            VideoBackend::CpuX264,
            VideoBackend::CpuX265,
            VideoBackend::CpuAv1,
        ] {
            let mut p28 = params();
            p28.backend = backend;
            p28.cq = 28;
            let mut p32 = params();
            p32.backend = backend;
            p32.cq = 32;
            let e28 = estimate_file(&m, &p28);
            let e32 = estimate_file(&m, &p32);
            assert!(
                e32.new_bytes < e28.new_bytes,
                "{backend:?}: crf32 {} should beat crf28 {}",
                e32.new_bytes,
                e28.new_bytes
            );
        }
    }

    #[test]
    fn copy_video_passes_video_through() {
        // Breaking-Bad-shaped file: lean HEVC video, fat unknown-bitrate
        // audio. Only the audio side may shrink.
        let mut m = file("hevc", 1280, 720, 415_000_000, None);
        m.audio[0].bitrate = Some(400_000);
        m.audio[0].channels = 6;
        m.audio[0].layout = "5.1".into();
        let mut p = params();
        p.backend = VideoBackend::CopyVideo;
        let est = estimate_file(&m, &p);
        assert_eq!(est.scaled_to, None);
        assert!((est.video_factor - 1.0).abs() < 1e-9);
        assert_eq!(est.audio_mode, "opus64k");
        // Video ≈ bytes − audio source (400k×600s/8 = 30 MB) = 385 MB;
        // audio → 64k×600s/8 = 4.8 MB; total ≈ 389.8 MB (6% off).
        assert!(
            (est.new_bytes as f64 - 389_800_000.0).abs() < 5_000_000.0,
            "got {}",
            est.new_bytes
        );
        assert_eq!(
            select_levels(VideoBackend::CopyVideo, &caps_with(&[])),
            vec![Level::CopyVideo]
        );
        // Below a 10% threshold it skips — but says copy video, and the
        // AV1/HEVC early-skip must not hijack efficient codecs here.
        match preflight(&m, &p, 10.0) {
            Preflight::Skip { reason } => {
                assert!(reason.contains("copy video"), "reason: {reason}")
            }
            Preflight::Shrink { reason } => panic!("6% should skip at 10%: {reason}"),
        }
        let mut av1 = m.clone();
        av1.vcodec = "av1".into();
        match preflight(&av1, &p, 10.0) {
            Preflight::Skip { reason } => assert!(
                !reason.contains("negligible gain expected"),
                "copy mode must not codec-gate: {reason}"
            ),
            Preflight::Shrink { reason } => panic!("6% should skip at 10%: {reason}"),
        }
    }

    #[test]
    fn guessed_audio_tracks_channels() {
        assert_eq!(guessed_audio_bps(1), 64_000);
        assert_eq!(guessed_audio_bps(2), 128_000);
        assert_eq!(guessed_audio_bps(6), 384_000);
        assert_eq!(guessed_audio_bps(8), 768_000);
    }

    #[test]
    fn copy_video_guesses_unknown_audio_by_channels() {
        // E01-shaped: unknown-bitrate 6ch audio. 384k guess → video split
        // lands near reality instead of assuming stereo.
        let mut m = file("hevc", 1280, 720, 415_000_000, None);
        m.audio[0].bitrate = None;
        m.audio[0].channels = 6;
        m.duration_s = 3486.0;
        let mut p = params();
        p.backend = VideoBackend::CopyVideo;
        let est = estimate_file(&m, &p);
        // Video ≈ 415 − 384k×3486/8 (167.3 MB) = 247.7 MB;
        // audio → 64k×3486/8 = 27.9 MB; total ≈ 275.6 MB (34% off).
        assert!(
            (est.new_bytes as f64 - 275_600_000.0).abs() < 5_000_000.0,
            "got {}",
            est.new_bytes
        );
    }

    #[test]
    fn never_upscales() {
        let m = file("h264", 1280, 720, 500_000_000, Some(6_000_000));
        let mut p = params();
        p.scale = ScalePolicy::Force1080p;
        let est = estimate_file(&m, &p);
        assert_eq!(est.scaled_to, None);
        assert!((est.scale_factor - 1.0).abs() < 1e-9);
    }

    #[test]
    fn short_side_cap_treats_portrait_like_landscape() {
        // 1080x1920 portrait is 1080p-class: Force1080p must leave it alone,
        // exactly like 1920x1080 landscape.
        let land = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        let port = file("h264", 1080, 1920, 1_000_000_000, Some(12_000_000));
        assert_eq!(scale_target(&land, ScalePolicy::Force1080p), None);
        assert_eq!(scale_target(&port, ScalePolicy::Force1080p), None);
        // Force720p applies symmetrically.
        assert_eq!(
            scale_target(&land, ScalePolicy::Force720p),
            Some((1280, 720))
        );
        assert_eq!(
            scale_target(&port, ScalePolicy::Force720p),
            Some((720, 1280))
        );
    }

    #[test]
    fn odd_dimensions_get_even_shave() {
        // Old AVI/DivX habit: odd widths every encoder rejects.
        let mut m = file("mpeg4", 321, 240, 100_000_000, Some(1_000_000));
        assert_eq!(
            effective_scale_target(&m, ScalePolicy::Preserve),
            Some((320, 240))
        );
        m.width = 640;
        assert_eq!(effective_scale_target(&m, ScalePolicy::Preserve), None);
    }

    #[test]
    fn four_k_scales_to_cap_preserving_aspect() {
        let uhd = file("h264", 3840, 2160, 4_000_000_000, Some(30_000_000));
        assert_eq!(
            scale_target(&uhd, ScalePolicy::Force1080p),
            Some((1920, 1080))
        );
        // 4K portrait scales on the same short-side rule.
        let uhd_port = file("h264", 2160, 3840, 4_000_000_000, Some(30_000_000));
        assert_eq!(
            scale_target(&uhd_port, ScalePolicy::Force1080p),
            Some((1080, 1920))
        );
    }

    #[test]
    fn custom_scale_fits_inside_the_box() {
        // Landscape hits the width cap: 4000x3000 into 1920x1080.
        let land = file("h264", 4000, 3000, 1_000_000_000, Some(12_000_000));
        assert_eq!(
            scale_target(&land, ScalePolicy::Custom(1920, 1080)),
            Some((1440, 1080))
        );
        // Portrait hits the height cap instead (the preset short-side cap
        // would have left this file untouched — the box must not).
        let port = file("h264", 3000, 4000, 1_000_000_000, Some(12_000_000));
        assert_eq!(
            scale_target(&port, ScalePolicy::Custom(1920, 1080)),
            Some((810, 1080))
        );
        // Sources already inside the box are never upscaled.
        let small = file("h264", 1280, 720, 500_000_000, Some(6_000_000));
        assert_eq!(scale_target(&small, ScalePolicy::Custom(1920, 1080)), None);
        // Aspect is preserved on non-16:9 sources.
        let sq = file("h264", 2000, 2000, 1_000_000_000, Some(12_000_000));
        assert_eq!(
            scale_target(&sq, ScalePolicy::Custom(1000, 500)),
            Some((500, 500))
        );
        // Degenerate boxes are sanitized to something encodable.
        assert_eq!(
            scale_target(&land, ScalePolicy::Custom(0, 0)),
            Some((2, 2))
        );
    }

    #[test]
    fn scale_policy_parse_round_trips() {
        assert_eq!(ScalePolicy::parse("preserve"), Some(ScalePolicy::Preserve));
        assert_eq!(ScalePolicy::parse("720p"), Some(ScalePolicy::Force720p));
        assert_eq!(
            ScalePolicy::parse("1920x1080"),
            Some(ScalePolicy::Custom(1920, 1080))
        );
        assert_eq!(
            ScalePolicy::parse("1920X1080"),
            Some(ScalePolicy::Custom(1920, 1080))
        );
        assert_eq!(ScalePolicy::parse("bogus"), None);
        assert_eq!(ScalePolicy::parse("1920"), None);
        assert_eq!(ScalePolicy::parse("axb"), None);
    }

    #[test]
    fn tiny_audio_is_copied_not_grown() {
        let mut m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        m.audio[0].bitrate = Some(32_000);
        assert!(audio_copy_eligible(&m));
        let est = estimate_file(&m, &params());
        assert_eq!(est.audio_mode, "copy");
    }

    #[test]
    fn exotic_layout_falls_back_to_aac() {
        // 5.1(side): Opus mapping families reject it (measured failure).
        let mut m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        m.audio[0].bitrate = Some(384_000);
        m.audio[0].channels = 6;
        m.audio[0].layout = "5.1(side)".into();
        assert!(!opus_layout_ok(&m.audio[0]));
        let plan = plan_streams(&m, true, Some(DEFAULT_OPUS_BPS), false);
        assert!(!plan.audio_copy_eligible);
        assert_eq!(plan.audio_codec, AudioCodec::Aac(128_000));
        let est = estimate_file(&m, &params());
        assert_eq!(est.audio_mode, "aac128k");
        // 600 s × 16 kB/s = 9.6 MB of AAC.
        assert_eq!(est.new_audio_bytes, 9_600_000);
        // Standard layouts stay on Opus.
        let mut s = m.clone();
        s.audio[0].layout = "5.1".into();
        assert!(opus_layout_ok(&s.audio[0]));
        assert_eq!(
            plan_streams(&s, true, Some(DEFAULT_OPUS_BPS), false).audio_codec,
            AudioCodec::Opus(64_000)
        );
        s.audio[0].layout = String::new();
        assert!(opus_layout_ok(&s.audio[0]));
    }

    #[test]
    fn audio_choice_drives_plan_and_estimate() {
        // 384k stereo source: default 64k re-encodes, 96k costs more,
        // Off keeps the original untouched.
        let mut m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        m.audio[0].bitrate = Some(384_000);
        m.audio[0].channels = 2;
        m.audio[0].layout = "stereo".into();
        let p64 = plan_streams(&m, true, Some(64_000), false);
        assert_eq!(p64.audio_codec, AudioCodec::Opus(64_000));
        let p96 = plan_streams(&m, true, Some(96_000), false);
        assert_eq!(p96.audio_codec, AudioCodec::Opus(96_000));
        let poff = plan_streams(&m, true, None, false);
        assert_eq!(poff.audio_codec, AudioCodec::Copy);
        assert!(poff.audio_copy_eligible);
        let mut e96 = params();
        e96.opus_bps = Some(96_000);
        let est = estimate_file(&m, &e96);
        assert_eq!(est.audio_mode, "opus96k");
        // 600 s × 12 kB/s = 7.2 MB.
        assert_eq!(est.new_audio_bytes, 7_200_000);
        // A 48k source is copied at 64k target (would grow otherwise)…
        m.audio[0].bitrate = Some(48_000);
        assert_eq!(
            plan_streams(&m, true, Some(64_000), false).audio_codec,
            AudioCodec::Copy
        );
        // …but re-encoded when the choice is below it.
        assert_eq!(
            plan_streams(&m, true, Some(32_000), false).audio_codec,
            AudioCodec::Opus(32_000)
        );
    }

    #[test]
    fn audio_off_copies_everything() {
        // Regression: Off used to fall through to Opus(64k) for
        // unknown-bitrate non-Opus tracks (the common MKV case).
        let mut m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        m.audio[0].bitrate = None;
        m.audio[0].codec = "aac".into();
        let p = plan_streams(&m, true, None, false);
        assert_eq!(p.audio_codec, AudioCodec::Copy);
        let mut e = params();
        e.opus_bps = None;
        let est = estimate_file(&m, &e);
        assert_eq!(est.audio_mode, "copy");
        // …including exotic layouts (no layout check runs under Off).
        m.audio[0].layout = "5.1(side)".into();
        assert_eq!(
            plan_streams(&m, true, None, false).audio_codec,
            AudioCodec::Copy
        );
    }

    #[test]
    fn all_audio_sums_every_track() {
        // Dual-audio: tiny primary (copied) + fat secondary (re-encoded).
        let mut m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        m.audio[0].bitrate = Some(32_000);
        m.audio.push(AudioTrack {
            codec: "dts".into(),
            bitrate: Some(768_000),
            channels: 6,
            layout: "5.1".into(),
            sample_rate: Some(48000),
        });
        // First-only: secondary vanishes from plan and estimate.
        let mut p1 = params();
        let e1 = estimate_file(&m, &p1);
        assert_eq!(e1.audio_mode, "copy");
        // All tracks: copy + opus64k, bytes = src1 + 64k×600s/8.
        p1.all_audio = true;
        let est = estimate_file(&m, &p1);
        assert_eq!(est.audio_mode, "copy+opus64k");
        let src1 = 32_000.0 * 600.0 / 8.0;
        assert_eq!(est.new_audio_bytes, (src1 as u64) + 4_800_000);
        // Off keeps both untouched.
        let mut poff = params();
        poff.opus_bps = None;
        poff.all_audio = true;
        let est = estimate_file(&m, &poff);
        assert_eq!(est.audio_mode, "copy");
        let src2 = 768_000.0 * 600.0 / 8.0;
        assert_eq!(est.new_audio_bytes, (src1 as u64) + (src2 as u64));
    }

    #[test]
    fn preflight_honours_combined_model() {
        let m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        let mut p = params();
        p.cq = 32;
        p.scale = ScalePolicy::Force720p;
        match preflight(&m, &p, 10.0) {
            Preflight::Shrink { reason } => {
                assert!(reason.contains("1280x720"), "reason: {reason}");
                assert!(reason.contains("cq32"), "reason: {reason}");
            }
            Preflight::Skip { reason } => panic!("should shrink: {reason}"),
        }
    }

    #[test]
    fn efficient_codecs_let_the_estimator_overrule() {
        // Regression: low-bitrate AV1 with SVT-AV1 + downscale selected
        // predicts ~50% off — the old codec-alone fast path skipped it as
        // "negligible" without consulting the estimate.
        let m = file("av1", 1520, 1080, 130_000_000, Some(1_600_000));
        let mut crunch = params();
        crunch.backend = VideoBackend::CpuAv1;
        crunch.cq = 28;
        crunch.scale = ScalePolicy::Force720p;
        match preflight(&m, &crunch, 10.0) {
            Preflight::Shrink { reason } => assert!(
                reason.contains("54%") || reason.contains("53%") || reason.contains("off"),
                "reason: {reason}"
            ),
            Preflight::Skip { reason } => panic!("should shrink: {reason}"),
        }
        // Same file at defaults (NVENC, preserve): genuinely negligible.
        match preflight(&m, &params(), 10.0) {
            Preflight::Skip { .. } => {}
            Preflight::Shrink { reason } => panic!("should skip: {reason}"),
        }
    }

    #[test]
    fn dense_grainy_source_predicts_less_saving() {
        // Same file, same settings — only the source bitrate differs.
        // 20 Mbps noisy 1080p must predict a clearly larger output than
        // a 3 Mbps typical one (density correction).
        let fat = file("h264", 1920, 1080, 2_500_000_000, Some(20_000_000));
        let lean = file("h264", 1920, 1080, 400_000_000, Some(3_000_000));
        let p = params();
        let e_fat = estimate_file(&fat, &p);
        let e_lean = estimate_file(&lean, &p);
        assert!(e_fat.density < e_lean.density);
        assert!(
            e_fat.ratio > e_lean.ratio + 0.1,
            "fat ratio {} vs lean {}",
            e_fat.ratio,
            e_lean.ratio
        );
    }

    #[test]
    fn container_bitrate_fills_in_missing_stream_rate() {
        // MKV with bit_rate=N/A on the video stream: the container rate
        // minus audio must drive density, not the 1.0 fallback.
        let mut fat = file("h264", 1920, 1080, 2_500_000_000, None);
        fat.format_bitrate = Some(20_128_000); // ≈20 Mbps video + 128k audio
        assert_eq!(effective_vbitrate(&fat), Some(20_000_000));
        let mut lean = file("h264", 1920, 1080, 400_000_000, None);
        lean.format_bitrate = Some(3_128_000);
        let p = params();
        let e_fat = estimate_file(&fat, &p);
        let e_lean = estimate_file(&lean, &p);
        assert!(
            e_fat.density < 1.0,
            "container-derived density should correct, got {}",
            e_fat.density
        );
        assert!(
            e_fat.ratio > e_lean.ratio + 0.1,
            "fat ratio {} vs lean {}",
            e_fat.ratio,
            e_lean.ratio
        );
        // Fully unknown bitrate stays permissive.
        let unknown = file("h264", 1920, 1080, 1_000_000_000, None);
        assert_eq!(effective_vbitrate(&unknown), None);
    }
}

/// Copy (don't re-encode) audio when it is already at/below the chosen
/// target, in ANY codec: re-encoding can't shrink it, it only costs quality
/// and time (forcing a bitrate onto a smaller source would even GROW it).
/// Unknown-bitrate Opus is assumed efficient and copied; unknown-bitrate
/// anything else is re-encoded since we can't prove it's small.
pub fn audio_copy_eligible_for(m: &MediaFile, target_bps: u32) -> bool {
    match m.primary_audio() {
        Some(a) => audio_copy_eligible_for_track(a, target_bps),
        None => false,
    }
}

/// Per-track version of the rule above.
pub fn audio_copy_eligible_for_track(a: &crate::media::AudioTrack, target_bps: u32) -> bool {
    const TOL: u64 = 8_000;
    match a.bitrate {
        Some(b) => b <= target_bps as u64 + TOL,
        None => a.codec.eq_ignore_ascii_case("opus"),
    }
}

/// Default-target version (64k + tolerance), kept for tests/callers that
/// don't carry a setting.
pub fn audio_copy_eligible(m: &MediaFile) -> bool {
    audio_copy_eligible_for(m, DEFAULT_OPUS_BPS)
}
