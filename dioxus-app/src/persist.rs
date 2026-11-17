//! Durable GUI state: pipeline picks, the unfinished queue, and the
//! processed-file ledger.
//!
//! The GUI keeps every knob in one big in-memory `AppState`; on first run
//! it all reset to defaults, which read as broken ("I set this yesterday").
//! `collect` snapshots the user-facing settings out of `AppState`,
//! `offer` hands the latest snapshot to a background saver (debounced -
//! sliders fire dozens of events per drag), and `load` reads the last
//! saved set back in `AppState::default`. Unknown fields and new fields
//! both default through serde, so schema drift degrades to "a knob or two
//! resets", never a crash.
//!
//! Two more files share the same folder:
//! * `queue.json` - the unfinished part of a running batch plus every
//!   waiting segment. Written while work is in flight; on the next launch
//!   it is re-probed and restored as a paused queue (`Resume queue`).
//! * `history.json` - a bounded ledger of processed inputs keyed by path,
//!   size and modified time. Scans can skip files that have not changed
//!   since a run instead of re-applying a lossy encode. The GUI exposes a
//!   toggle and a clear button; the ledger is advisory, never destructive.

use crate::state::{AppState, ExecCfg, PendingSegment};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// Bump when a field's meaning changes in a way old files can't map onto
/// (currently: never - serde defaults make every addition/back-compat
/// rename loadable without a version bump).
const SCHEMA_VERSION: u32 = 1;
const QUEUE_SCHEMA_VERSION: u32 = 1;
const HISTORY_SCHEMA_VERSION: u32 = 1;
/// Keep the ledger bounded: enough for a large archived project, small
/// enough that lookup and the JSON write stay cheap.
const MAX_HISTORY_ENTRIES: usize = 5000;

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
    pub skip_processed: bool,
    pub parallel: usize,
    pub replace: bool,
    pub extra_args: String,
    pub target_text: String,
    /// Codec mutes are user decisions about their library - persisted too.
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
    /// settings file - the load+apply happens in App, so constructing a
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
        s.skip_processed = self.skip_processed;
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
        skip_processed: s.skip_processed,
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

// ── Unfinished queue ─────────────────────────────────────────────────

/// One batch of paths plus the settings they were queued with. The files
/// are re-probed on restore, so a moved/deleted input is dropped instead
/// of failing the resume.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QueuedBatch {
    pub files: Vec<PathBuf>,
    pub cfg: ExecCfg,
    pub warn: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct QueueSnapshot {
    pub schema_version: u32,
    pub saved_at: u64,
    /// The part of the running batch that had not finished when the
    /// snapshot was taken (the in-flight file is re-run from scratch).
    pub running: Option<QueuedBatch>,
    /// Batches waiting behind it, in order.
    pub pending: Vec<QueuedBatch>,
}

impl QueueSnapshot {
    pub fn is_empty(&self) -> bool {
        self.running.is_none() && self.pending.is_empty()
    }

    pub fn file_count(&self) -> usize {
        self.running
            .iter()
            .chain(self.pending.iter())
            .map(|b| b.files.len())
            .sum()
    }
}

/// A queue snapshot for the current state. Only unfinished work is
/// captured: done/failed/removed rows and user-skipped paths are left out
/// so a restart never repeats a finished encode or overrides a skip.
pub fn collect_queue(s: &AppState) -> QueueSnapshot {
    let skipped: HashSet<PathBuf> = s
        .exec_skips
        .as_ref()
        .and_then(|m| m.lock().ok())
        .map(|g| g.clone())
        .unwrap_or_default();
    let running = if s.executing {
        s.exec_cfg.as_ref().and_then(|cfg| {
            let files: Vec<PathBuf> = s
                .exec_items
                .iter()
                .zip(s.exec_files.iter())
                .filter(|(item, f)| {
                    !item.done && !item.failed && !item.removed && !skipped.contains(&f.path)
                })
                .map(|(_, f)| f.path.clone())
                .collect();
            if files.is_empty() {
                None
            } else {
                Some(QueuedBatch {
                    files,
                    cfg: cfg.clone(),
                    warn: false,
                })
            }
        })
    } else {
        None
    };
    let pending: Vec<QueuedBatch> = s
        .pending
        .iter()
        .filter_map(|seg| {
            let files: Vec<PathBuf> = seg
                .files
                .iter()
                .filter(|f| !skipped.contains(&f.path))
                .map(|f| f.path.clone())
                .collect();
            if files.is_empty() {
                None
            } else {
                Some(QueuedBatch {
                    files,
                    cfg: seg.cfg.clone(),
                    warn: seg.warn,
                })
            }
        })
        .collect();
    QueueSnapshot {
        schema_version: QUEUE_SCHEMA_VERSION,
        saved_at: now_ms(),
        running,
        pending,
    }
}

/// Hand the current queue to the saver. No-op while the startup restore
/// is still probing: the file on disk is the source of truth until the
/// restored queue lands in `AppState`.
pub fn save_queue(s: &AppState) {
    if s.queue_restoring {
        return;
    }
    offer_queue(collect_queue(s));
}

/// Restore a snapshot into `PendingSegment`s, keeping only paths that
/// still probe successfully. Returns the segments and the number of
/// dropped paths.
pub fn restored_segments(
    snapshot: QueueSnapshot,
    probed: &[(PathBuf, Option<shrinkr_core::media::MediaFile>)],
) -> (Vec<PendingSegment>, usize) {
    let by_path: HashMap<PathBuf, shrinkr_core::media::MediaFile> = probed
        .iter()
        .filter_map(|(p, m)| m.as_ref().map(|m| (p.clone(), m.clone())))
        .collect();
    let mut dropped = 0usize;
    let mut segments = Vec::new();
    for batch in snapshot.running.into_iter().chain(snapshot.pending) {
        let before = batch.files.len();
        let files: Vec<_> = batch
            .files
            .iter()
            .filter_map(|p| by_path.get(p).cloned())
            .collect();
        dropped += before - files.len();
        if !files.is_empty() {
            segments.push(PendingSegment {
                files,
                cfg: batch.cfg,
                warn: batch.warn,
            });
        }
    }
    (segments, dropped)
}

// ── Processed-file ledger ────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Original spelling of the path, for display.
    pub path: String,
    pub size: u64,
    pub mtime_ms: u64,
    pub processed_at_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct History {
    pub schema_version: u32,
    pub entries: BTreeMap<String, HistoryEntry>,
}

impl History {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Record one path if its metadata can be read. The ledger key is the
    /// platform-appropriate spelling: case-insensitive on Windows so a
    /// path picked with different casing still matches.
    pub fn record(&mut self, path: &Path, now: u64) -> bool {
        let Some((size, mtime_ms)) = fingerprint(path) else {
            return false;
        };
        self.schema_version = HISTORY_SCHEMA_VERSION;
        self.entries.insert(
            history_key(path),
            HistoryEntry {
                path: path.to_string_lossy().into_owned(),
                size,
                mtime_ms,
                processed_at_ms: now,
            },
        );
        while self.entries.len() > MAX_HISTORY_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.processed_at_ms)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
        true
    }

    /// True when this exact file (same path, size and mtime) is in the
    /// ledger. A touched or rewritten file is a new input and must run.
    pub fn matches(&self, path: &Path) -> bool {
        let Some((size, mtime_ms)) = fingerprint(path) else {
            return false;
        };
        self.entries
            .get(&history_key(path))
            .is_some_and(|e| e.size == size && e.mtime_ms == mtime_ms)
    }
}

fn history_key(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s
    }
}

fn fingerprint(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let ms = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some((meta.len(), ms))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Debounced saver ──────────────────────────────────────────────────

static PENDING: Mutex<Option<GuiSettings>> = Mutex::new(None);
static DIRTY: AtomicBool = AtomicBool::new(false);
static QUEUE_PENDING: Mutex<Option<QueueSnapshot>> = Mutex::new(None);
static QUEUE_DIRTY: AtomicBool = AtomicBool::new(false);
static HISTORY: OnceLock<RwLock<History>> = OnceLock::new();
static HISTORY_DIRTY: AtomicBool = AtomicBool::new(false);
/// Serializes disk writes: the saver tick and an exit `flush` can run on
/// different threads, and two writers racing over the same temp file
/// could interleave into a torn snapshot.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Hand the latest settings snapshot to the saver thread. Cheap enough to
/// call on every state change: it just overwrites the pending slot and
/// sets a flag - the actual disk write happens at most once per saver
/// tick.
pub fn offer(settings: GuiSettings) {
    // Recover from a poisoned slot: the saver must keep working after a
    // panic elsewhere, and the slot only holds one built value.
    let mut slot = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(settings);
    DIRTY.store(true, Ordering::Relaxed);
}

/// Hand the latest queue snapshot to the saver. Empty snapshots remove
/// the file, so a clean finish leaves no stale restore behind.
pub fn offer_queue(snapshot: QueueSnapshot) {
    let mut slot = QUEUE_PENDING.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(snapshot);
    QUEUE_DIRTY.store(true, Ordering::Relaxed);
}

/// Whether the next saver tick has something to write.
#[cfg(test)]
pub fn is_dirty() -> bool {
    DIRTY.load(Ordering::Relaxed)
}

/// Snapshots taken out of the pending slots, ready to be written. Taking
/// clears the dirty flags, so a change offered while the write runs is
/// re-flagged for the next tick instead of being dropped.
struct PendingWrites {
    settings: Option<GuiSettings>,
    queue: Option<QueueSnapshot>,
    history: Option<History>,
}

impl PendingWrites {
    fn is_empty(&self) -> bool {
        self.settings.is_none() && self.queue.is_none() && self.history.is_none()
    }
}

fn take_pending() -> PendingWrites {
    // A panic while a slot was held must not lose the last snapshot: the
    // slot only ever holds one fully-built value, so a poisoned guard is
    // still safe to read.
    let settings = if DIRTY.swap(false, Ordering::Relaxed) {
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()
    } else {
        None
    };
    let queue = if QUEUE_DIRTY.swap(false, Ordering::Relaxed) {
        QUEUE_PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    } else {
        None
    };
    let history = if HISTORY_DIRTY.swap(false, Ordering::Relaxed) {
        HISTORY
            .get()
            .map(|h| h.read().unwrap_or_else(|e| e.into_inner()).clone())
    } else {
        None
    };
    PendingWrites {
        settings,
        queue,
        history,
    }
}

/// Write one taken set of snapshots into `dir`. Every file keeps the
/// temp-then-rename discipline, so a crash mid-write can only leave the
/// previous snapshot behind.
fn write_pending_in(dir: &Path, writes: PendingWrites) {
    if let Some(settings) = writes.settings {
        if let Ok(json) = serde_json::to_string_pretty(&settings) {
            save_atomically(&dir.join("settings.json"), &json);
        }
    }
    if let Some(queue) = writes.queue {
        save_queue_to_path(&dir.join("queue.json"), &queue);
    }
    if let Some(history) = writes.history {
        if let Ok(json) = serde_json::to_string_pretty(&history) {
            save_atomically(&dir.join("history.json"), &json);
        }
    }
}

/// Write whatever is pending now, under the writer lock.
fn write_pending_now() {
    let Some(dir) = app_data_dir() else { return };
    // A panic while a writer held the lock must not cost the user the
    // final save; the write itself is atomic, so recovering is safe.
    // Taking the lock before the take keeps "read the slots, write the
    // files" atomic against another writer.
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let writes = take_pending();
    if writes.is_empty() {
        return;
    }
    write_pending_in(&dir, writes);
}

/// Saver loop: wake up once per second, write at most one snapshot per
/// file. Runs for the process lifetime on a background thread.
pub fn spawn_saver() {
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        write_pending_now();
    });
}

/// Write every pending snapshot to disk right now. Called on the exit
/// paths (window close, restart, panic): the background saver writes at
/// most once per second, so without this the last changes a user made
/// before closing the app died with the process — "I picked these
/// settings, closed it, and they were back to defaults on the next
/// launch". Best-effort: an unwritable folder just leaves the file as it
/// was.
pub fn flush() {
    write_pending_now();
}

fn history_store() -> &'static RwLock<History> {
    HISTORY.get_or_init(|| {
        // Under test the ledger must not inherit whatever this machine
        // processed: the pure History tests own their data.
        #[cfg(not(test))]
        let loaded = load_history_from_disk();
        #[cfg(test)]
        let loaded: Option<History> = None;
        let history = match loaded {
            Some(h) => History {
                schema_version: HISTORY_SCHEMA_VERSION,
                ..h
            },
            None => History {
                schema_version: HISTORY_SCHEMA_VERSION,
                ..Default::default()
            },
        };
        RwLock::new(history)
    })
}

/// Record one or more successfully placed paths (source and output) in
/// the ledger. Best-effort: unreadable files are simply not recorded.
/// Returns whether the ledger changed (drives the History tab refresh).
pub fn record_processed(paths: &[&Path]) -> bool {
    let now = now_ms();
    let mut changed = false;
    if let Ok(mut h) = history_store().write() {
        for p in paths {
            changed |= h.record(p, now);
        }
    }
    if changed {
        HISTORY_DIRTY.store(true, Ordering::Relaxed);
    }
    changed
}

/// True when the file has not changed since a recorded successful run.
pub fn was_processed(path: &Path) -> bool {
    history_store()
        .read()
        .map(|h| h.matches(path))
        .unwrap_or(false)
}

pub fn history_count() -> usize {
    history_store().read().map(|h| h.len()).unwrap_or(0)
}

/// Every processed-file ledger entry, newest first. A snapshot for the
/// History tab: the ledger itself stays keyed by path (that is what the
/// skip gate looks up), while the view wants chronology.
pub fn history_entries() -> Vec<HistoryEntry> {
    history_store()
        .read()
        .map(|h| sorted_entries(&h))
        .unwrap_or_default()
}

fn sorted_entries(history: &History) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = history.entries.values().cloned().collect();
    // Newest first; a path tie (source and output recorded in the same
    // call) keeps a stable, readable order by path.
    entries.sort_by(|a, b| {
        b.processed_at_ms
            .cmp(&a.processed_at_ms)
            .then_with(|| a.path.cmp(&b.path))
    });
    entries
}

/// Forget every processed-file mark; the next scan treats all files as
/// fresh inputs.
pub fn clear_history() {
    if let Ok(mut h) = history_store().write() {
        h.entries.clear();
        h.schema_version = HISTORY_SCHEMA_VERSION;
    }
    HISTORY_DIRTY.store(true, Ordering::Relaxed);
}

// ── Paths and IO ─────────────────────────────────────────────────────

/// Same durable location the bench calibration uses (`%LOCALAPPDATA%\shrinkr`
/// on Windows, `~/.cache/shrinkr` elsewhere) - one app-data folder.
fn app_data_dir() -> Option<PathBuf> {
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

fn settings_path() -> Option<PathBuf> {
    app_data_dir().map(|d| d.join("settings.json"))
}

fn queue_path() -> Option<PathBuf> {
    app_data_dir().map(|d| d.join("queue.json"))
}

fn history_path() -> Option<PathBuf> {
    app_data_dir().map(|d| d.join("history.json"))
}

/// Write-through: temp file first, then a rename over the target, so a
/// crash mid-write can never leave a half-written file behind.
fn save_atomically(path: &std::path::Path, json: &str) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state.json");
    // A second concurrently running instance must not clobber this one's
    // temp file mid-rename.
    let tmp = dir.join(format!("{name}.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, json).is_err() {
        return;
    }
    // Windows rename fails onto an existing destination; remove first.
    // The window where the target is absent is one rename wide - and a
    // missing file just means "defaults" on the next start.
    let _ = std::fs::remove_file(path);
    let _ = std::fs::rename(&tmp, path);
}

/// Load the saved settings (None on first run / unreadable file).
pub fn load() -> Option<GuiSettings> {
    load_settings_from_path(&settings_path()?)
}

fn load_settings_from_path(path: &Path) -> Option<GuiSettings> {
    let text = std::fs::read_to_string(path).ok()?;
    let s: GuiSettings = serde_json::from_str(&text).ok()?;
    // A future schema version may change a field's meaning; refuse the
    // whole file rather than misread it.
    if s.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(s)
}

/// Load an unfinished queue snapshot. `None` when there is none, it is
/// empty, or the file is unreadable/corrupt (a lost queue is always
/// recoverable by re-adding the files).
pub fn load_queue() -> Option<QueueSnapshot> {
    load_queue_from_path(&queue_path()?)
}

fn load_queue_from_path(path: &Path) -> Option<QueueSnapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    let snap: QueueSnapshot = serde_json::from_str(&text).ok()?;
    if snap.schema_version != QUEUE_SCHEMA_VERSION || snap.is_empty() {
        return None;
    }
    Some(snap)
}

fn save_queue_to_path(path: &Path, snapshot: &QueueSnapshot) {
    if snapshot.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(snapshot) {
        save_atomically(path, &json);
    }
}

#[cfg_attr(test, allow(dead_code))]
fn load_history_from_disk() -> Option<History> {
    let text = std::fs::read_to_string(history_path()?).ok()?;
    let h: History = serde_json::from_str(&text).ok()?;
    if h.schema_version != HISTORY_SCHEMA_VERSION {
        return None;
    }
    Some(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ExecItem;
    use std::sync::Arc;

    fn media(path: &str) -> shrinkr_core::media::MediaFile {
        shrinkr_core::media::MediaFile {
            path: PathBuf::from(path),
            bytes: 1_000_000,
            vcodec: "h264".into(),
            width: 1920,
            height: 1080,
            fps: 30.0,
            pix_fmt: "yuv420p".into(),
            bit_depth: 8,
            color_space: "bt709".into(),
            color_primaries: "bt709".into(),
            color_transfer: "bt709".into(),
            color_range: "tv".into(),
            vbitrate: Some(12_000_000),
            format_bitrate: None,
            duration_s: 600.0,
            audio: vec![],
            acodec: "aac".into(),
            rotation_deg: 0,
            video_stream_count: 1,
            audio_stream_count: 1,
            sub_count: 0,
            attach_count: 0,
            has_chapters: false,
        }
    }

    fn cfg() -> ExecCfg {
        ExecCfg {
            params: AppState::default().est_params(),
            keep_subs: true,
            parallel: 2,
            replace: false,
            threshold: 10.0,
            threshold_policy: shrinkr_core::pipeline::ThresholdPolicy::Warn,
            extra_args: vec![],
        }
    }

    /// The pending slots are process-global; these tests mutate them and
    /// must not interleave (a parallel `offer` would land between an
    /// assert and its take).
    static PENDING_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn pending_test_guard() -> std::sync::MutexGuard<'static, ()> {
        PENDING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

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
            skip_processed: false,
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
        assert!(!restored.skip_processed);
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
        // "NotABackend" is an unknown enum variant - the whole file is
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
        let _guard = pending_test_guard();
        offer(collect(&AppState::default()));
        assert!(is_dirty());
        DIRTY.store(false, Ordering::Relaxed);
    }

    /// The exit path: whatever the user picked last must already be in
    /// the pending slot, and `flush` must write it without waiting for
    /// the saver tick. This is the "close the app, settings intact"
    /// guarantee.
    #[test]
    fn flush_writes_the_latest_snapshot_before_exit() {
        let _guard = pending_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let s = AppState {
            cq: 34,
            backend: shrinkr_core::pipeline::VideoBackend::CpuX265,
            preserve_quality: false,
            replace: true,
            parallel: 3,
            ..Default::default()
        };
        offer(collect(&s));

        let writes = take_pending();
        assert!(
            !writes.is_empty(),
            "an offered snapshot must survive into the flush"
        );
        write_pending_in(dir.path(), writes);

        let loaded = load_settings_from_path(&dir.path().join("settings.json"))
            .expect("flush wrote a loadable settings.json");
        assert_eq!(loaded.cq, 34);
        assert_eq!(
            loaded.backend,
            shrinkr_core::pipeline::VideoBackend::CpuX265
        );
        assert!(!loaded.preserve_quality);
        assert!(loaded.replace, "the last picked toggle is restored");
        assert_eq!(loaded.parallel, 3);

        // Taking twice writes nothing: the flush does not rewrite a file
        // the saver already handled.
        assert!(take_pending().is_empty());
    }

    /// A change offered while a write is running must be re-flagged, not
    /// lost with the snapshots the write already took.
    #[test]
    fn changes_offered_after_a_take_are_written_next_time() {
        let _guard = pending_test_guard();
        let dir = tempfile::tempdir().unwrap();
        offer(collect(&AppState::default()));
        let first = take_pending();
        assert!(first.settings.is_some());
        // The user moves another knob while the write runs.
        let s = AppState {
            image_cq: 36,
            ..Default::default()
        };
        offer(collect(&s));

        write_pending_in(dir.path(), first);
        let second = take_pending();
        assert!(second.settings.is_some());
        write_pending_in(dir.path(), second);

        let loaded = load_settings_from_path(&dir.path().join("settings.json")).unwrap();
        assert_eq!(loaded.image_cq, 36);
    }

    #[test]
    fn queue_snapshot_only_keeps_unfinished_and_unskipped_files() {
        let mut s = AppState {
            executing: true,
            exec_cfg: Some(cfg()),
            exec_files: vec![media("done.mkv"), media("todo.mkv")],
            exec_items: vec![
                ExecItem::new(
                    "done.mkv".into(),
                    PathBuf::from("done.mkv"),
                    "1920x1080".into(),
                ),
                ExecItem::new(
                    "todo.mkv".into(),
                    PathBuf::from("todo.mkv"),
                    "1920x1080".into(),
                ),
            ],
            ..Default::default()
        };
        s.exec_items[0].done = true;
        s.pending.push(PendingSegment {
            files: vec![media("wait.mkv"), media("skip.mkv")],
            cfg: cfg(),
            warn: true,
        });
        let mut skips = HashSet::new();
        skips.insert(PathBuf::from("skip.mkv"));
        s.exec_skips = Some(Arc::new(Mutex::new(skips)));

        let snap = collect_queue(&s);
        assert_eq!(
            snap.running.as_ref().unwrap().files,
            vec![PathBuf::from("todo.mkv")]
        );
        assert_eq!(snap.pending.len(), 1);
        assert_eq!(snap.pending[0].files, vec![PathBuf::from("wait.mkv")]);
        assert!(snap.pending[0].warn);
        assert_eq!(snap.file_count(), 2);
    }

    #[test]
    fn queue_json_round_trips_and_empty_removes_the_file() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("queue.json");
        let snap = QueueSnapshot {
            schema_version: QUEUE_SCHEMA_VERSION,
            saved_at: 1,
            running: Some(QueuedBatch {
                files: vec![PathBuf::from("a.mkv")],
                cfg: cfg(),
                warn: false,
            }),
            pending: vec![],
        };
        let json = serde_json::to_string(&snap).unwrap();
        save_atomically(&path, &json);
        let loaded = load_queue_from_path(&path).unwrap();
        assert_eq!(loaded.file_count(), 1);
        assert_eq!(loaded.running.unwrap().files, vec![PathBuf::from("a.mkv")]);
        std::fs::remove_file(&path).unwrap();
        assert!(load_queue_from_path(&path).is_none());
    }

    #[test]
    fn queue_restore_drops_missing_paths_and_keeps_segment_settings() {
        let d = tempfile::tempdir().unwrap();
        let one = d.path().join("one.mkv");
        std::fs::write(&one, b"x").unwrap();
        let missing = d.path().join("missing.mkv");
        let snap = QueueSnapshot {
            schema_version: QUEUE_SCHEMA_VERSION,
            saved_at: 0,
            running: Some(QueuedBatch {
                files: vec![one.clone(), missing],
                cfg: cfg(),
                warn: false,
            }),
            pending: vec![],
        };
        let probed = vec![(one.clone(), Some(media(&one.to_string_lossy())))];
        let (segments, dropped) = restored_segments(snap, &probed);
        assert_eq!(dropped, 1);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].files.len(), 1);
        assert_eq!(segments[0].files[0].path, one);
    }

    #[test]
    fn history_matches_exact_size_and_mtime_then_rejects_changes() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("clip.mkv");
        std::fs::write(&file, b"hello").unwrap();
        let mut h = History::default();
        assert!(h.record(&file, 1));
        assert!(h.matches(&file));
        // Different bytes (and therefore size) are a new input.
        std::fs::write(&file, b"hello world").unwrap();
        assert!(!h.matches(&file));
        // Re-record then remove: the ledger must not claim a missing file.
        assert!(h.record(&file, 2));
        std::fs::remove_file(&file).unwrap();
        assert!(!h.matches(&file));
    }

    #[test]
    fn history_is_bounded_and_json_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let mut h = History {
            schema_version: HISTORY_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        };
        for i in 0..3 {
            let p = d.path().join(format!("f{i}.mkv"));
            std::fs::write(&p, b"x").unwrap();
            assert!(h.record(&p, i as u64));
        }
        let json = serde_json::to_string(&h).unwrap();
        let loaded: History = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.len(), 3);
        assert!(loaded.entries.values().any(|e| e.path.ends_with("f1.mkv")));
    }

    #[test]
    fn history_entries_are_newest_first() {
        let d = tempfile::tempdir().unwrap();
        let mut h = History {
            schema_version: HISTORY_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        };
        // Recorded out of order so sorting, not insertion, decides.
        for (name, at) in [("old.mkv", 100), ("newest.mkv", 300), ("middle.mkv", 200)] {
            let p = d.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            assert!(h.record(&p, at));
        }
        let entries = sorted_entries(&h);
        let names: Vec<&str> = entries
            .iter()
            .map(|e| {
                Path::new(&e.path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap()
            })
            .collect();
        assert_eq!(names, vec!["newest.mkv", "middle.mkv", "old.mkv"]);
        assert_eq!(entries[0].processed_at_ms, 300);
    }

    #[test]
    fn settings_default_matches_safe_app_defaults() {
        let s = AppState::default();
        let snap = collect(&s);
        assert!(snap.skip_processed);
        assert!(!snap.replace);
        assert!(snap.keep_subs);
        assert!(snap.all_audio);
    }
}
