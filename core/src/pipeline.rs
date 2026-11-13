//! Pipeline selection: preflight, resolution policy and fallback hierarchy.
//!
//! Fallback hierarchy (runtime capability driven), NVIDIA box:
//!
//! 1. NVDEC/CUDA hardware decode + HEVC NVENC (zero-copy GPU path)
//! 2. Hardware decode + H.264 NVENC (HEVC unavailable)
//! 3. CPU decode + NVENC (HW decode failed, encoder works)
//! 4. CPU x264 (final fallback; x265 kept as legacy option)
//!
//! On machines without NVENC the ladder continues through the other vendor
//! families before reaching the CPU: VideoToolbox (Apple) → Quick Sync
//! (Intel) → AMF (AMD) → x264. Auto stays HEVC-first for playback
//! compatibility; the AV1 hardware encoders are reached through the
//! Balanced/Smallest goals or explicit backend selection.
//!
//! Encode levels that consume system-memory frames (CPU encoders and the
//! VT/QSV/AMF families, which upload internally) get a `-hwaccel auto`
//! decode assist, so NVDEC/QSV/VideoToolbox decode feeds them when
//! available — the encoder sees identical frames, only the wall clock
//! improves.
//!
//! The [`VideoBackend`] / [`NvencPreset`] / [`ScalePolicy`] types are the
//! knobs the UI and benchmark matrix drive. [`select_levels`] turns a mode
//! plus [`HwCaps`] into the ordered level list to attempt.

use crate::hw::HwCaps;
use crate::media::MediaFile;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoBackend {
    /// Ordered fallback ladder driven by [`HwCaps`] across all families.
    Auto,
    /// AV1 NVENC (RTX 40/50): ~20–30% smaller than HEVC NVENC at the same
    /// speed — the free-savings engine on Ada+. Hidden unless the trial
    /// encode passed.
    Av1Nvenc,
    HevcNvenc,
    H264Nvenc,
    /// Apple VideoToolbox HEVC (macOS release builds; the encoder exists
    /// only there and its `-q:v` quality mode is Apple-Silicon-only, which
    /// matches the shipped target).
    VtHevc,
    /// Intel Quick Sync HEVC.
    QsvHevc,
    /// Intel Quick Sync AV1 (Arc): hidden unless the ICQ trial passed.
    QsvAv1,
    /// AMD AMF HEVC.
    AmfHevc,
    /// AMD AMF AV1 (RX 7000+): hidden unless the CQP trial passed.
    AmfAv1,
    /// Baseline for benchmarks: libx264 CRF 28.
    CpuX264,
    /// Legacy CPU path (kept working, not the default).
    CpuX265,
    /// Max-shrink CPU path: SVT-AV1 (preset from the shared P-knob, CRF =
    /// the CQ slider value). Software encode on the CPU; needs libsvtav1.
    CpuAv1,
    /// Audio-only pass: video stream-copied untouched (any codec), audio
    /// converted per the audio setting (the target is an instruction, so
    /// this is the audio-upscale mode), subs copied, everything else
    /// dropped. No quality slider, preset, or resolution applies — speed
    /// of a remux; the output may legitimately grow when the target is
    /// above the source bitrate, and size thresholds don't apply.
    CopyVideo,
}

impl VideoBackend {
    pub fn label(&self) -> &'static str {
        match self {
            VideoBackend::Auto => "Auto (best HW encoder → CPU)",
            VideoBackend::Av1Nvenc => "NVENC AV1 (RTX 40+, GPU decode)",
            VideoBackend::HevcNvenc => "NVENC HEVC (prefer GPU decode)",
            VideoBackend::H264Nvenc => "NVENC H.264 (compat/fallback)",
            VideoBackend::VtHevc => "VideoToolbox HEVC (Apple GPU)",
            VideoBackend::QsvHevc => "Quick Sync HEVC (Intel GPU)",
            VideoBackend::QsvAv1 => "Quick Sync AV1 (Intel Arc)",
            VideoBackend::AmfHevc => "AMF HEVC (AMD GPU)",
            VideoBackend::AmfAv1 => "AMF AV1 (AMD RX 7000+)",
            VideoBackend::CpuX264 => "CPU x264 (CRF = quality slider)",
            VideoBackend::CpuX265 => "CPU x265 slow (CRF = quality slider)",
            VideoBackend::CpuAv1 => "CPU AV1 SVT (max shrink, slow)",
            VideoBackend::CopyVideo => "Copy video (audio-only — upscale audio)",
        }
    }

    /// True when the CQ slider means SVT-AV1 CRF rather than NVENC CQ.
    pub fn cq_is_av1_crf(&self) -> bool {
        matches!(self, VideoBackend::CpuAv1)
    }

    /// True when this mode encodes HEVC/AV1 through one of the hardware
    /// families that consume system-memory frames (upload inside the
    /// encoder). Used by the estimator to pick the family's size table.
    pub fn is_hw_upload_family(&self) -> bool {
        matches!(
            self,
            VideoBackend::VtHevc | VideoBackend::QsvHevc | VideoBackend::AmfHevc
        )
    }
}

/// Shared encoder-preset knob. P3–P6 are the benchmark matrix. P7–P9
/// exist for SVT-AV1's slower presets (6/5/4): the software encoder keeps
/// buying a few percent at each step, which is exactly what the Smallest
/// goal is for. Hardware encoders clamp P8/P9 to NVENC's own P7, the
/// slowest preset their API accepts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NvencPreset {
    P3,
    P4,
    P5,
    P6,
    P7,
    P8,
    P9,
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
            NvencPreset::P7 | NvencPreset::P8 | NvencPreset::P9 => "p7",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            NvencPreset::P3 => "P3 (fast)",
            NvencPreset::P4 => "P4 (medium, NVENC default)",
            NvencPreset::P5 => "P5 (slow, good quality)",
            NvencPreset::P6 => "P6 (slower, better quality)",
            NvencPreset::P7 => "P7 (SVT 6, much slower)",
            NvencPreset::P8 => "P8 (SVT 5, very slow)",
            NvencPreset::P9 => "P9 (SVT 4, overnight)",
        }
    }

    /// The same knob mapped onto SVT-AV1's preset scale (1–13, lower =
    /// slower & smaller). P5 — the app default — is SVT preset 8, the
    /// established sweet spot; each P step moves SVT by one preset, so
    /// the shared control is a speed↔size lever on the CPU path too.
    pub fn svt(&self) -> u8 {
        match self {
            NvencPreset::P3 => 10,
            NvencPreset::P4 => 9,
            NvencPreset::P5 => 8,
            NvencPreset::P6 => 7,
            NvencPreset::P7 => 6,
            NvencPreset::P8 => 5,
            NvencPreset::P9 => 4,
        }
    }

    /// Mapped onto Quick Sync's named presets.
    pub fn qsv(&self) -> &'static str {
        match self {
            NvencPreset::P3 => "veryfast",
            NvencPreset::P4 => "faster",
            NvencPreset::P5 => "medium",
            NvencPreset::P6 => "slow",
            NvencPreset::P7 | NvencPreset::P8 | NvencPreset::P9 => "slower",
        }
    }

    /// Mapped onto AMF's quality-speed modes.
    pub fn amf(&self) -> &'static str {
        match self {
            NvencPreset::P3 => "speed",
            NvencPreset::P4 => "balanced",
            NvencPreset::P5
            | NvencPreset::P6
            | NvencPreset::P7
            | NvencPreset::P8
            | NvencPreset::P9 => "quality",
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
    /// Zero-copy CUDA path + av1_nvenc (RTX 40/50).
    HwAv1,
    /// CPU decode + hevc_nvenc upload.
    CpuDecodeHevcNvenc,
    /// CPU decode + h264_nvenc upload.
    CpuDecodeH264Nvenc,
    /// CPU decode + av1_nvenc upload.
    CpuDecodeAv1Nvenc,
    /// Apple VideoToolbox HEVC (`-q:v` quality, uploads internally).
    VtHevc,
    /// Apple VideoToolbox H.264.
    VtH264,
    /// Intel Quick Sync HEVC (ICQ via `-global_quality`).
    QsvHevc,
    /// Intel Quick Sync H.264.
    QsvH264,
    /// Intel Quick Sync AV1 (Arc).
    QsvAv1,
    /// AMD AMF HEVC (CQP).
    AmfHevc,
    /// AMD AMF H.264.
    AmfH264,
    /// AMD AMF AV1 (RX 7000+).
    AmfAv1,
    /// libx264 fallback.
    CpuX264,
    /// libx265 legacy.
    CpuX265,
    /// SVT-AV1 software encode (preset from the P-knob, CRF = CQ slider).
    CpuAv1,
    /// Stream-copy video untouched (any input codec).
    CopyVideo,
}

impl Level {
    pub fn encoder_name(&self) -> &'static str {
        match self {
            Level::HwHevc | Level::CpuDecodeHevcNvenc => "hevc_nvenc",
            Level::HwH264 | Level::CpuDecodeH264Nvenc => "h264_nvenc",
            Level::HwAv1 | Level::CpuDecodeAv1Nvenc => "av1_nvenc",
            Level::VtHevc => "hevc_videotoolbox",
            Level::VtH264 => "h264_videotoolbox",
            Level::QsvHevc => "hevc_qsv",
            Level::QsvH264 => "h264_qsv",
            Level::QsvAv1 => "av1_qsv",
            Level::AmfHevc => "hevc_amf",
            Level::AmfH264 => "h264_amf",
            Level::AmfAv1 => "av1_amf",
            Level::CpuX264 => "libx264",
            Level::CpuX265 => "libx265",
            Level::CpuAv1 => "libsvtav1",
            Level::CopyVideo => "copy",
        }
    }
    /// True for the explicit zero-copy CUDA levels (decode stays on the GPU).
    pub fn uses_hw_decode(&self) -> bool {
        matches!(self, Level::HwHevc | Level::HwH264 | Level::HwAv1)
    }
    pub fn uses_nvenc(&self) -> bool {
        matches!(
            self,
            Level::HwHevc
                | Level::HwH264
                | Level::HwAv1
                | Level::CpuDecodeHevcNvenc
                | Level::CpuDecodeH264Nvenc
                | Level::CpuDecodeAv1Nvenc
        )
    }
}

/// The NVENC-family ladder (shared by Auto and the explicit NVENC modes):
/// zero-copy first when CUDA decode exists, then CPU-decode NVENC uploads.
fn nvenc_hevc_ladder(caps: &HwCaps) -> Vec<Level> {
    let mut v = vec![];
    if caps.cuda_hwaccel && caps.hevc_nvenc {
        v.push(Level::HwHevc);
    }
    if caps.hevc_nvenc {
        v.push(Level::CpuDecodeHevcNvenc);
    }
    v
}

fn nvenc_h264_ladder(caps: &HwCaps) -> Vec<Level> {
    let mut v = vec![];
    if caps.cuda_hwaccel && caps.h264_nvenc {
        v.push(Level::HwH264);
    }
    if caps.h264_nvenc {
        v.push(Level::CpuDecodeH264Nvenc);
    }
    v
}

/// The non-NVIDIA hardware families in Auto's preference order. Mutually
/// exclusive on real machines in practice; when more than one exists
/// (Intel iGPU + NVIDIA dGPU), NVENC's own ladder already ran first.
fn other_families_hevc_ladder(caps: &HwCaps) -> Vec<Level> {
    let mut v = vec![];
    if caps.vt_hevc {
        v.push(Level::VtHevc);
    }
    if caps.vt_h264 {
        v.push(Level::VtH264);
    }
    if caps.qsv_hevc {
        v.push(Level::QsvHevc);
    }
    if caps.qsv_h264 {
        v.push(Level::QsvH264);
    }
    if caps.amf_hevc {
        v.push(Level::AmfHevc);
    }
    if caps.amf_h264 {
        v.push(Level::AmfH264);
    }
    v
}

/// Ordered attempt list for a mode + detected caps.
pub fn select_levels(mode: VideoBackend, caps: &HwCaps) -> Vec<Level> {
    match mode {
        VideoBackend::Auto => {
            // HEVC-first for playback compatibility: every TV/phone decodes
            // HEVC, AV1 support is still spottier. AV1 hardware is reached
            // through the Balanced/Smallest goals or explicit selection.
            let mut v = nvenc_hevc_ladder(caps);
            v.extend(nvenc_h264_ladder(caps));
            v.extend(other_families_hevc_ladder(caps));
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::Av1Nvenc => {
            let mut v = vec![];
            if caps.av1_nvenc_usable {
                if caps.cuda_hwaccel {
                    v.push(Level::HwAv1);
                }
                v.push(Level::CpuDecodeAv1Nvenc);
            }
            // Encoder unusable (or its HW decode failed): keep the GPU busy
            // with NVENC HEVC rather than dropping straight to the CPU.
            v.extend(nvenc_hevc_ladder(caps));
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::HevcNvenc => {
            let mut v = nvenc_hevc_ladder(caps);
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::H264Nvenc => {
            let mut v = nvenc_h264_ladder(caps);
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::VtHevc => {
            let mut v = vec![];
            if caps.vt_hevc {
                v.push(Level::VtHevc);
            }
            if caps.vt_h264 {
                v.push(Level::VtH264);
            }
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::QsvHevc => {
            let mut v = vec![];
            if caps.qsv_hevc {
                v.push(Level::QsvHevc);
            }
            if caps.qsv_h264 {
                v.push(Level::QsvH264);
            }
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::QsvAv1 => {
            let mut v = vec![];
            if caps.qsv_av1_usable {
                v.push(Level::QsvAv1);
            }
            if caps.qsv_hevc {
                v.push(Level::QsvHevc);
            }
            if caps.qsv_h264 {
                v.push(Level::QsvH264);
            }
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::AmfHevc => {
            let mut v = vec![];
            if caps.amf_hevc {
                v.push(Level::AmfHevc);
            }
            if caps.amf_h264 {
                v.push(Level::AmfH264);
            }
            v.push(Level::CpuX264);
            v
        }
        VideoBackend::AmfAv1 => {
            let mut v = vec![];
            if caps.amf_av1_usable {
                v.push(Level::AmfAv1);
            }
            if caps.amf_hevc {
                v.push(Level::AmfHevc);
            }
            if caps.amf_h264 {
                v.push(Level::AmfH264);
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

/// What preflight does when an estimate lands below `min_saving_pct`.
///
/// The GUI exposes this as the "Skip already-efficient files" checkbox:
/// checked means below-threshold files are skipped; unchecked means they
/// are still queued, but flagged as warnings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThresholdPolicy {
    /// Skip estimates below the threshold (classic preflight).
    Skip,
    /// Queue below-threshold estimates anyway; the plan warns instead.
    Warn,
}

#[derive(Clone, Debug)]
pub enum Preflight {
    Shrink {
        reason: String,
    },
    /// Eligible, but the estimate falls short of the configured threshold.
    /// Only produced under [`ThresholdPolicy::Warn`].
    Warn {
        reason: String,
    },
    Skip {
        reason: String,
    },
}

impl Preflight {
    /// True when the file is part of the run (shrink normally or warned).
    pub fn will_shrink(&self) -> bool {
        !matches!(self, Preflight::Skip { .. })
    }

    /// True when the file runs despite a below-threshold estimate.
    pub fn is_warn(&self) -> bool {
        matches!(self, Preflight::Warn { .. })
    }

    pub fn reason(&self) -> &str {
        match self {
            Preflight::Shrink { reason }
            | Preflight::Warn { reason }
            | Preflight::Skip { reason } => reason,
        }
    }
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
    /// Conservative video encode, protected audio, and a measured quality
    /// check before placement. The GUI searches stronger settings from
    /// samples and uses `cq` only as the fallback estimate/retry value.
    pub preserve_quality: bool,
    pub backend: VideoBackend,
    pub preset: NvencPreset,
    pub cq: u32,
    pub scale: ScalePolicy,
    /// Opus target in bps (`Some(64000)` default). `None` = Off: keep
    /// every audio track as-is (stream copy, never re-encoded).
    pub opus_bps: Option<u32>,
    /// Keep every audio track (dual-audio releases). Callers choose the
    /// default; the GUI and CLI start with every track kept.
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
    /// SVT-AV1 film-grain synthesis: frame analysis measures actual grain in
    /// flat areas (bitrate density alone is not evidence), then short trial
    /// encodes choose a denoise strength whose regenerated grain still
    /// matches the source's texture. Clean sources stay off. The grain is
    /// statistically similar, not identical, which is why this stays a
    /// visible switch.
    pub film_grain: bool,
}

impl EstParams {
    /// Resolve protection per source so estimates and encodes use the same
    /// audio decision. An unsafe target falls back to copying every track.
    pub fn for_media(&self, m: &MediaFile) -> Self {
        let mut p = *self;
        if p.preserve_quality {
            p.cq = crate::quality::VIDEO_CQ;
            p.all_audio = true;
            p.film_grain = false;
            p.opus_bps = crate::quality::audio_target(m, p.opus_bps, p.backend);
        }
        p
    }
}

/// Default Opus target: transparent-ish stereo, lean 5.1.
pub const DEFAULT_OPUS_BPS: u32 = 64_000;

/// Highest fixed target the UI/CLI exposes. libopus allows 256 kbps per
/// channel, so this is the ceiling for stereo (the common case); mono is
/// clamped down by [`opus_bps_for_track`]. Going higher would be a no-op
/// on stereo and make the preset labels lie about the real bitrate.
pub const MAX_OPUS_BPS: u32 = 512_000;

/// libopus's per-channel bitrate ceiling (FFmpeg enforces 256k/channel).
pub const OPUS_BPS_PER_CHANNEL: u32 = 256_000;

/// Opus targets offered by the GUI, in kbps (the CLI accepts any 8–512).
/// The top presets are for fat lossless/DTS sources; stereo is already
/// transparent well below them, so the extra headroom mainly helps
/// multichannel tracks and avoids needless generational loss.
pub const OPUS_TARGETS_KBPS: [u32; 11] = [32, 48, 64, 96, 128, 192, 256, 320, 384, 448, 512];

/// Effective Opus bitrate for one track: the chosen target capped at
/// libopus's per-channel limit. A mono track cannot take the stereo-max
/// 512k preset — FFmpeg rejects an over-cap `-b:a` outright.
pub fn opus_bps_for_track(t: &crate::media::AudioTrack, target: u32) -> u32 {
    if t.channels == 0 {
        // Unknown channel count: trust the target rather than assuming
        // mono and needlessly halving it.
        return target;
    }
    target.min(u32::from(t.channels).saturating_mul(OPUS_BPS_PER_CHANNEL))
}

/// AAC fallback bitrate for a given Opus target: AAC needs roughly twice
/// the bits for similar quality (floored/ceilinged to sane bounds).
pub fn aac_bps_for(opus_bps: u32) -> u32 {
    (opus_bps * 2).clamp(96_000, 320_000)
}

/// True for an audio-only pass with a bitrate target: video is copied and
/// the audio is re-encoded at the chosen bitrate even when the source is
/// already at/below it. In every other mode a "copy" decision is the
/// smarter default; here the target is an instruction, not a ceiling.
pub fn forces_audio_reencode(backend: VideoBackend, opus_bps: Option<u32>) -> bool {
    backend == VideoBackend::CopyVideo && opus_bps.is_some()
}

/// Human label for an estimated size change: direction, byte delta and
/// percentage in one phrase. Growing estimates read `250 MB larger (+25%)`
/// instead of a confusing negative "saving"; shrinking ones read
/// `250 MB smaller (−25%)`.
pub fn size_change_label(input_bytes: u64, output_bytes: u64) -> String {
    if output_bytes == input_bytes {
        return "no size change".to_string();
    }
    let delta = output_bytes.abs_diff(input_bytes);
    let pct = delta as f64 / input_bytes.max(1) as f64 * 100.0;
    // Don't round a real change down to the misleading "0%".
    let pct = if pct < 0.5 {
        "<1%".to_string()
    } else {
        format!("{pct:.0}%")
    };
    if output_bytes > input_bytes {
        format!("{} larger (+{pct})", crate::media::human_bytes(delta))
    } else {
        format!("{} smaller (−{pct})", crate::media::human_bytes(delta))
    }
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

/// SVT-AV1's CRF scale is stricter than the hardware/NVENC CQ scale: the
/// same number buys more quality and a bigger file. Calibrated on 720p
/// sources, NVENC HEVC CQ 32 lands near SVT CRF 44, so libsvtav1 gets the
/// shared CQ plus this offset. The UI slider stays 18–40 and every
/// estimate stays in shared-CQ units; only the AV1 encoder call (and the
/// label it reports) carries the mapped CRF.
pub const SVT_CRF_OFFSET: u32 = 12;

/// Map the shared 18–40 quality slider to libsvtav1's CRF.
pub fn svt_crf(cq: u32) -> u32 {
    cq.saturating_add(SVT_CRF_OFFSET)
}

/// CRF steps a max-shrink SVT encode climbs when its first result is not
/// smaller than the source. Only [`VideoBackend::CpuAv1`] retries: the
/// other presets describe a user-chosen quality and must keep it rather
/// than silently degrade the picture.
pub const GAIN_RETRY_STEP: u32 = 4;
/// Highest shared CQ the slider (and therefore a gain retry) may reach.
/// Kept equal to the UI ceiling so a retry never lands on a setting the
/// user could not pick by hand. On SVT that maps to CRF 52.
pub const MAX_GAIN_CQ: u32 = 40;
/// At most this many no-gain retries per file (shared CQ 32 → 36 → 40,
/// i.e. SVT CRF 44 → 48 → 52, on the Smallest preset). Each retry is a
/// full encode, so the cap matters.
pub const MAX_GAIN_RETRIES: u32 = 3;
/// Reference SVT CRF for a source that is already efficient in a modern
/// codec. A lean HEVC source the app itself wrote (NVENC CQ 28) sits near
/// SVT CRF 36. Estimating below that quality promises savings a
/// second-generation encode cannot deliver — the source is already at the
/// efficient frontier. The estimator works in shared-slider units, so the
/// reference is stored there (CRF 36 = CQ 24 with [`SVT_CRF_OFFSET`]).
const LEAN_REENCODE_REFERENCE_CQ: u32 = 36 - SVT_CRF_OFFSET;

/// Next CRF to try after an encode failed to beat its source, or `None`
/// when this backend should report no gain instead. Only the max-shrink
/// SVT path retries; preserve-quality owns its own retry policy, and
/// every other backend keeps the quality the user selected.
pub fn gain_retry_cq(
    backend: VideoBackend,
    cq: u32,
    preserve_quality: bool,
    retries: u32,
) -> Option<u32> {
    if preserve_quality || backend != VideoBackend::CpuAv1 || retries >= MAX_GAIN_RETRIES {
        return None;
    }
    let next = cq.saturating_add(GAIN_RETRY_STEP).min(MAX_GAIN_CQ);
    (next > cq).then_some(next)
}

/// Preset multiplier relative to P5 (the default). Fast presets spend
/// more bits for the same CQ; P6 squeezes a little more. Applies to the
/// NVENC P-series and (via the mapped named presets) Quick Sync; the
/// VideoToolbox and AMF families have no preset axis of their own.
/// SVT has its own preset axis, calibrated from 720p encodes: preset 8
/// (P5) comes out ~12% larger than preset 6 (P7), which is the Smallest
/// reference at 1.0.
pub fn preset_factor(backend: VideoBackend, preset: NvencPreset) -> f64 {
    match backend {
        VideoBackend::CpuAv1 => match preset {
            NvencPreset::P3 => 1.22,
            NvencPreset::P4 => 1.18,
            NvencPreset::P5 => 1.12,
            NvencPreset::P6 => 1.06,
            NvencPreset::P7 => 1.0,
            NvencPreset::P8 => 0.97,
            NvencPreset::P9 => 0.94,
        },
        VideoBackend::CpuX264
        | VideoBackend::CpuX265
        | VideoBackend::CopyVideo
        | VideoBackend::VtHevc
        | VideoBackend::AmfHevc
        | VideoBackend::AmfAv1 => 1.0,
        _ => match preset {
            NvencPreset::P3 => 1.12,
            NvencPreset::P4 => 1.04,
            NvencPreset::P5 => 1.0,
            NvencPreset::P6 => 0.96,
            // Hardware clamps the SVT-only settings to its slowest preset;
            // the estimate follows the filename, not a knob the encoder
            // cannot honour.
            NvencPreset::P7 | NvencPreset::P8 | NvencPreset::P9 => 0.94,
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
        VideoBackend::CpuAv1 => format!("svt-av1 p{} crf{}", p.preset.svt(), svt_crf(p.cq)),
        VideoBackend::CopyVideo => "copy video".into(),
        VideoBackend::VtHevc => format!("vt-hevc cq{}", p.cq),
        VideoBackend::QsvHevc => format!("qsv-hevc icq{} {}", p.cq, p.preset.qsv()),
        VideoBackend::QsvAv1 => format!("qsv-av1 icq{}", p.cq),
        VideoBackend::AmfHevc => format!("amf-hevc qp{} {}", p.cq, p.preset.amf()),
        VideoBackend::AmfAv1 => format!("amf-av1 qp{}", p.cq),
        _ => format!("cq{} {}", p.cq, p.preset.as_str()),
    }
}

/// Full-file estimate: video part (factored) + audio part (copy or 64k
/// Opus), split via stream bitrate when known.
pub fn estimate_file(m: &MediaFile, p: &EstParams) -> FileEstimate {
    let resolved = p.for_media(m);
    let p = &resolved;
    let density = source_density(m);
    // Film-grain synthesis denoises before encoding, so a dense/grainy
    // source stops behaving like one: the density penalty (which exists
    // because grain is brutally expensive to encode) no longer applies in
    // full. Floor the modeled density at 0.8 — optimistic beyond that,
    // since the denoiser still leaves real structure to encode.
    let eff_density = if fgs_applies(m, p.backend, p.film_grain) {
        density.max(0.8)
    } else {
        density
    };
    // CQ-mode output size is content-driven: grainy/dense sources cost
    // nearly as many bits in HEVC as they did in H.264, while clean
    // sources collapse. The base table assumes typical density, so scale
    // the *saving* by observed density (measured: 8 Mbps noisy 1080p saved
    // ~1% at CQ28 where the raw table predicted 25%).
    let base = 1.0 - (1.0 - est_ratio(&m.vcodec, p.backend)) * eff_density;
    // CQ/CRF curve around the 28 reference. Tables above are calibrated at
    // the shared CQ 28 for every backend; libsvtav1 receives CQ + 12, so
    // its table is anchored to that mapped CRF and the same ≈7%-per-point
    // factor keeps the slider comparable across encoders.
    let cf = cq_factor(p.cq);
    let pf = preset_factor(p.backend, p.preset);
    let sf = scale_factor(m, p.scale);
    let mut video_factor = base * cf * pf * sf;
    // A lean modern-codec source is already at the efficient frontier:
    // re-encoding it at a CRF below its own quality level grows the file.
    // Model that floor, or the plan promises a saving the encode cannot
    // deliver (a 0.9 Mbps 720p HEVC file promised 154 → 83 MB and grew).
    if lean_reencode_floor_applies(m, p, density, sf) {
        // The floor is the frontier at this preset: a faster SVT preset
        // spends more bits for the same CRF, so its minimum output is
        // proportionally larger (P5 is ~12% above the P7 reference).
        let floor = pf * cf / cq_factor(LEAN_REENCODE_REFERENCE_CQ);
        video_factor = video_factor.max(floor);
    }
    let scaled_to = effective_scale_target(m, p.scale);
    let plan = plan_streams_with(
        m,
        true,
        p.opus_bps,
        p.all_audio,
        forces_audio_reencode(p.backend, p.opus_bps),
    );
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
    // Source bytes held by audio. `audio_src_bytes` guesses when ffprobe
    // reports no stream rate, which is the norm in MKV — including every
    // file Shrinkr itself wrote.
    let audio_src_total: u64 = if dur > 0.0 {
        m.audio.iter().map(|t| audio_src_bytes(t, dur)).sum()
    } else {
        0
    };
    let new_bytes = if p.backend == VideoBackend::CopyVideo {
        // Video passes through byte-identical: source video bytes plus the
        // planned audio. Source video = stream rate when known, else total
        // minus ALL source audio (kept or dropped — dropped tracks vanish
        // from the output too).
        let vbytes = match m.vbitrate {
            Some(vbr) if dur > 0.0 => (vbr as f64 * dur / 8.0) as u64,
            _ => m.bytes.saturating_sub(audio_src_total),
        };
        vbytes.saturating_add(new_audio_bytes)
    } else if dur > 0.0 {
        // Video is the source minus its own audio — not a flat 90% of the
        // file — because the audio term is added back below. A flat share
        // double-counted audio, so any file whose video stream rate
        // ffprobe omits (i.e. every file Shrinkr wrote) estimated a bigger
        // output than its own source and warned "re-encoding would grow
        // it" no matter what the audio target was.
        let vbytes = match m.vbitrate {
            Some(vbr) => vbr as f64 * dur / 8.0,
            None => m.bytes.saturating_sub(audio_src_total) as f64,
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

/// True when the estimate must assume the source has no cheap quality
/// headroom left: SVT-AV1 is re-encoding a lean HEVC/AV1/VP9 file at its
/// native resolution. Older codecs still collapse even when lean (their
/// encoders were less efficient), and downscaling creates real savings,
/// so neither gets the floor.
fn lean_reencode_floor_applies(m: &MediaFile, p: &EstParams, density: f64, scale: f64) -> bool {
    p.backend == VideoBackend::CpuAv1
        && !p.preserve_quality
        && matches!(m.vcodec.as_str(), "hevc" | "av1" | "vp9")
        && density >= 1.0
        // `density` defaults to 1.0 when the bitrate is unknown; that is
        // not evidence of leanness, so only floor a measured source.
        && m.fps > 0.1
        && effective_vbitrate(m).is_some()
        && scale >= 0.999
}

/// True when the source models as dense/grainy — the files the density
/// correction punishes. This is only the size-model hint; the actual encode
/// analyzes frames and measures a grain strength (see `grain.rs`).
pub(crate) fn is_dense_source(m: &MediaFile) -> bool {
    source_density(m) < 0.7
}

/// Size-model hint used by the estimate: the source is dense enough that
/// film-grain synthesis *may* help. The encode still measures actual grain
/// and the chosen strength before applying it, so a dense but clean source
/// stays untouched.
pub(crate) fn fgs_applies(m: &MediaFile, backend: VideoBackend, film_grain: bool) -> bool {
    film_grain && backend == VideoBackend::CpuAv1 && is_dense_source(m)
}

/// Decide shrink vs skip.
///
/// * `min_saving_pct` is the configurable threshold (e.g. 10 → skip when
///   the estimated saving is below 10%).
pub fn preflight(m: &MediaFile, p: &EstParams, min_saving_pct: f64) -> Preflight {
    preflight_with(m, p, min_saving_pct, ThresholdPolicy::Skip)
}

/// Turn a below-threshold verdict into a skip or a warning. The warning
/// appends what actually happened so the plan row explains itself.
pub(crate) fn below_threshold(reason: String, policy: ThresholdPolicy) -> Preflight {
    match policy {
        ThresholdPolicy::Skip => Preflight::Skip { reason },
        ThresholdPolicy::Warn => Preflight::Warn {
            // The placement guard drops any output that isn't actually
            // smaller, so say that up front: a growth estimate still runs,
            // its result is just discarded unless the encode beats the
            // source.
            reason: format!("{reason} — warned, running anyway (kept only if it shrinks)"),
        },
    }
}

/// [`preflight`] with an explicit below-threshold policy: [`ThresholdPolicy::Warn`]
/// keeps low-gain files in the run with a warning instead of skipping them.
pub fn preflight_with(
    m: &MediaFile,
    p: &EstParams,
    min_saving_pct: f64,
    policy: ThresholdPolicy,
) -> Preflight {
    use crate::media::human_bytes;
    let resolved = p.for_media(m);
    let p = &resolved;
    // Corrupt / undecodable inputs are rejected by probe already; a missing
    // video stream here means skip, not crash.
    if m.vcodec == "unknown" || m.width == 0 {
        return Preflight::Skip {
            reason: "no video stream found".into(),
        };
    }
    // Audio-only pass: video is copied byte-for-byte and the audio is
    // re-encoded at the chosen bitrate, so the file may legitimately grow.
    // Size thresholds describe shrinking, which is not what was asked for —
    // skipping here would silently ignore "re-encode the audio to 192k".
    if forces_audio_reencode(p.backend, p.opus_bps) {
        let est = estimate_file(m, p);
        let plan = plan_streams_with(m, true, p.opus_bps, p.all_audio, true);
        if !plan.has_audio {
            return Preflight::Skip {
                reason: "audio-only pass, but this file has no audio track".into(),
            };
        }
        return Preflight::Shrink {
            reason: format!(
                "audio-only: video copied, audio {} (est. {} → {}, {})",
                audio_mode_label(m, &plan),
                human_bytes(m.bytes),
                human_bytes(est.new_bytes),
                size_change_label(m.bytes, est.new_bytes),
            ),
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
            if est.new_bytes >= m.bytes || saving < min_saving_pct {
                let outcome = if est.new_bytes > m.bytes {
                    "re-encoding would grow it"
                } else {
                    "negligible gain expected"
                };
                return below_threshold(
                    format!(
                        "already {}{} — {} (est. {} → {}, {})",
                        m.vcodec,
                        bitrate_suffix(m),
                        outcome,
                        human_bytes(m.bytes),
                        human_bytes(est.new_bytes),
                        size_change_label(m.bytes, est.new_bytes),
                    ),
                    policy,
                );
            }
        }
    }
    let est = estimate_file(m, p);
    let saving = (1.0 - est.ratio) * 100.0;
    let change = size_change_label(m.bytes, est.new_bytes);
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
    let dense_note = if fgs_applies(m, p.backend, p.film_grain) {
        "; dense source — film-grain strength measured at encode"
    } else if est.density < 0.7 {
        "; dense/grainy source"
    } else {
        ""
    };
    if est.new_bytes >= m.bytes || saving < min_saving_pct {
        // Wording follows the direction: a growing estimate is a growth,
        // not a negative saving, and it says why it fell below the bar.
        let shortfall = if est.new_bytes >= m.bytes {
            format!("below {:.0}% saving threshold", min_saving_pct)
        } else {
            format!("{:.0}% < {:.0}% threshold", saving, min_saving_pct)
        };
        below_threshold(
            format!(
                "est. {} → {} ({change}; {shortfall}; {} {}, {}, audio {}{})",
                human_bytes(m.bytes),
                human_bytes(est.new_bytes),
                m.vcodec,
                res_note,
                quality_label(p),
                est.audio_mode,
                dense_note
            ),
            policy,
        )
    } else {
        Preflight::Shrink {
            reason: format!(
                "est. {} → {} ({change}; {:.0}% off; {} {}, {}, audio {}{})",
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
    preflight_auto_with(m, p, min_saving_pct, ThresholdPolicy::Skip)
}

/// [`preflight_auto`] with an explicit below-threshold policy, so still
/// images and videos agree on whether low-gain files warn or skip.
pub fn preflight_auto_with(
    m: &MediaFile,
    p: &EstParams,
    min_saving_pct: f64,
    policy: ThresholdPolicy,
) -> Preflight {
    if crate::images::is_shrinkable_image(&m.path) {
        crate::images::preflight_image_with(
            m,
            p.image_cq,
            p.image_scale,
            p.image_preserve_format,
            min_saving_pct,
            policy,
        )
    } else {
        preflight_with(m, p, min_saving_pct, policy)
    }
}

fn est_ratio(vcodec: &str, backend: VideoBackend) -> f64 {
    // Bench calibration (when a benchmark has run on this machine) scales
    // the table ratio by measured/modeled for that encoder family.
    let calib = crate::calib::factor_for(backend);
    let raw = match backend {
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
        // AV1 NVENC (Ada+): ~25% below HEVC NVENC at the same CQ — AV1's
        // compression edge at equal speed.
        VideoBackend::Av1Nvenc => av1_nvenc_ratio_for(vcodec),
        // Arc's AV1 encoder: strong, a step behind NVENC's.
        VideoBackend::QsvAv1 | VideoBackend::AmfAv1 => av1_nvenc_ratio_for(vcodec) * 1.10,
        // VideoToolbox / QSV / AMF HEVC: slightly behind NVENC per bit at
        // the same nominal quality.
        VideoBackend::VtHevc | VideoBackend::QsvHevc | VideoBackend::AmfHevc => {
            hw_upload_hevc_ratio_for(vcodec)
        }
        // Copy-video never re-encodes: video ratio is exactly 1.0
        // (estimate_file shortcuts before using this, belt and braces).
        VideoBackend::CopyVideo => 1.0,
        // Auto and the explicit NVENC HEVC/H.264 modes.
        _ => nvenc_hevc_ratio_for(vcodec),
    };
    (raw * calib).max(0.02)
}

/// Expected output/input byte ratio for AV1 NVENC at CQ28: the HEVC NVENC
/// table minus AV1's compression edge. Initial estimate — benchmark
/// calibration replaces it with measured values per machine.
fn av1_nvenc_ratio_for(vcodec: &str) -> f64 {
    match vcodec {
        "mpeg4" | "msmpeg4" | "xvid" => 0.40,
        "h264" => 0.56,
        "hevc" => 0.72,
        "av1" => 0.95,
        "mpeg2video" => 0.46,
        "vp9" => 0.72,
        _ => 0.60,
    }
}

/// HEVC via the upload families (VideoToolbox/QSV/AMF): NVENC's table with
/// a small penalty for their per-bit deficit at the same nominal quality.
fn hw_upload_hevc_ratio_for(vcodec: &str) -> f64 {
    (nvenc_hevc_ratio_for(vcodec) * 1.08).min(1.0)
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
    /// Audio-only pass: re-encode at the chosen target even when the source
    /// is already at/below it. Copying would be the "smart" default, but in
    /// a copy-video run the user explicitly asked for that bitrate —
    /// copying silently ignores the setting (and produced "192k" files whose
    /// audio was untouched 64k).
    pub force_audio: bool,
    /// Set when the plan had to dodge an Opus-hostile layout; surfaced in
    /// logs so "audio aac128k" never surprises.
    pub audio_layout_note: &'static str,
}

/// Codec decision for one audio track.
pub fn audio_track_codec(
    t: &crate::media::AudioTrack,
    opus_bps: Option<u32>,
    force_aac: bool,
    force_audio: bool,
) -> AudioCodec {
    // Off (None) keeps everything as-is however big.
    let Some(target) = opus_bps else {
        return AudioCodec::Copy;
    };
    if !force_audio && audio_copy_eligible_for_track(t, target) {
        return AudioCodec::Copy;
    }
    if force_aac || !opus_layout_ok(t) {
        return AudioCodec::Aac(aac_bps_for(target));
    }
    AudioCodec::Opus(opus_bps_for_track(t, target))
}

pub fn plan_streams(
    m: &MediaFile,
    keep_subs: bool,
    opus_bps: Option<u32>,
    all_audio: bool,
) -> StreamPlan {
    plan_streams_with(m, keep_subs, opus_bps, all_audio, false)
}

/// [`plan_streams`] with the audio policy spelled out: `force_audio` makes
/// the target an order rather than a ceiling.
pub fn plan_streams_with(
    m: &MediaFile,
    keep_subs: bool,
    opus_bps: Option<u32>,
    all_audio: bool,
    force_audio: bool,
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
            force_audio: false,
            audio_layout_note: "",
        };
    }
    let primary = m.primary_audio();
    let eligible = !force_audio
        && primary
            .map(|a| audio_copy_eligible_for_track(a, opus_bps.unwrap_or(DEFAULT_OPUS_BPS)))
            .unwrap_or(false);
    let codec = primary
        .map(|a| audio_track_codec(a, opus_bps, false, force_audio))
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
        force_audio,
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
            let c = audio_track_codec(t, plan.opus_bps, plan.force_aac, plan.force_audio);
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
        let l = audio_track_codec(t, plan.opus_bps, plan.force_aac, plan.force_audio).label();
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
            preserve_quality: false,
            backend: VideoBackend::Auto,
            preset: NvencPreset::P5,
            cq: 28,
            scale: ScalePolicy::Preserve,
            opus_bps: Some(DEFAULT_OPUS_BPS),
            all_audio: false,
            image_cq: 28,
            image_scale: ScalePolicy::Preserve,
            image_preserve_format: false,
            film_grain: false,
        }
    }

    fn caps_with(cuvid: &[&str]) -> HwCaps {
        HwCaps {
            cuda_hwaccel: true,
            hevc_nvenc: true,
            h264_nvenc: true,
            av1_nvenc_listed: false,
            av1_nvenc_usable: false,
            nvenc_b_ref_mode: false,
            vt_h264: false,
            vt_hevc: false,
            qsv_h264: false,
            qsv_hevc: false,
            qsv_av1_listed: false,
            qsv_av1_usable: false,
            amf_h264: false,
            amf_hevc: false,
            amf_av1_listed: false,
            amf_av1_usable: false,
            svt_av1: true,
            cuvid_decoders: cuvid.iter().map(|s| s.to_string()).collect(),
            gpu_label: "test".into(),
            hwaccels: vec!["cuda".into()],
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
    fn av1_backend_selects_svt_and_calibrates_its_preset_ladder() {
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
            preserve_quality: false,
            backend: VideoBackend::CpuAv1,
            preset: NvencPreset::P5,
            cq: 28,
            scale: ScalePolicy::Preserve,
            opus_bps: Some(DEFAULT_OPUS_BPS),
            all_audio: false,
            image_cq: 28,
            image_scale: ScalePolicy::Preserve,
            image_preserve_format: false,
            film_grain: false,
        };
        let slow_params = EstParams {
            preset: NvencPreset::P7,
            ..av1_params
        };
        let e_fast = estimate_file(&m, &av1_params);
        let e_slow = estimate_file(&m, &slow_params);
        assert!(
            e_slow.new_bytes < e_fast.new_bytes,
            "SVT preset 6 (P7) must estimate smaller than preset 8 (P5): {} vs {}",
            e_slow.new_bytes,
            e_fast.new_bytes
        );
    }

    #[test]
    fn cq_reference_is_neutral() {
        assert!((cq_factor(28) - 1.0).abs() < 1e-9);
        assert!(cq_factor(32) < 1.0 && cq_factor(32) > 0.6);
        assert!(cq_factor(24) > 1.0);
        // The shared slider stays put; only SVT's encoder call is offset.
        assert_eq!(svt_crf(28), 40);
        assert_eq!(svt_crf(32), 44);
        assert_eq!(svt_crf(40), 52);
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
        // An audio-only pass with a bitrate target is always queued: the
        // size thresholds describe shrinking, but the user asked for the
        // audio work, not for a smaller file.
        match preflight(&m, &p, 10.0) {
            Preflight::Shrink { reason } => {
                assert!(reason.contains("audio-only"), "reason: {reason}");
                assert!(reason.contains("opus64k"), "reason: {reason}");
            }
            Preflight::Skip { reason } | Preflight::Warn { reason } => {
                panic!("audio-only pass must not skip: {reason}")
            }
        }
        let mut av1 = m.clone();
        av1.vcodec = "av1".into();
        match preflight(&av1, &p, 10.0) {
            Preflight::Shrink { reason } => assert!(
                !reason.contains("negligible gain expected"),
                "copy mode must not codec-gate: {reason}"
            ),
            Preflight::Skip { reason } | Preflight::Warn { reason } => {
                panic!("audio-only pass must not skip: {reason}")
            }
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
        assert_eq!(scale_target(&land, ScalePolicy::Custom(0, 0)), Some((2, 2)));
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
    fn high_opus_targets_reach_the_codec_ceiling() {
        // A fixed target above 512k would fail for stereo (libopus allows
        // only 256k/ch), so the shared preset list stops at MAX_OPUS_BPS.
        assert_eq!(MAX_OPUS_BPS, OPUS_BPS_PER_CHANNEL * 2);
        assert_eq!(MAX_OPUS_BPS, *OPUS_TARGETS_KBPS.last().unwrap() * 1000);
        assert!(OPUS_TARGETS_KBPS.windows(2).all(|w| w[0] < w[1]));

        // A fat 5.1 DTS track is the case the top presets exist for.
        let mut m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        m.audio[0].codec = "dts".into();
        m.audio[0].bitrate = Some(768_000);
        m.audio[0].channels = 6;
        m.audio[0].layout = "5.1".into();
        let plan = plan_streams(&m, true, Some(MAX_OPUS_BPS), false);
        assert_eq!(plan.audio_codec, AudioCodec::Opus(512_000));

        let mut e = params();
        e.opus_bps = Some(MAX_OPUS_BPS);
        let est = estimate_file(&m, &e);
        assert_eq!(est.audio_mode, "opus512k");
        // 600 s × 64 kB/s.
        assert_eq!(est.new_audio_bytes, 38_400_000);

        // The same 512k preset on mono is clamped to libopus's 256k
        // per-channel ceiling instead of failing the encode.
        let mut mono = m.clone();
        mono.audio[0].channels = 1;
        mono.audio[0].layout = "mono".into();
        assert_eq!(
            plan_streams(&mono, true, Some(MAX_OPUS_BPS), false).audio_codec,
            AudioCodec::Opus(OPUS_BPS_PER_CHANNEL)
        );
        assert_eq!(estimate_file(&mono, &e).audio_mode, "opus256k");

        // A 448k lossy source is already below the ceiling: copy, since
        // transcoding upward cannot add quality.
        m.audio[0].bitrate = Some(448_000);
        assert_eq!(
            plan_streams(&m, true, Some(MAX_OPUS_BPS), false).audio_codec,
            AudioCodec::Copy
        );
    }

    #[test]
    fn audio_only_pass_honours_the_chosen_bitrate() {
        // An already-Shrinkr library: HEVC + Opus 5.1 with no reported
        // audio bitrate. A normal shrink copies that audio (it may already
        // be at/below the target), but an audio-only pass with an explicit
        // bitrate is an instruction — copying there is what made "192k"
        // silently produce untouched 64k tracks.
        let mut m = file("hevc", 1280, 720, 123_253_623, None);
        m.format_bitrate = Some(743_406);
        m.duration_s = 1326.365;
        m.audio[0] = AudioTrack {
            codec: "opus".into(),
            bitrate: None,
            channels: 6,
            layout: "5.1".into(),
            sample_rate: Some(48_000),
        };
        m.acodec = "opus".into();
        let mut p = params();
        p.opus_bps = Some(192_000);

        // Shrink mode: still copies (re-encoding upward cannot add quality).
        assert_eq!(
            plan_streams(&m, true, p.opus_bps, false).audio_codec,
            AudioCodec::Copy
        );
        assert!(!forces_audio_reencode(VideoBackend::Auto, p.opus_bps));

        // Audio-only mode: encodes the requested rate.
        p.backend = VideoBackend::CopyVideo;
        assert!(forces_audio_reencode(p.backend, p.opus_bps));
        let plan = plan_streams_with(&m, true, p.opus_bps, false, true);
        assert_eq!(plan.audio_codec, AudioCodec::Opus(192_000));
        assert_eq!(audio_mode_label(&m, &plan), "opus192k");

        // The estimate prices the re-encoded audio, not a copy.
        let est = estimate_file(&m, &p);
        assert_eq!(est.new_audio_bytes, (192_000.0 * m.duration_s / 8.0) as u64);
        assert_eq!(est.audio_mode, "opus192k");

        // And preflight queues it: size thresholds describe shrinking, and
        // this pass may legitimately grow the file.
        match preflight_with(&m, &p, 10.0, ThresholdPolicy::Skip) {
            Preflight::Shrink { reason } => {
                assert!(reason.contains("audio-only"), "{reason}");
                assert!(reason.contains("opus192k"), "{reason}");
            }
            Preflight::Skip { reason } => panic!("audio-only must not skip: {reason}"),
            Preflight::Warn { reason } => panic!("audio-only is not a warning: {reason}"),
        }
        // Off means nothing to do: still a plain copy.
        let mut off = p;
        off.opus_bps = None;
        assert!(!forces_audio_reencode(off.backend, off.opus_bps));
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
            Preflight::Skip { reason } | Preflight::Warn { reason } => {
                panic!("should shrink: {reason}")
            }
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
            Preflight::Skip { reason } | Preflight::Warn { reason } => {
                panic!("should shrink: {reason}")
            }
        }
        // Same file at defaults (NVENC, preserve): genuinely negligible.
        match preflight(&m, &params(), 10.0) {
            Preflight::Skip { .. } => {}
            Preflight::Shrink { reason } | Preflight::Warn { reason } => {
                panic!("should skip: {reason}")
            }
        }
        // Unchecking "skip already-efficient" turns that verdict into a
        // warning: the file stays in the run instead of being skipped.
        match preflight_with(&m, &params(), 10.0, ThresholdPolicy::Warn) {
            Preflight::Warn { reason } => {
                assert!(reason.contains("negligible gain"), "reason: {reason}");
                assert!(reason.contains("warned"), "reason: {reason}");
            }
            Preflight::Shrink { reason } => panic!("6% should warn at 10%: {reason}"),
            Preflight::Skip { reason } => panic!("warn mode must not skip: {reason}"),
        }
    }

    #[test]
    fn warn_mode_keeps_low_gain_files_in_the_run() {
        // A saving between 1% and the 10% threshold: a lean-ish HEVC file
        // (bytes match the bitrate × duration, as on disk).
        let m = file("hevc", 1920, 1080, 300_000_000, Some(4_000_000));
        let p = params();

        match preflight_with(&m, &p, 10.0, ThresholdPolicy::Warn) {
            Preflight::Warn { reason } => {
                // Either wording is a below-threshold verdict: the early
                // "already efficient" path or the generic estimate.
                assert!(
                    reason.contains("< 10% threshold") || reason.contains("negligible gain"),
                    "reason: {reason}"
                );
                assert!(reason.contains("warned"), "reason: {reason}");
            }
            Preflight::Shrink { reason } => panic!("below threshold should warn: {reason}"),
            Preflight::Skip { reason } => panic!("warn mode must not skip: {reason}"),
        }
        // A zero threshold still shrinks normally — no warning to show.
        match preflight_with(&m, &p, 0.0, ThresholdPolicy::Warn) {
            Preflight::Shrink { .. } => {}
            Preflight::Warn { reason } => panic!("0% threshold should not warn: {reason}"),
            Preflight::Skip { reason } => panic!("0% threshold must not skip: {reason}"),
        }
        // Above the threshold the warning policy is inert.
        match preflight_with(&m, &p, 1.0, ThresholdPolicy::Warn) {
            Preflight::Shrink { .. } => {}
            Preflight::Warn { reason } => panic!("saving is above 1%: {reason}"),
            Preflight::Skip { reason } => panic!("saving is above 1%: {reason}"),
        }
    }

    #[test]
    fn size_change_label_matches_direction() {
        assert_eq!(size_change_label(0, 0), "no size change");
        let shrink = size_change_label(1_000_000_000, 750_000_000);
        assert!(shrink.contains("smaller"), "{shrink}");
        assert!(shrink.contains("−25%"), "{shrink}");
        let grow = size_change_label(1_000_000_000, 1_250_000_000);
        assert!(grow.contains("larger"), "{grow}");
        assert!(grow.contains("+25%"), "{grow}");
        // A real but tiny change must not collapse to the misleading 0%.
        let tiny = size_change_label(1_000_000, 1_000_001);
        assert!(tiny.contains("<1%"), "{tiny}");
    }

    #[test]
    fn growing_estimate_is_described_as_growth() {
        // AV1 in, low CQ out: re-encoding genuinely predicts a bigger file.
        let m = file("av1", 1920, 1080, 1_000_000_000, Some(12_000_000));
        let mut p = params();
        p.cq = 18;
        p.preset = NvencPreset::P3;
        let est = estimate_file(&m, &p);
        assert!(
            est.new_bytes > m.bytes,
            "fixture must grow: {} → {}",
            m.bytes,
            est.new_bytes
        );
        match preflight_with(&m, &p, 10.0, ThresholdPolicy::Warn) {
            Preflight::Warn { reason } => {
                assert!(reason.contains("larger"), "reason: {reason}");
                assert!(
                    !reason.contains("-%"),
                    "growth must not read as a negative percentage: {reason}"
                );
                assert!(
                    reason.contains("below 10% saving threshold"),
                    "reason: {reason}"
                );
            }
            Preflight::Shrink { reason } => panic!("growth must not pass as a shrink: {reason}"),
            Preflight::Skip { reason } => panic!("warn mode must not skip: {reason}"),
        }
        // The skip policy rejects the same growth with the same wording.
        match preflight_with(&m, &p, 10.0, ThresholdPolicy::Skip) {
            Preflight::Skip { reason } => {
                assert!(reason.contains("larger"), "reason: {reason}");
                assert!(!reason.contains("-%"), "reason: {reason}");
            }
            Preflight::Warn { reason } | Preflight::Shrink { reason } => {
                panic!("growth must skip: {reason}")
            }
        }
    }

    #[test]
    fn unknown_stream_rates_do_not_double_count_audio() {
        // The shape of every MKV Shrinkr itself writes: container rate
        // known, per-stream rates not (ffprobe reports N/A in MKV). This is
        // a 22-minute 720p HEVC file with a 5.1 Opus track. The estimate
        // used to take 90% of the file as "video" AND add the audio back,
        // predicting 117.5 MB → 160.4 MB (+36%) — a bogus "re-encoding
        // would grow it" on the whole library, whatever the audio target.
        let mut m = file("hevc", 1280, 720, 123_253_623, None);
        m.format_bitrate = Some(743_406);
        m.duration_s = 1326.365;
        m.fps = 23.976;
        m.audio[0] = AudioTrack {
            codec: "opus".into(),
            bitrate: None,
            channels: 6,
            layout: "5.1".into(),
            sample_rate: Some(48_000),
        };
        m.acodec = "opus".into();
        let p = params();
        let est = estimate_file(&m, &p);
        assert!(
            est.new_bytes <= m.bytes,
            "a copied track cannot invent growth: {} → {}",
            m.bytes,
            est.new_bytes
        );
        assert_eq!(est.audio_mode, "copy");
        // Unknown-rate Opus is assumed efficient and stream-copied, so the
        // chosen target must not move the estimate at all.
        for target in [64_000, 96_000, 192_000, 512_000] {
            let mut t = params();
            t.opus_bps = Some(target);
            assert_eq!(
                estimate_file(&m, &t).new_bytes,
                est.new_bytes,
                "{target} bps must not change a copied track"
            );
        }
        // Verdict: a small honest loss, never a growth.
        match preflight_with(&m, &p, 10.0, ThresholdPolicy::Warn) {
            Preflight::Warn { reason } => {
                assert!(!reason.contains("grow"), "reason: {reason}");
                assert!(!reason.contains("larger"), "reason: {reason}");
            }
            Preflight::Shrink { reason } => panic!("3% gain should warn: {reason}"),
            Preflight::Skip { reason } => panic!("warn mode must not skip: {reason}"),
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
    fn lean_modern_source_has_no_small_crf_headroom() {
        // The real report: an already-shrunk 720p HEVC episode (0.9 Mbps,
        // 154 MB) was encoded to 184 MB at SVT CRF 32. A lean modern source
        // has no same-quality headroom; only a materially higher CRF can
        // beat it. CRF 32 is shared CQ 20 under the SVT offset.
        let mut m = file("hevc", 1280, 720, 161_813_503, Some(851_574));
        m.fps = 23.976;
        m.duration_s = 1321.5;
        let mut p = params();
        p.backend = VideoBackend::CpuAv1;
        p.preset = NvencPreset::P7;
        p.cq = 20;
        let below_frontier = estimate_file(&m, &p);
        assert!(
            below_frontier.new_bytes >= m.bytes,
            "a synthetic re-encode below the source's own CRF must not promise a shrink: {} -> {}",
            m.bytes,
            below_frontier.new_bytes
        );
        // Shared CQ 32 maps to SVT CRF 44 — above the source's frontier.
        p.cq = 32;
        let at32 = estimate_file(&m, &p);
        assert!(
            at32.new_bytes < m.bytes,
            "a stronger CRF must unlock savings: {} vs {}",
            at32.new_bytes,
            m.bytes
        );
        p.cq = 40;
        let at40 = estimate_file(&m, &p);
        assert!(
            at40.new_bytes < at32.new_bytes,
            "the top of the slider must be the smallest estimate: {} vs {}",
            at40.new_bytes,
            at32.new_bytes
        );
    }

    #[test]
    fn reencode_floor_skips_old_codecs_and_downscales() {
        let mut lean = file("hevc", 1920, 1080, 150_000_000, Some(2_000_000));
        lean.fps = 24.0;
        let mut p = params();
        p.backend = VideoBackend::CpuAv1;
        p.cq = 32;
        // Downscaling creates real headroom, so the floor must not apply.
        p.scale = ScalePolicy::Force720p;
        let scaled = estimate_file(&lean, &p);
        assert!(
            scaled.new_bytes < lean.bytes,
            "a 720p output from a lean 1080p source still shrinks: {} vs {}",
            scaled.new_bytes,
            lean.bytes
        );
        // A lean H.264 source is still old-encoder material: no floor.
        lean.vcodec = "h264".into();
        p.scale = ScalePolicy::Preserve;
        let old = estimate_file(&lean, &p);
        assert!(
            old.new_bytes < lean.bytes,
            "lean H.264 still has codec headroom: {} vs {}",
            old.new_bytes,
            lean.bytes
        );
    }

    #[test]
    fn gain_retries_climb_only_the_max_shrink_svt_path() {
        assert_eq!(gain_retry_cq(VideoBackend::CpuAv1, 32, false, 0), Some(36));
        assert_eq!(gain_retry_cq(VideoBackend::CpuAv1, 36, false, 1), Some(40));
        assert_eq!(gain_retry_cq(VideoBackend::CpuAv1, 40, false, 2), None);
        assert_eq!(
            gain_retry_cq(VideoBackend::CpuAv1, 28, false, 0),
            Some(32),
            "a manual lower CRF still gets a real retry"
        );
        assert_eq!(gain_retry_cq(VideoBackend::CpuAv1, 32, true, 0), None);
        assert_eq!(gain_retry_cq(VideoBackend::Auto, 32, false, 0), None);
        assert_eq!(
            gain_retry_cq(VideoBackend::CpuAv1, 32, false, MAX_GAIN_RETRIES),
            None
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

    fn mac_caps() -> HwCaps {
        HwCaps {
            cuda_hwaccel: false,
            hevc_nvenc: false,
            h264_nvenc: false,
            av1_nvenc_listed: false,
            av1_nvenc_usable: false,
            nvenc_b_ref_mode: false,
            vt_h264: true,
            vt_hevc: true,
            qsv_h264: false,
            qsv_hevc: false,
            qsv_av1_listed: false,
            qsv_av1_usable: false,
            amf_h264: false,
            amf_hevc: false,
            amf_av1_listed: false,
            amf_av1_usable: false,
            svt_av1: true,
            cuvid_decoders: vec![],
            gpu_label: "Apple M-series".into(),
            hwaccels: vec!["videotoolbox".into()],
        }
    }

    #[test]
    fn auto_ladder_covers_non_nvidia_machines() {
        // A Mac: no NVENC anywhere — Auto must start at VideoToolbox HEVC,
        // not fall straight through to CPU x264.
        let caps = mac_caps();
        let levels = select_levels(VideoBackend::Auto, &caps);
        assert_eq!(levels[0], Level::VtHevc, "{levels:?}");
        assert!(levels.contains(&Level::VtH264));
        assert_eq!(*levels.last().unwrap(), Level::CpuX264);
        // The family's explicit mode starts the same place and keeps an
        // H.264 sibling before the CPU floor.
        assert_eq!(
            select_levels(VideoBackend::VtHevc, &caps),
            vec![Level::VtHevc, Level::VtH264, Level::CpuX264]
        );
        // Intel Arc: QSV AV1 usable, HEVC behind it, CPU floor last.
        let arc = HwCaps {
            qsv_h264: true,
            qsv_hevc: true,
            qsv_av1_listed: true,
            qsv_av1_usable: true,
            vt_h264: false,
            vt_hevc: false,
            ..mac_caps()
        };
        assert_eq!(
            select_levels(VideoBackend::QsvAv1, &arc),
            vec![
                Level::QsvAv1,
                Level::QsvHevc,
                Level::QsvH264,
                Level::CpuX264
            ]
        );
        // Auto stays HEVC-first (compat) even with AV1 hardware present.
        let auto = select_levels(VideoBackend::Auto, &arc);
        assert_eq!(auto[0], Level::QsvHevc, "{auto:?}");
    }

    #[test]
    fn av1_nvenc_ladder_needs_the_trial() {
        let mut caps = caps_with(&["h264_cuvid", "hevc_cuvid", "av1_cuvid"]);
        // Trial passed: zero-copy AV1 first, CPU-decode upload behind it,
        // HEVC NVENC as the in-family fallback, CPU last.
        caps.av1_nvenc_usable = true;
        assert_eq!(
            select_levels(VideoBackend::Av1Nvenc, &caps),
            vec![
                Level::HwAv1,
                Level::CpuDecodeAv1Nvenc,
                Level::HwHevc,
                Level::CpuDecodeHevcNvenc,
                Level::CpuX264,
            ]
        );
        // Trial failed (RTX 30): the mode silently degrades to the HEVC
        // ladder — the UI never shows it in this state anyway.
        caps.av1_nvenc_usable = false;
        assert_eq!(
            select_levels(VideoBackend::Av1Nvenc, &caps),
            vec![Level::HwHevc, Level::CpuDecodeHevcNvenc, Level::CpuX264,]
        );
    }

    #[test]
    fn svt_preset_follows_the_shared_knob() {
        assert_eq!(NvencPreset::P5.svt(), 8);
        let presets = NvencPreset::all_bench().map(|p| p.svt());
        assert_eq!(presets, [10, 9, 8, 7]);
        assert!(presets.windows(2).all(|pair| pair[0] > pair[1]));
        // The SVT-only effort steps: each is one preset slower and smaller.
        assert_eq!(NvencPreset::P7.svt(), 6);
        assert_eq!(NvencPreset::P8.svt(), 5);
        assert_eq!(NvencPreset::P9.svt(), 4);
        assert_eq!(NvencPreset::P8.as_str(), "p7");
        assert_eq!(NvencPreset::P5.qsv(), "medium");
        assert_eq!(NvencPreset::P3.qsv(), "veryfast");
        assert_eq!(NvencPreset::P4.amf(), "balanced");
        assert_eq!(NvencPreset::P6.amf(), "quality");
        assert_eq!(NvencPreset::P9.qsv(), "slower");
        // The estimator follows the SVT axis too: faster presets spend
        // more bits, and preset 6 (P7) is the reference at 1.0.
        assert_eq!(preset_factor(VideoBackend::CpuAv1, NvencPreset::P7), 1.0);
        assert!(
            preset_factor(VideoBackend::CpuAv1, NvencPreset::P5) > 1.1,
            "SVT 8 is materially larger than SVT 6"
        );
        assert!(preset_factor(VideoBackend::CpuAv1, NvencPreset::P9) < 1.0);
        assert_eq!(preset_factor(VideoBackend::CpuX265, NvencPreset::P5), 1.0);
    }

    #[test]
    fn film_grain_relaxes_the_density_penalty_on_dense_sources() {
        // 20 Mbps noisy 1080p models as dense; AV1 with film-grain on must
        // estimate smaller than the same encode with it off, and the clean
        // 3 Mbps file must not care.
        let dense = file("h264", 1920, 1080, 2_500_000_000, Some(20_000_000));
        let clean = file("h264", 1920, 1080, 400_000_000, Some(3_000_000));
        let mut on = params();
        on.backend = VideoBackend::CpuAv1;
        on.film_grain = true;
        let mut off = on;
        off.film_grain = false;
        let d_on = estimate_file(&dense, &on);
        let d_off = estimate_file(&dense, &off);
        assert!(
            d_on.new_bytes < d_off.new_bytes,
            "fgs should shrink the dense estimate: {} vs {}",
            d_on.new_bytes,
            d_off.new_bytes
        );
        // Clean source: the switch changes nothing.
        assert_eq!(
            estimate_file(&clean, &on).new_bytes,
            estimate_file(&clean, &off).new_bytes
        );
        // And the predicate is SVT-only: NVENC modes ignore it.
        let mut nv = params();
        nv.backend = VideoBackend::HevcNvenc;
        nv.film_grain = true;
        assert!(!fgs_applies(&dense, nv.backend, nv.film_grain));
        assert!(fgs_applies(&dense, on.backend, on.film_grain));
    }

    #[test]
    fn av1_nvenc_estimates_below_hevc_nvenc() {
        // The whole point of the backend: at the same CQ the AV1 hardware
        // table must promise a smaller file than HEVC NVENC's.
        let m = file("h264", 1920, 1080, 1_000_000_000, Some(12_000_000));
        let mut av1 = params();
        av1.backend = VideoBackend::Av1Nvenc;
        let mut hevc = params();
        hevc.backend = VideoBackend::HevcNvenc;
        assert!(estimate_file(&m, &av1).new_bytes < estimate_file(&m, &hevc).new_bytes);
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
