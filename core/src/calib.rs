//! Bench calibration: turn benchmark measurements into per-encoder size
//! corrections for the analytical estimator.
//!
//! The estimator's ratio tables (`pipeline::est_ratio`) are generic
//! starting points; GPUs differ by driver and generation. After a
//! benchmark run, each measured row is compared against what the model
//! predicted for the same file/settings, and the measured/modeled ratio
//! becomes a per-encoder factor. The factors persist as JSON under the
//! app-data folder and load on the next start, so Goal-segment and batch
//! estimates become "measured on this machine" instead of "from a table".
//!
//! Design limits, on purpose:
//! * one factor per encoder family (not per codec/preset/CQ) — a single
//!   correction carries most of the bias with none of the overfitting;
//! * clamped to [0.5, 2.0] — a wild measurement can at most halve/double
//!   the modeled estimate, never invert the plan;
//! * calibration is additive information: with no bench run the factors
//!   are all 1.0 and the tables behave exactly as before.

use crate::pipeline::{EstParams, VideoBackend};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

/// Persisted shape: encoder key → multiplicative correction.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Calibration {
    pub factors: HashMap<String, f64>,
    /// Human-readable provenance for the log panel.
    #[serde(default)]
    pub source: String,
}

const MIN_FACTOR: f64 = 0.5;
const MAX_FACTOR: f64 = 2.0;

/// Encoder keys the estimator consults. Backends without a key (H.264
/// NVENC, the VT/QSV/AMF families) stay at 1.0 — the benchmark doesn't
/// measure them, and a cross-family correction would be noise.
pub fn calib_key(backend: VideoBackend) -> Option<&'static str> {
    match backend {
        VideoBackend::Auto | VideoBackend::HevcNvenc => Some("hevc_nvenc"),
        VideoBackend::Av1Nvenc => Some("av1_nvenc"),
        VideoBackend::CpuX264 => Some("x264"),
        VideoBackend::CpuAv1 => Some("svt_av1"),
        _ => None,
    }
}

static CAL: OnceLock<RwLock<Calibration>> = OnceLock::new();

fn store() -> &'static RwLock<Calibration> {
    CAL.get_or_init(|| {
        // Under test the estimator must be deterministic: a calibration
        // left on this machine by a real benchmark would silently shift
        // every estimate expectation. The load/save paths get their own
        // direct tests below.
        #[cfg(not(test))]
        let cal = load_from_disk().unwrap_or_default();
        #[cfg(test)]
        let cal = Calibration::default();
        RwLock::new(cal)
    })
}

/// Multiplicative correction for a backend (1.0 = no calibration).
pub fn factor_for(backend: VideoBackend) -> f64 {
    let Some(key) = calib_key(backend) else {
        return 1.0;
    };
    store()
        .read()
        .ok()
        .and_then(|c| c.factors.get(key).copied())
        .filter(|f| f.is_finite())
        .unwrap_or(1.0)
}

/// Replace the active calibration (called after a benchmark) and persist
/// it best-effort — a read-only app-data folder must not fail a bench run.
pub fn apply(cal: Calibration) {
    if let Ok(mut g) = store().write() {
        *g = cal.clone();
    }
    let _ = save_to_disk(&cal);
}

/// Compute factors from benchmark measurements: for every `(settings,
/// measured_ratio)` point, the model's prediction for the same file and
/// settings is `estimate_file`, and the per-key factor is the mean of
/// `measured / modeled` across that key's points.
pub fn from_measurements(
    m: &crate::media::MediaFile,
    points: &[(EstParams, f64)],
    source: &str,
) -> Calibration {
    let mut sums: HashMap<&str, (f64, usize)> = HashMap::new();
    for (p, measured) in points {
        let Some(key) = calib_key(p.backend) else {
            continue;
        };
        let modeled = crate::pipeline::estimate_file(m, p).ratio;
        if !(modeled > 0.02 && *measured > 0.0) {
            continue;
        }
        let e = sums.entry(key).or_insert((0.0, 0));
        e.0 += measured / modeled;
        e.1 += 1;
    }
    let mut factors = HashMap::new();
    for (key, (sum, n)) in sums {
        if n > 0 {
            factors.insert(
                key.to_string(),
                (sum / n as f64).clamp(MIN_FACTOR, MAX_FACTOR),
            );
        }
    }
    Calibration {
        factors,
        source: source.to_string(),
    }
}

// ── Persistence ──────────────────────────────────────────────────────

/// App-data folder (mirrors `ffmpeg::work_candidates`'s durable choices;
/// temp dirs get wiped). Missing folder → None, caller keeps defaults.
fn calib_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("shrinkr"))
    }
    #[cfg(not(windows))]
    {
        if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
            Some(PathBuf::from(cache).join("shrinkr"))
        } else {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("shrinkr"))
        }
    }
}

fn calib_path() -> Option<PathBuf> {
    calib_dir().map(|d| d.join("bench-calibration.json"))
}

fn load_from_disk() -> Option<Calibration> {
    load_from_path(&calib_path()?)
}

fn load_from_path(path: &std::path::Path) -> Option<Calibration> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut cal: Calibration = serde_json::from_str(&text).ok()?;
    // Sanitize: a corrupt/hand-edited value must not poison the estimator.
    cal.factors
        .retain(|_, v| v.is_finite() && (MIN_FACTOR..=MAX_FACTOR).contains(v));
    Some(cal)
}

fn save_to_disk(cal: &Calibration) -> std::io::Result<()> {
    let path = calib_path().ok_or_else(|| {
        std::io::Error::other("no app-data folder for calibration (LOCALAPPDATA/HOME unset)")
    })?;
    save_to_path(&path, cal)
}

fn save_to_path(path: &std::path::Path, cal: &Calibration) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cal).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{AudioTrack, MediaFile};
    use crate::pipeline::{NvencPreset, ScalePolicy, DEFAULT_OPUS_BPS};

    fn media() -> MediaFile {
        MediaFile {
            path: std::path::PathBuf::from("t.mp4"),
            bytes: 1_000_000_000,
            vcodec: "h264".into(),
            width: 1920,
            height: 1080,
            fps: 30.0,
            pix_fmt: "yuv420p".into(),
            bit_depth: 8,
            color_space: String::new(),
            color_primaries: String::new(),
            color_transfer: String::new(),
            color_range: String::new(),
            vbitrate: Some(12_000_000),
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

    fn params_for(backend: VideoBackend) -> EstParams {
        EstParams {
            backend,
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

    #[test]
    fn keys_cover_only_measured_families() {
        assert_eq!(calib_key(VideoBackend::Auto), Some("hevc_nvenc"));
        assert_eq!(calib_key(VideoBackend::CpuAv1), Some("svt_av1"));
        assert_eq!(calib_key(VideoBackend::Av1Nvenc), Some("av1_nvenc"));
        assert_eq!(calib_key(VideoBackend::VtHevc), None);
        assert_eq!(calib_key(VideoBackend::CopyVideo), None);
    }

    #[test]
    fn factor_defaults_to_one_and_clamps() {
        // No bench run: everything is neutral.
        assert!((factor_for(VideoBackend::Auto) - 1.0).abs() < 1e-12);
        assert!((factor_for(VideoBackend::VtHevc) - 1.0).abs() < 1e-12);
        // A wild measurement is clamped into the sane band.
        let m = media();
        let modeled = crate::pipeline::estimate_file(&m, &params_for(VideoBackend::CpuX264)).ratio;
        let cal = from_measurements(
            &m,
            &[(params_for(VideoBackend::CpuX264), modeled * 50.0)],
            "test",
        );
        assert!((cal.factors["x264"] - MAX_FACTOR).abs() < 1e-12);
        // And a normal one round-trips the ratio.
        let cal = from_measurements(
            &m,
            &[(params_for(VideoBackend::CpuX264), modeled * 1.2)],
            "test",
        );
        assert!((cal.factors["x264"] - 1.2).abs() < 1e-9);
    }

    #[test]
    fn calibration_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("bench-calibration.json");
        let cal = Calibration {
            factors: [("hevc_nvenc".to_string(), 1.25)].into_iter().collect(),
            source: "test".into(),
        };
        save_to_path(&path, &cal).unwrap();
        let back = load_from_path(&path).unwrap();
        assert!((back.factors["hevc_nvenc"] - 1.25).abs() < 1e-12);
        // Hand-corrupted values are dropped on load, sane ones survive.
        let bad = r#"{"factors":{"hevc_nvenc":9.5,"av1_nvenc":0.9},"source":"x"}"#;
        std::fs::write(&path, bad).unwrap();
        let sane = load_from_path(&path).unwrap();
        assert!(!sane.factors.contains_key("hevc_nvenc"));
        assert!(sane.factors.contains_key("av1_nvenc"));
        // Garbage JSON is a None, not a panic.
        std::fs::write(&path, "not json").unwrap();
        assert!(load_from_path(&path).is_none());
    }
}
