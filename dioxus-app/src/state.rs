//! Shared UI state for the Dioxus frontend: worker threads in `jobs`
//! mutate this through a `Signal<AppState>` (signals are thread-safe).

use shrinkr_core::bench::BenchRow;
use shrinkr_core::convert::MediaKind;
use shrinkr_core::hw::HwCaps;
use shrinkr_core::log::{LevelFilter, LogEntry};
use shrinkr_core::media::MediaFile;
use shrinkr_core::pipeline::{
    estimate_file, preflight_auto_with, EstParams, NvencPreset, Preflight, ScalePolicy,
    ThresholdPolicy, VideoBackend,
};
use std::path::PathBuf;

/// The three video goals on the single speed↔size axis. The names state
/// the trade-off; position on the axis (Fastest → Balanced → Smallest)
/// is the explanation. Resolution is capability-driven: each goal maps
/// to the best encoder this machine can actually run, like Auto does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Goal {
    Fastest,
    Balanced,
    Smallest,
}

impl Goal {
    pub const ALL: [Goal; 3] = [Goal::Fastest, Goal::Balanced, Goal::Smallest];

    pub fn label(&self) -> &'static str {
        match self {
            Goal::Fastest => "Fastest",
            Goal::Balanced => "Balanced",
            Goal::Smallest => "Smallest",
        }
    }

    /// Second line shown on the segment before files are loaded, once
    /// per-goal estimates take over. Says the axis position in words.
    pub fn subtitle(&self) -> &'static str {
        match self {
            Goal::Fastest => "least shrink",
            Goal::Balanced => "default",
            Goal::Smallest => "most shrink",
        }
    }

    /// `(backend, nvenc preset, cq)` this goal runs as on this machine.
    /// Fastest keeps Balanced's CQ so quality stays constant across the
    /// fast half of the axis — the trade-off there is purely speed vs
    /// size (P4 encodes quicker than P5 at the same CQ, files come out
    /// bigger). Balanced rides AV1 NVENC where the trial encode passed
    /// (RTX 40/50): same speed class as HEVC NVENC, meaningfully smaller —
    /// free savings for the default goal, HEVC anywhere else for playback
    /// compatibility. Smallest means smallest output, period: SVT-AV1 is
    /// the most compression-efficient encoder in the toolbox (software,
    /// so slow — that's the cost the name admits), falling back through
    /// the hardware AV1/HEVC encoders to whatever this machine has.
    pub fn resolve(&self, caps: &HwCaps) -> (VideoBackend, NvencPreset, u32) {
        match self {
            Goal::Fastest => (VideoBackend::Auto, NvencPreset::P4, 28),
            Goal::Balanced => {
                if caps.av1_nvenc_usable {
                    (VideoBackend::Av1Nvenc, NvencPreset::P5, 28)
                } else {
                    (VideoBackend::Auto, NvencPreset::P5, 28)
                }
            }
            Goal::Smallest => {
                if caps.svt_av1 {
                    (VideoBackend::CpuAv1, NvencPreset::P5, 32)
                } else if caps.av1_nvenc_usable {
                    (VideoBackend::Av1Nvenc, NvencPreset::P6, 32)
                } else if caps.hevc_nvenc {
                    (VideoBackend::HevcNvenc, NvencPreset::P6, 32)
                } else if caps.vt_hevc {
                    (VideoBackend::VtHevc, NvencPreset::P5, 32)
                } else if caps.qsv_hevc {
                    (VideoBackend::QsvHevc, NvencPreset::P5, 32)
                } else if caps.amf_hevc {
                    (VideoBackend::AmfHevc, NvencPreset::P5, 32)
                } else {
                    (VideoBackend::Auto, NvencPreset::P6, 32)
                }
            }
        }
    }

    /// Whether the current settings triple sits on this goal's spot.
    /// The preset knob now maps onto every encoder family (NVENC P-series,
    /// Quick Sync named presets, SVT presets), so it is always part of the
    /// match; everything must match exactly, so manual knob edits read as
    /// custom settings with no segment highlighted.
    pub fn matches(
        &self,
        backend: VideoBackend,
        preset: NvencPreset,
        cq: u32,
        caps: &HwCaps,
    ) -> bool {
        let (b, p, c) = self.resolve(caps);
        backend == b && preset == p && cq == c
    }

    /// [`Goal::matches`] for a Lossless batch: the quality value is pinned
    /// by the checkbox, so segments match on encoder + preset alone and the
    /// axis keeps working as the speed↔size control it is there.
    pub fn matches_lossless(
        &self,
        backend: VideoBackend,
        preset: NvencPreset,
        caps: &HwCaps,
    ) -> bool {
        let (b, p, _) = self.resolve(caps);
        backend == b && preset == p
    }
}

/// Quality value the video encodes at while the Lossless box is checked:
/// the visually-lossless end of the shared slider. CRF/CQ 18 is the range
/// where the encode is indistinguishable from the source; below it, files
/// mostly grow with nothing visible to show for it.
pub const LOSSLESS_CQ: u32 = 18;

/// Measured-fps table for wall-time estimates. Shared by the batch
/// estimate and the per-goal segment estimates.
fn backend_fps(backend: VideoBackend) -> f64 {
    match backend {
        VideoBackend::CpuX264 => 90.0,
        VideoBackend::CpuX265 => 45.0,
        VideoBackend::CpuAv1 => 120.0, // SVT-AV1 preset 8, measured 145+ fps
        // AV1 hardware sits a notch under the HEVC engines on the same die;
        // the other families' HEVC is NVENC-class on their own machines.
        VideoBackend::Av1Nvenc | VideoBackend::QsvAv1 | VideoBackend::AmfAv1 => 500.0,
        _ => 600.0,
    }
}

/// Per-goal estimate over the video library: `estimate_file` is
/// analytical, so three of these cost about as much as the one batch
/// estimate and can run inline on every render.
#[derive(Clone, Copy, Debug, Default)]
pub struct GoalEstimate {
    pub total_b: u64,
    pub new_b: u64,
    pub secs: f64,
    /// Videos this goal would actually re-encode to a smaller file.
    /// Goals pass the threshold gate differently, so segment times
    /// differ even at equal savings — the count makes that visible.
    pub files: usize,
    /// Warn-verdict videos: below-threshold (often growing) estimates
    /// the goal would encode anyway and discard if the result comes out
    /// larger — they can't grow the kept output, only burn time, so
    /// they stay out of the shrink math and surface as a count on the
    /// segment line.
    pub warned: usize,
    /// Warn sizes, so tests can still compare a capped run's estimates
    /// against a released one (Lossless pins CQ away).
    pub warned_total_b: u64,
    pub warned_new_b: u64,
}

impl GoalEstimate {
    /// Net change of the *shrinking* subset as a fraction of its bytes
    /// (positive would mean growth; not reachable with the Shrink gate
    /// today, but never clamped in case the model shifts). Warn verdicts
    /// don't enter this — they can only grow, and the placement guard
    /// keeps their originals. `None` when nothing shrinks.
    pub fn saving_frac(&self) -> Option<f64> {
        if self.total_b == 0 {
            None
        } else {
            Some((self.new_b as f64 - self.total_b as f64) / self.total_b as f64)
        }
    }
}
use std::sync::{atomic::AtomicBool, Arc};

/// Encode settings snapshot for chained batches: files queued mid-run
/// re-run with exactly what the finished batch used.
#[derive(Clone, Debug)]
pub struct ExecCfg {
    pub params: EstParams,
    pub keep_subs: bool,
    pub parallel: usize,
    pub replace: bool,
    pub threshold: f64,
    pub threshold_policy: ThresholdPolicy,
    pub extra_args: Vec<String>,
}

/// Per-file encode progress. The group bar is the mean of these, so it
/// can never jump backwards no matter how workers interleave.
#[derive(Clone, Debug)]
pub struct ExecItem {
    pub name: String,
    /// Source path, so queue-anyway logic can tell which library files
    /// are already part of the running batch (names can collide).
    pub path: PathBuf,
    /// Source resolution ("1920x1080"; "?" when the file has no
    /// dimensions, e.g. audio).
    pub res: String,
    pub frac: f64,
    pub fps: f64,
    pub done: bool,
    pub failed: bool,
}

impl ExecItem {
    pub fn new(name: String, path: PathBuf, res: String) -> Self {
        Self {
            name,
            path,
            res,
            frac: 0.0,
            fps: 0.0,
            done: false,
            failed: false,
        }
    }

    pub fn status_text(&self) -> String {
        let base = if self.failed {
            "failed".to_string()
        } else if self.done {
            "done".to_string()
        } else if self.frac > 0.0 {
            format!("{:.0}%", self.frac * 100.0)
        } else {
            "queued".to_string()
        };
        base + &fps_suffix(self.fps)
    }
}

fn fps_suffix(fps: f64) -> String {
    if fps > 0.5 {
        format!(" · {fps:.0} fps")
    } else {
        String::new()
    }
}

pub struct AppState {
    pub targets: Vec<PathBuf>,
    pub files: Vec<MediaFile>,
    pub scanning: bool,
    pub probing: bool,
    pub probe_done: usize,
    pub probe_total: usize,
    /// Checked by default: keeps perceived audio/video quality intact by
    /// pinning quality to the visually-lossless end of the slider
    /// ([`LOSSLESS_CQ`]), resolution to Preserve, and audio to Off (stream
    /// copy, never re-encoded). Backend and preset stay user-controlled —
    /// they trade speed for size, not quality. The raw knobs keep their
    /// values, so unchecking restores the user's settings exactly.
    pub lossless: bool,
    pub backend: VideoBackend,
    /// Backend to restore when Audio-only is switched off (checked state
    /// is derived from `backend == CopyVideo`).
    pub last_video_backend: VideoBackend,
    pub nvenc_preset: NvencPreset,
    pub cq: u32,
    pub scale: ScalePolicy,
    /// Image-only quality/resolution, separate from the video knobs
    /// above: an image batch never shows NVENC/audio controls, and
    /// moving the video quality slider must not re-quality images.
    pub image_cq: u32,
    pub image_scale: ScalePolicy,
    pub image_preserve_format: bool,
    /// SVT-AV1 film-grain synthesis (grainy sources only; the control is
    /// shown when the ffmpeg build has SVT). Default on: statistically
    /// similar synthetic grain is the trade for much smaller files on
    /// exactly the sources nothing else helps.
    pub film_grain: bool,
    pub keep_subs: bool,
    pub min_saving_pct: f64,
    pub skip_efficient: bool,
    pub all_audio: bool,
    pub parallel: usize,
    pub replace: bool,
    /// Opus target in bps for re-encoded audio (`None` = Off: keep all
    /// audio tracks as-is). Drives plan, estimate and encode together.
    pub opus_bps: Option<u32>,
    /// Raw advanced free-text ffmpeg flags (parsed + validated at Process).
    pub extra_args: String,
    /// Raw target-size text for the solver (e.g. "400MB", "2x").
    pub target_text: String,
    pub target_solving: bool,
    pub executing: bool,
    pub exec_items: Vec<ExecItem>,
    /// The files behind `exec_items`, index-aligned. The worker owns the
    /// batch, but state keeps light clones so per-index events (failures)
    /// can resolve back to full media metadata — that's what retry runs on.
    pub exec_files: Vec<MediaFile>,
    /// Files that failed this run, full records captured at failure time.
    /// A failed encode placed nothing, so the snapshot's metadata is still
    /// what should run again. Accumulates across chained batches; cleared
    /// when a fresh Process run starts (Retry is not a reset — a retry's
    /// failures stay retryable).
    pub failed_files: Vec<MediaFile>,
    pub exec_total: usize,
    /// Files in this batch whose encode finished but produced a result that
    /// wasn't smaller than the source (discarded, original kept). Reported
    /// in the batch summary so "finished but nothing was written" explains
    /// itself.
    pub exec_discarded: usize,
    pub cancel: Option<Arc<AtomicBool>>,
    /// True while the run is paused: in-flight encodes are suspended
    /// (mirrors the core-level pause flag; flipped by the Pause button,
    /// cleared at batch start and ExecDone). UI label only — no worker
    /// decisions read this field.
    pub paused: bool,
    /// Files probed while a batch runs. Drained into a chained batch at
    /// ExecDone (same settings); discarded to the library on cancel.
    pub pending: Vec<MediaFile>,
    /// True when the user explicitly queued files via the mid-run Queue
    /// button: below-threshold files joined deliberately, so the chained
    /// batch runs warn instead of skip (the no-gain guard still keeps
    /// originals safe). Cleared once the chain starts or on cancel.
    pub pending_warn: bool,
    /// Settings snapshot of the running batch, for chained batches.
    pub exec_cfg: Option<ExecCfg>,
    /// Pump sender, stored so job handlers can start chained batches.
    pub job_tx: Option<futures_channel::mpsc::UnboundedSender<crate::jobs::JobMsg>>,
    pub log: Vec<LogEntry>,
    pub log_filter: LevelFilter,
    pub bench_running: bool,
    pub bench_rows: Vec<BenchRow>,
    pub bench_note: String,
    pub ffmpeg_ok: bool,
    pub ffprobe_ok: bool,
    // ── One-click ffmpeg fetch (header, when ffmpeg is missing) ──
    pub ffmpeg_fetching: bool,
    pub ffmpeg_fetch_pct: f64,
    // ── Convert tab ──
    pub convert_files: Vec<ConvertItem>,
    pub convert_scanning: bool,
    pub convert_executing: bool,
    pub convert_replace: bool,
    pub convert_cancel: Option<Arc<AtomicBool>>,
    pub convert_done: usize,
    // ── Self-update (GitHub Releases) ──
    pub update_checking: bool,
    pub update_available: Option<shrinkr_core::update::AvailableUpdate>,
    pub update_installing: bool,
    /// Installed version waiting for restart (on-disk exe already swapped).
    pub update_ready: Option<String>,
    pub update_error: Option<String>,
    pub update_checked_once: bool,
}

/// One file in the Convert tab.
#[derive(Clone, Debug)]
pub struct ConvertItem {
    pub path: PathBuf,
    pub bytes: u64,
    pub kind: MediaKind,
    pub src_ext: String,
    /// Probed media (None until the worker probes it).
    pub media: Option<MediaFile>,
    /// Chosen output extension (always a feasible target).
    pub target_ext: String,
    pub frac: f64,
    pub done: bool,
    pub failed: bool,
    pub detail: String,
}

impl ConvertItem {
    pub fn new(path: PathBuf, bytes: u64, kind: MediaKind, src_ext: String) -> Self {
        Self {
            path,
            bytes,
            kind,
            src_ext,
            media: None,
            target_ext: String::new(),
            frac: 0.0,
            done: false,
            failed: false,
            detail: String::new(),
        }
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }

    /// Feasible output extensions for this file with the current build.
    /// Purely capability-driven (same list before/after probing, so the
    /// dropdown never shifts under the user; documents narrow to their
    /// sub-family inside `feasible_targets`). Video files include audio
    /// extraction targets; silent videos hide them once probed.
    pub fn feasible_options(&self) -> Vec<shrinkr_core::convert::ConvertTarget> {
        let caps = shrinkr_core::convert::caps();
        shrinkr_core::convert::feasible_targets(self.kind, &self.src_ext, caps)
            .into_iter()
            .filter(|t| {
                if shrinkr_core::convert::is_audio_extract(self.kind, t.ext) {
                    match &self.media {
                        Some(m) => m.audio_stream_count > 0,
                        // Unprobed: offer extraction (probe arrives in ~ms
                        // and narrows the list if the video is silent).
                        None => true,
                    }
                } else {
                    true
                }
            })
            .collect()
    }

    pub fn feasible_exts(&self) -> Vec<String> {
        self.feasible_options()
            .into_iter()
            .map(|t| t.ext.to_string())
            .collect()
    }

    /// `→ 12.1 MB (−8%)` estimate label, or a probing placeholder.
    /// Documents carry no probe (ffprobe can't read them) — their
    /// estimate comes from the byte count alone.
    pub fn estimate_text(&self) -> String {
        if self.target_ext.is_empty() {
            return "probing…".to_string();
        }
        match &self.media {
            Some(m) => shrinkr_core::convert::estimate_label(m, self.kind, &self.target_ext),
            None => shrinkr_core::convert::estimate_label_for_bytes(
                self.bytes,
                self.kind,
                &self.target_ext,
            ),
        }
    }

    /// Ready to run: a feasible target picked, not done/failed, and —
    /// for ffmpeg kinds — probed. Documents need no probe (ffprobe
    /// cannot read them; LibreOffice does the work).
    pub fn runnable(&self) -> bool {
        !self.target_ext.is_empty()
            && !self.done
            && !self.failed
            && (self.media.is_some() || self.kind == MediaKind::Document)
    }

    pub fn status_text(&self) -> String {
        if self.failed {
            "failed".to_string()
        } else if self.done {
            "done".to_string()
        } else if self.frac > 0.0 {
            format!("{:.0}%", self.frac * 100.0)
        } else {
            "queued".to_string()
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            targets: vec![],
            files: vec![],
            scanning: false,
            probing: false,
            probe_done: 0,
            probe_total: 0,
            lossless: true,
            backend: VideoBackend::Auto,
            last_video_backend: VideoBackend::Auto,
            nvenc_preset: NvencPreset::P5,
            cq: 28,
            scale: ScalePolicy::Preserve,
            image_cq: 28,
            image_scale: ScalePolicy::Preserve,
            image_preserve_format: true,
            film_grain: true,
            keep_subs: false,
            all_audio: false,
            opus_bps: None,
            min_saving_pct: 10.0,
            skip_efficient: true,
            parallel: 2,
            replace: true,
            extra_args: String::new(),
            target_text: "2x".into(),
            target_solving: false,
            executing: false,
            exec_items: vec![],
            exec_files: vec![],
            failed_files: vec![],
            exec_total: 0,
            exec_discarded: 0,
            cancel: None,
            paused: false,
            pending: vec![],
            pending_warn: false,
            exec_cfg: None,
            job_tx: None,
            log: vec![LogEntry::new("Add folders and/or files, then Scan.".into())],
            log_filter: LevelFilter::all(),
            bench_running: false,
            bench_rows: vec![],
            bench_note: String::new(),
            ffmpeg_ok: tool_ok("ffmpeg"),
            ffprobe_ok: tool_ok("ffprobe"),
            ffmpeg_fetching: false,
            ffmpeg_fetch_pct: 0.0,
            convert_files: vec![],
            convert_scanning: false,
            convert_executing: false,
            convert_replace: false,
            convert_cancel: None,
            convert_done: 0,
            update_checking: false,
            update_available: None,
            update_installing: false,
            update_ready: None,
            update_error: None,
            update_checked_once: false,
        }
    }
}

pub fn tool_ok(prog: &str) -> bool {
    shrinkr_core::process::cmd(prog)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

impl AppState {
    /// The configured minimum-saving percentage. With
    /// [`Self::threshold_policy`] set to [`ThresholdPolicy::Warn`] this
    /// is advisory (plan warnings); with `Skip` it removes files.
    pub fn threshold(&self) -> f64 {
        self.min_saving_pct
    }

    /// Checked "Skip already-efficient files" removes below-threshold
    /// files; unchecked keeps them in the run and only warns.
    pub fn threshold_policy(&self) -> ThresholdPolicy {
        if self.skip_efficient {
            ThresholdPolicy::Skip
        } else {
            ThresholdPolicy::Warn
        }
    }

    /// `(shrink, warned, skipped)` over the probed library.
    pub fn plan_counts(&self) -> (usize, usize, usize) {
        let p = self.est_params();
        let t = self.threshold();
        let policy = self.threshold_policy();
        let mut counts = (0, 0, 0);
        for f in &self.files {
            match preflight_auto_with(f, &p, t, policy) {
                Preflight::Shrink { .. } => counts.0 += 1,
                Preflight::Warn { .. } => counts.1 += 1,
                Preflight::Skip { .. } => counts.2 += 1,
            }
        }
        counts
    }

    /// Settings that keep perceived quality intact, applied on top of the
    /// raw knobs while Lossless is checked. Only the three knobs that can
    /// visibly degrade the result are pinned: quality value, resolution
    /// and audio (re-encoding audio to Opus is the loudest quality loss a
    /// default batch could inflict).
    fn apply_lossless(&self, p: &mut EstParams) {
        if !self.lossless {
            return;
        }
        p.cq = LOSSLESS_CQ;
        p.scale = ScalePolicy::Preserve;
        p.opus_bps = None;
    }

    /// The CQ/CRF the video actually encodes at (Lossless pins it).
    pub fn effective_cq(&self) -> u32 {
        if self.lossless {
            LOSSLESS_CQ
        } else {
            self.cq
        }
    }

    /// The resolution policy the video actually runs with.
    pub fn effective_scale(&self) -> ScalePolicy {
        if self.lossless {
            ScalePolicy::Preserve
        } else {
            self.scale
        }
    }

    /// The audio target the video actually runs with (`None` = Off).
    pub fn effective_opus(&self) -> Option<u32> {
        if self.lossless {
            None
        } else {
            self.opus_bps
        }
    }

    pub fn est_params(&self) -> EstParams {
        let mut p = EstParams {
            backend: self.backend,
            preset: self.nvenc_preset,
            cq: self.cq,
            scale: self.scale,
            opus_bps: self.opus_bps,
            all_audio: self.all_audio,
            image_cq: self.image_cq,
            image_scale: self.image_scale,
            image_preserve_format: self.image_preserve_format,
            film_grain: self.film_grain,
        };
        self.apply_lossless(&mut p);
        p
    }

    /// Any still image in the library — drives which Pipeline settings
    /// sections are shown (image knobs appear only when they matter).
    pub fn has_images(&self) -> bool {
        self.files
            .iter()
            .any(|f| shrinkr_core::images::is_shrinkable_image(&f.path))
    }

    /// Any non-image (video) file in the library.
    pub fn has_videos(&self) -> bool {
        self.files
            .iter()
            .any(|f| !shrinkr_core::images::is_shrinkable_image(&f.path))
    }

    pub fn eligible(&self) -> Vec<usize> {
        let p = self.est_params();
        let t = self.threshold();
        let policy = self.threshold_policy();
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| preflight_auto_with(f, &p, t, policy).will_shrink())
            .map(|(i, _)| i)
            .collect()
    }

    /// Library files still addable to a running batch via the mid-run
    /// Queue button. Warn policy, so below-threshold files count too —
    /// the user pressed this deliberately; the no-gain guard keeps
    /// originals safe when the estimate was optimistic. Estimates use
    /// the running batch's settings snapshot (the chained batch encodes
    /// with it), so a mid-run slider change can't smuggle a file in that
    /// the chain's preflight would then reject for a different reason.
    /// Files already queued or already in the running batch are excluded
    /// by path.
    pub fn queueable(&self) -> Vec<usize> {
        let (p, t) = match self.exec_cfg.as_ref() {
            Some(cfg) => (cfg.params.clone(), cfg.threshold),
            None => (self.est_params(), self.threshold()),
        };
        let queued: std::collections::HashSet<_> = self
            .pending
            .iter()
            .map(|f| f.path.clone())
            .chain(self.exec_items.iter().map(|i| i.path.clone()))
            .collect();
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                !queued.contains(&f.path)
                    && preflight_auto_with(f, &p, t, ThresholdPolicy::Warn).will_shrink()
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// (eligible_input_bytes, new_bytes, byte delta, est_seconds). The
    /// delta is signed: positive means the estimate grows, negative means
    /// it shrinks — never clamped, so a growing plan reports its real
    /// increase instead of a misleading 0%. Skipped files count toward
    /// neither input nor output — the headline describes exactly the bytes
    /// Process will touch. Images use their own size model and a flat
    /// per-file time (they encode in well under a second; the
    /// frames-per-second model doesn't apply to single frames).
    pub fn estimate(&self) -> (u64, u64, i64, f64) {
        let p = self.est_params();
        let idx = self.eligible();
        let mut new_b = 0u64;
        let mut total_frames = 0f64;
        let mut image_count = 0usize;
        for &i in &idx {
            let f = &self.files[i];
            if shrinkr_core::images::is_shrinkable_image(&f.path) {
                new_b += shrinkr_core::images::estimate_image_bytes(
                    f,
                    p.image_cq,
                    p.image_scale,
                    p.image_preserve_format,
                );
                image_count += 1;
            } else {
                new_b += estimate_file(f, &p).new_bytes;
                total_frames += f.total_frames();
            }
        }
        let total_b: u64 = idx.iter().map(|&i| self.files[i].bytes).sum();
        let saved = new_b as i64 - total_b as i64;
        let fps = backend_fps(self.backend);
        let video_secs = total_frames / fps / (self.parallel.max(1) as f64);
        let image_secs = image_count as f64 * 0.35 / (self.parallel.max(1) as f64);
        (total_b, new_b, saved, video_secs + image_secs)
    }

    /// Per-goal estimate over the video library: the goal's encoder and
    /// quality substituted into the current settings, everything else
    /// (resolution, audio, images) as configured. These numbers are what
    /// the Goal segments display, so the speed↔size trade-off is stated
    /// in the batch's own bytes and minutes. Under Lossless the goal's CQ
    /// is overridden right back — all three segments share the pinned
    /// quality and differ only in encoder speed and efficiency.
    ///
    /// Only shrinking files enter the percentage: warn verdicts (below
    /// threshold, often growing at a pinned CQ 18) would otherwise read
    /// "+38%" on a shrink axis although the run discards those results
    /// and keeps the originals — they surface as a warned count instead.
    pub fn goal_estimate(&self, goal: Goal) -> GoalEstimate {
        let caps = shrinkr_core::hw::caps();
        let (backend, preset, cq) = goal.resolve(caps);
        let mut p = self.est_params();
        p.backend = backend;
        p.preset = preset;
        p.cq = cq;
        self.apply_lossless(&mut p);
        let t = self.threshold();
        let policy = self.threshold_policy();
        let mut est = GoalEstimate::default();
        for f in &self.files {
            if shrinkr_core::images::is_shrinkable_image(&f.path) {
                // Images don't differ across goals — they'd dilute the
                // segment contrast with three identical numbers.
                continue;
            }
            let verdict = preflight_auto_with(f, &p, t, policy);
            if !verdict.will_shrink() {
                continue;
            }
            est.secs += f.total_frames() / backend_fps(backend);
            if verdict.is_warn() {
                est.warned += 1;
                est.warned_total_b += f.bytes;
                est.warned_new_b += estimate_file(f, &p).new_bytes;
            } else {
                est.total_b += f.bytes;
                est.new_b += estimate_file(f, &p).new_bytes;
                est.files += 1;
            }
        }
        est.secs /= self.parallel.max(1) as f64;
        est
    }

    /// (filename, resolution, verdict) rows for the plan list. Resolution
    /// is "?" when the file has no dimensions. `Preflight::Warn` rows are
    /// part of the run but flagged for the user.
    pub fn preflight_rows(&self) -> Vec<(String, String, Preflight)> {
        let p = self.est_params();
        let t = self.threshold();
        let policy = self.threshold_policy();
        self.files
            .iter()
            .map(|f| {
                let name = f
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                (name, f.res_label(), preflight_auto_with(f, &p, t, policy))
            })
            .collect()
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    /// Finished items (done or failed).
    pub fn exec_done(&self) -> usize {
        self.exec_items
            .iter()
            .filter(|i| i.done || i.failed)
            .count()
    }

    /// Files that failed this run — leveraged straight out of the run's
    /// own records (`failed_files`): a failed encode placed nothing, so
    /// the snapshot's metadata is exactly what should run again.
    /// Accumulates across chained batches (their ExecDone swaps
    /// `exec_items`, but failures from the previous leg are counted here
    /// until a fresh Process resets the run).
    pub fn retry_files(&self) -> &[MediaFile] {
        &self.failed_files
    }

    /// Group progress: mean of per-file fractions. Monotonic by
    /// construction — per-file fractions only move forward.
    pub fn exec_overall(&self) -> f64 {
        if self.exec_items.is_empty() {
            0.0
        } else {
            self.exec_items.iter().map(|i| i.frac).sum::<f64>() / self.exec_items.len() as f64
        }
    }

    pub fn push_log(&mut self, line: String) {
        self.log.push(LogEntry::new(line));
        if self.log.len() > 300 {
            let excess = self.log.len() - 300;
            self.log.drain(..excess);
        }
    }

    /// Chronological, filter-honouring view for the log panel (newest last).
    /// Capped so the DOM stays small during long runs.
    pub fn visible_log(&self) -> Vec<LogEntry> {
        let n = self.log.len();
        let start = n.saturating_sub(200);
        self.log[start..]
            .iter()
            .filter(|e| self.log_filter.visible(e.level))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shrinkr_core::media::AudioTrack;

    fn app() -> AppState {
        // Defaults are lossless; give the raw knobs non-pinned values so
        // the tests prove the override (and that the knobs survive it).
        AppState {
            cq: 32,
            scale: ScalePolicy::Force720p,
            opus_bps: Some(96_000),
            ..Default::default()
        }
    }

    fn video() -> MediaFile {
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
                bitrate: Some(384_000),
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

    #[test]
    fn lossless_is_on_by_default_and_pins_the_quality_knobs() {
        let s = app();
        let p = s.est_params();
        assert_eq!(p.cq, LOSSLESS_CQ);
        assert_eq!(p.scale, ScalePolicy::Preserve);
        assert_eq!(p.opus_bps, None);
        // The raw knobs survive, so unchecking restores them exactly.
        assert_eq!(s.cq, 32);
        assert_eq!(s.scale, ScalePolicy::Force720p);
        assert_eq!(s.opus_bps, Some(96_000));
    }

    #[test]
    fn unchecking_lossless_releases_the_pins() {
        let mut s = app();
        s.lossless = false;
        let p = s.est_params();
        assert_eq!(p.cq, 32);
        assert_eq!(p.scale, ScalePolicy::Force720p);
        assert_eq!(p.opus_bps, Some(96_000));
    }

    #[test]
    fn lossless_goal_estimates_share_the_pinned_quality() {
        // goal_estimate substitutes the goal's own CQ after est_params —
        // it must re-apply the pin, or the segments would silently
        // estimate (and the axis promise) quality the run won't deliver.
        // Thresholds off: a cq18 estimate on a dense source may grow, and
        // the default skip policy would drop the file from the estimate
        // before the two plans could be compared.
        let mut s = app();
        s.min_saving_pct = 0.0;
        s.skip_efficient = false;
        s.files.push(video());
        let pinned = s.goal_estimate(Goal::Fastest);
        let released = {
            s.lossless = false;
            s.goal_estimate(Goal::Fastest)
        };
        // Growth estimates surface in the warn bucket, not the shrink
        // sum — the pinned run's file lands there; the released (cq28)
        // run shrinks it. Compare the per-file output sizes to keep the
        // original inequality: cq18 must estimate bigger.
        assert_eq!(pinned.warned, 1);
        assert_eq!(pinned.files, 0);
        assert!(released.files == 1 && released.warned == 0);
        assert!(
            pinned.warned_new_b > released.new_b,
            "cq{} estimate {} should exceed the cq28 plan {}",
            LOSSLESS_CQ,
            pinned.warned_new_b,
            released.new_b
        );
    }

    #[test]
    fn goal_segments_hide_growth_and_say_no_gain() {
        // The reason for the warn bucket: a pinned CQ 18 run over
        // already-tight sources once read "+38%" on a shrink axis. Growth
        // can never reach the displayed percentage (the run would discard
        // those results and keep the originals anyway).
        let mut s = app();
        s.min_saving_pct = 0.0;
        s.skip_efficient = false;
        s.files.push(video());
        let est = s.goal_estimate(Goal::Fastest);
        assert_eq!(est.warned_total_b, video().bytes);
        assert!(est.total_b == 0 && est.new_b == 0);
        assert!(est.saving_frac().is_none());
    }

    #[test]
    fn lossless_goal_matching_ignores_the_pinned_cq() {
        let caps = shrinkr_core::hw::HwCaps {
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
            cuvid_decoders: vec![],
            gpu_label: "test".into(),
            hwaccels: vec![],
        };
        // Full matches() demands the goal's CQ (28), which Lossless pins
        // away — the segment would never highlight. matches_lossless
        // compares encoder + preset only.
        assert!(!Goal::Fastest.matches(VideoBackend::Auto, NvencPreset::P4, LOSSLESS_CQ, &caps));
        assert!(Goal::Fastest.matches_lossless(VideoBackend::Auto, NvencPreset::P4, &caps));
        assert!(!Goal::Balanced.matches_lossless(VideoBackend::Auto, NvencPreset::P4, &caps));
        assert!(!Goal::Fastest.matches_lossless(VideoBackend::CpuAv1, NvencPreset::P4, &caps));
    }

    #[test]
    fn goals_resolve_per_hardware() {
        let mut nv40 = shrinkr_core::hw::HwCaps {
            cuda_hwaccel: true,
            hevc_nvenc: true,
            h264_nvenc: true,
            av1_nvenc_listed: true,
            av1_nvenc_usable: true,
            nvenc_b_ref_mode: true,
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
            cuvid_decoders: vec![],
            gpu_label: "RTX test".into(),
            hwaccels: vec!["cuda".into()],
        };
        // RTX 40: Balanced is the free-savings AV1 NVENC ride, Smallest
        // stays CPU SVT (the name promises minimum bytes, not speed).
        assert_eq!(
            Goal::Balanced.resolve(&nv40),
            (VideoBackend::Av1Nvenc, NvencPreset::P5, 28)
        );
        assert_eq!(
            Goal::Smallest.resolve(&nv40),
            (VideoBackend::CpuAv1, NvencPreset::P5, 32)
        );
        // Same box without SVT in the build: Smallest takes AV1 NVENC.
        nv40.svt_av1 = false;
        assert_eq!(
            Goal::Smallest.resolve(&nv40),
            (VideoBackend::Av1Nvenc, NvencPreset::P6, 32)
        );
        // A Mac: Balanced falls back to Auto (→ VideoToolbox HEVC inside
        // the ladder), Smallest resolves VideoToolbox when SVT is missing
        // or CPU SVT when present.
        let mut mac = shrinkr_core::hw::HwCaps {
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
            svt_av1: false,
            cuvid_decoders: vec![],
            gpu_label: "Apple test".into(),
            hwaccels: vec!["videotoolbox".into()],
        };
        assert_eq!(
            Goal::Balanced.resolve(&mac),
            (VideoBackend::Auto, NvencPreset::P5, 28)
        );
        assert_eq!(
            Goal::Smallest.resolve(&mac),
            (VideoBackend::VtHevc, NvencPreset::P5, 32)
        );
        mac.svt_av1 = true;
        assert_eq!(
            Goal::Smallest.resolve(&mac),
            (VideoBackend::CpuAv1, NvencPreset::P5, 32)
        );
    }

    #[test]
    fn lossless_batch_cannot_become_audio_only() {
        // Audio-only re-encodes the soundtrack — exactly what Lossless
        // forbids — so the effective audio target stays Off even with the
        // Copy-video backend picked.
        let mut s = app();
        s.backend = VideoBackend::CopyVideo;
        s.opus_bps = Some(192_000);
        assert_eq!(s.effective_opus(), None);
        assert!(!shrinkr_core::pipeline::forces_audio_reencode(
            s.backend,
            s.effective_opus()
        ));
    }

    #[test]
    fn queueable_counts_below_threshold_files_the_skip_policy_would_drop() {
        // A high threshold puts every file with any estimated saving
        // below it: the skip policy drops it from a run, but the mid-run
        // Queue button is a deliberate click, so warn policy counts it.
        let mut s = app();
        s.lossless = false;
        s.min_saving_pct = 99.0;
        s.files.push(video());
        assert!(s.eligible().is_empty());
        assert_eq!(s.queueable().len(), 1);
    }

    #[test]
    fn queueable_excludes_pending_and_running_files() {
        let mut s = app();
        s.lossless = false;
        s.min_saving_pct = 99.0;
        let mut second = video();
        second.path = std::path::PathBuf::from("t2.mp4");
        let mut third = video();
        third.path = std::path::PathBuf::from("t3.mp4");
        s.files = vec![video(), second.clone(), third];
        s.executing = true;
        s.exec_cfg = Some(ExecCfg {
            params: s.est_params(),
            keep_subs: true,
            parallel: 2,
            replace: true,
            threshold: 99.0,
            threshold_policy: ThresholdPolicy::Skip,
            extra_args: vec![],
        });
        // One file queued behind the batch, one pending in the queue.
        s.pending.push(second.clone());
        // The first video is the file currently encoding.
        s.exec_items.push(ExecItem::new(
            "t.mp4".into(),
            s.files[0].path.clone(),
            "1920x1080".into(),
        ));
        s.exec_items.push(ExecItem::new(
            "t2.mp4".into(),
            second.path,
            "1920x1080".into(),
        ));
        // Only the untouched third file is addable.
        assert_eq!(s.queueable(), vec![2]);
    }
}
