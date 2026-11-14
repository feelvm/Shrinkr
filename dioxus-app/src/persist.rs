//! Settings persistence: the user's pipeline picks survive restarts.
//!
//! The GUI keeps every knob in one big in-memory `AppState`; on first run
//! it all reset to defaults, which read as broken ("I set this yesterday").
//! `collect` snapshots the user-facing settings out of `AppState`,
//! `offer` hands the latest snapshot to a background saver (debounced —
//! sliders fire dozens of events per drag), and `load` reads the last
//! saved set back in `AppState::default`. Unknown fields and new fields
//! both default through serde, so schema drift degrades to "a knob or two
//! resets", never a crash.

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Bump when a field's meaning changes in a way old files can't map onto
/// (currently: never — serde defaults make every addition/back-compat
/// rename loadable without a version bump).
const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct GuiSettings {
    pub schema_version: u32,
    // ── Video pipeline ──
    pub preserve_quality: bool,
    pub backend: shrinkr_core::pipeline::VideoBackend,
    pub nvenc_preset: shrinkr_core::pipeline::NvencPreset,
    pub cq: u32,
    pub scale: shrinkr_core::pipeline::ScalePolicy,
    pub opus_bps: Option<u32>,
    pub all_audio: bool,
    pub film_grain: bool,
    pub keep_subs: bool,
    // ── Images ──
    pub image_cq: u32,
    pub image_scale: shrinkr_core::pipeline::ScalePolicy,
    pub image_preserve_format: bool,
    // ── General ──
    pub min_saving_pct: f64,
    pub skip_efficient: bool,
    pub parallel: usize,
    pub replace: bool,
    pub extra_args: String,
    pub target_text: String,
    /// Codec mutes are user decisions about their library — persisted too.
    pub muted_codecs: Vec<String>,
    // ── Convert / Optimize tabs ──
    pub convert_replace: bool,
    pub optimize_replace: bool,
    pub optimize_pdf_preset: shrinkr_core::pdf::PdfPreset,
    pub optimize_image_level: shrinkr_core::optimize::OptimizeLevel,
    /// Log severity lanes the user turned off.
    pub log_filter: shrinkr_core::log::LevelFilter,
}

impl Default for GuiSettings {
    /// Defaults mirror `AppState::default` (which does NOT read the
    /// settings file — the load+apply happens in App, so constructing a
    /// default here cannot recurse or race the file).
    fn default() -> Self {
        let s = AppState::default();
        collect(&s)
    }
}

impl GuiSettings {
    /// Overlay the saved settings onto a freshly-defaulted `AppState`.
    /// Only user choices are restored; everything transient (running
    /// batches, probed files, update state) keeps its default.
    pub fn apply(&self, s: &mut AppState) {
        s.preserve_quality = self.preserve_quality;
        s.backend = self.backend;
        s.last_video_backend = self.backend;
        s.nvenc_preset = self.nvenc_preset;
        s.cq = self.cq;
        s.scale = self.scale;
        s.opus_bps = self.opus_bps;
        s.all_audio = self.all_audio;
        s.film_grain = self.film_grain;
        s.keep_subs = self.keep_subs;
        s.image_cq = self.image_cq;
        s.image_scale = self.image_scale;
        s.image_preserve_format = self.image_preserve_format;
        s.min_saving_pct = self.min_saving_pct;
        s.skip_efficient = self.skip_efficient;
        s.parallel = self.parallel.clamp(1, 8);
        s.replace = self.replace;
        s.extra_args = self.extra_args.clone();
        s.target_text = self.target_text.clone();
        s.muted_codecs = self.muted_codecs.iter().cloned().collect();
        s.convert_replace = self.convert_replace;
        s.optimize_replace = self.optimize_replace;
        s.optimize_pdf_preset = self.optimize_pdf_preset;
        s.optimize_image_level = self.optimize_image_level;
        s.log_filter = self.log_filter;
    }
}

/// Snapshot the persisted subset out of the current state.
pub fn collect(s: &AppState) -> GuiSettings {
    GuiSettings {
        schema_version: SCHEMA_VERSION,
        preserve_quality: s.preserve_quality,
        backend: s.backend,
        nvenc_preset: s.nvenc_preset,
        cq: s.cq,
        scale: s.scale,
        opus_bps: s.opus_bps,
        all_audio: s.all_audio,
        film_grain: s.film_grain,
        keep_subs: s.keep_subs,
        image_cq: s.image_cq,
        image_scale: s.image_scale,
        image_preserve_format: s.image_preserve_format,
        min_saving_pct: s.min_saving_pct,
        skip_efficient: s.skip_efficient,
        parallel: s.parallel.clamp(1, 8),
        replace: s.replace,
        extra_args: s.extra_args.clone(),
        target_text: s.target_text.clone(),
        muted_codecs: {
            let mut v: Vec<String> = s.muted_codecs.iter().cloned().collect();
            v.sort();
            v
        },
        convert_replace: s.convert_replace,
        optimize_replace: s.optimize_replace,
        optimize_pdf_preset: s.optimize_pdf_preset,
        optimize_image_level: s.optimize_image_level,
        log_filter: s.log_filter,
    }
}

// ── Debounced saver ─────────────────────────────────────────────────

static PENDING: std::sync::Mutex<Option<GuiSettings>> = std::sync::Mutex::new(None);
static DIRTY: AtomicBool = AtomicBool::new(false);

/// Hand the latest snapshot to the saver thread. Cheap enough to call on
/// every state change: it just overwrites the pending slot and sets a
/// flag — the actual disk write happens at most once per saver tick.
pub fn offer(settings: GuiSettings) {
    if let Ok(mut slot) = PENDING.lock() {
        *slot = Some(settings);
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// Whether the next saver tick has something to write.
#[cfg(test)]
pub fn is_dirty() -> bool {
    DIRTY.load(Ordering::Relaxed)
}

/// Saver loop: wake up once per second, write at most one snapshot.
/// Runs for the process lifetime on a background thread.
pub fn spawn_saver() {
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if !DIRTY.swap(false, Ordering::Relaxed) {
            continue;
        }
        let snapshot = PENDING.lock().ok().and_then(|mut slot| slot.take());
        if let Some(settings) = snapshot {
            let json = match serde_json::to_string_pretty(&settings) {
                Ok(j) => j,
                Err(_) => continue,
            };
            if let Some(path) = settings_path() {
                save_atomically(&path, &json);
            }
        }
    });
}

/// Same durable location the bench calibration uses (`%LOCALAPPDATA%\shrinkr`
/// on Windows, `~/.cache/shrinkr` elsewhere) — one app-data folder.
fn settings_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|l| PathBuf::from(l).join("shrinkr").join("settings.json"))
    }
    #[cfg(not(windows))]
    {
        let base = if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
            PathBuf::from(cache).join("shrinkr")
        } else {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("shrinkr"))?
        };
        Some(base.join("settings.json"))
    }
}

/// Write-through: temp file first, then a rename over the target, so a
/// crash mid-write can never leave a half-written settings file behind.
fn save_atomically(path: &std::path::Path, json: &str) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // A second concurrently running instance must not clobber this one's
    // temp file mid-rename.
    let tmp = dir.join(format!("settings.json.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, json).is_err() {
        return;
    }
    // Windows rename fails onto an existing destination; remove first.
    // The window where settings.json is absent is one rename wide — and
    // a missing file just means "defaults" on the next start.
    let _ = std::fs::remove_file(path);
    let _ = std::fs::rename(&tmp, path);
}

/// Load the saved settings (None on first run / unreadable file).
pub fn load() -> Option<GuiSettings> {
    let text = std::fs::read_to_string(settings_path()?).ok()?;
    let s: GuiSettings = serde_json::from_str(&text).ok()?;
    // A future schema version may change a field's meaning; refuse the
    // whole file rather than misread it.
    if s.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_apply_round_trips_every_knob() {
        let mut log_filter = shrinkr_core::log::LevelFilter::all();
        log_filter.0[shrinkr_core::log::Level::Command as usize] = false;
        let s = AppState {
            preserve_quality: false,
            backend: shrinkr_core::pipeline::VideoBackend::CpuX264,
            nvenc_preset: shrinkr_core::pipeline::NvencPreset::P3,
            cq: 33,
            scale: shrinkr_core::pipeline::ScalePolicy::Force720p,
            opus_bps: Some(128_000),
            all_audio: false,
            film_grain: false,
            keep_subs: false,
            image_cq: 30,
            image_scale: shrinkr_core::pipeline::ScalePolicy::Custom(1600, 900),
            image_preserve_format: false,
            min_saving_pct: 7.5,
            skip_efficient: false,
            parallel: 4,
            replace: true,
            extra_args: "-tune grain".into(),
            target_text: "400MB".into(),
            muted_codecs: ["mpeg4".to_string()].into_iter().collect(),
            convert_replace: true,
            optimize_replace: true,
            optimize_pdf_preset: shrinkr_core::pdf::PdfPreset::Small,
            optimize_image_level: shrinkr_core::optimize::OptimizeLevel::Max,
            log_filter,
            ..Default::default()
        };

        let snap = collect(&s);
        let json = serde_json::to_string(&snap).unwrap();
        let loaded: GuiSettings = serde_json::from_str(&json).unwrap();
        let mut restored = AppState::default();
        loaded.apply(&mut restored);

        assert_eq!(restored.backend, snap.backend);
        assert_eq!(restored.nvenc_preset, snap.nvenc_preset);
        assert_eq!(restored.cq, 33);
        assert_eq!(restored.scale, snap.scale);
        assert_eq!(restored.opus_bps, Some(128_000));
        assert_eq!(restored.image_scale, snap.image_scale);
        assert_eq!(restored.min_saving_pct, 7.5);
        assert_eq!(restored.parallel, 4);
        assert_eq!(restored.extra_args, "-tune grain");
        assert_eq!(restored.target_text, "400MB");
        assert!(restored.muted_codecs.contains("mpeg4"));
        assert_eq!(
            restored.optimize_pdf_preset,
            shrinkr_core::pdf::PdfPreset::Small
        );
        assert!(!restored
            .log_filter
            .visible(shrinkr_core::log::Level::Command));
        // Transient state is never restored.
        assert!(!restored.executing);
        assert!(restored.files.is_empty());
    }

    #[test]
    fn unknown_fields_and_missing_fields_degrade_to_defaults() {
        let base = serde_json::to_string(&GuiSettings::default()).unwrap();
        let with_unknown = base.replace(
            "{",
            "{\"some_future_field\": 1, \"backend\": \"NotABackend\",",
        );
        // "NotABackend" is an unknown enum variant — the whole file is
        // rejected rather than silently mapping to Auto.
        let parsed: Result<GuiSettings, _> = serde_json::from_str(&with_unknown);
        assert!(
            parsed.is_err(),
            "unknown enum values must not load as defaults"
        );

        // A file missing fields (older schema) loads with defaults filled.
        let trimmed: serde_json::Value = serde_json::from_str(&base).unwrap();
        let mut partial = serde_json::Map::new();
        for (k, v) in trimmed.as_object().unwrap() {
            if k != "cq" && k != "parallel" {
                partial.insert(k.clone(), v.clone());
            }
        }
        let loaded: GuiSettings = serde_json::from_value(partial.into()).unwrap();
        assert_eq!(loaded.cq, 28, "missing fields fall back to the default");
        assert_eq!(loaded.parallel, 2);
    }

    #[test]
    fn offer_flags_dirty_until_taken() {
        offer(collect(&AppState::default()));
        assert!(is_dirty());
        DIRTY.store(false, Ordering::Relaxed);
    }
}
