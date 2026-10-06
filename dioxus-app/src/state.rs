//! Shared UI state for the Dioxus frontend: worker threads in `jobs`
//! mutate this through a `Signal<AppState>` (signals are thread-safe).

use shrinkr_core::bench::BenchRow;
use shrinkr_core::convert::MediaKind;
use shrinkr_core::log::{LevelFilter, LogEntry};
use shrinkr_core::media::MediaFile;
use shrinkr_core::pipeline::{
    estimate_file, preflight_auto, EstParams, NvencPreset, Preflight, ScalePolicy, VideoBackend,
};
use std::path::PathBuf;
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
    pub extra_args: Vec<String>,
}

/// Per-file encode progress. The group bar is the mean of these, so it
/// can never jump backwards no matter how workers interleave.
#[derive(Clone, Debug)]
pub struct ExecItem {
    pub name: String,
    /// Source resolution ("1920x1080"; "?" when the file has no
    /// dimensions, e.g. audio).
    pub res: String,
    pub frac: f64,
    pub fps: f64,
    pub done: bool,
    pub failed: bool,
}

impl ExecItem {
    pub fn new(name: String, res: String) -> Self {
        Self {
            name,
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
    pub backend: VideoBackend,
    pub nvenc_preset: NvencPreset,
    pub cq: u32,
    pub scale: ScalePolicy,
    /// Image-only quality/resolution, separate from the video knobs
    /// above: an image batch never shows NVENC/audio controls, and
    /// moving the video quality slider must not re-quality images.
    pub image_cq: u32,
    pub image_scale: ScalePolicy,
    pub image_preserve_format: bool,
    pub keep_subs: bool,
    pub min_saving_pct: f64,
    pub skip_efficient: bool,
    pub all_audio: bool,
    pub parallel: usize,
    pub replace: bool,
    /// Opus target in bps for re-encoded audio (`None` = Off: keep all
    /// audio tracks as-is). Drives plan, estimate and encode together.
    pub opus_bps: Option<u32>,
    /// Raw advanced free-text ffmpeg flags (parsed + validated at Shrink).
    pub extra_args: String,
    /// Raw target-size text for the solver (e.g. "400MB", "2x").
    pub target_text: String,
    pub target_solving: bool,
    pub executing: bool,
    pub exec_items: Vec<ExecItem>,
    pub exec_total: usize,
    pub cancel: Option<Arc<AtomicBool>>,
    /// Files probed while a batch runs. Drained into a chained batch at
    /// ExecDone (same settings); discarded to the library on cancel.
    pub pending: Vec<MediaFile>,
    /// Settings snapshot of the running batch, for chained batches.
    pub exec_cfg: Option<ExecCfg>,
    /// Pump sender, stored so job handlers can start chained batches.
    pub job_tx: Option<futures_channel::mpsc::UnboundedSender<crate::jobs::JobMsg>>,
    pub log: Vec<LogEntry>,
    pub log_filter: LevelFilter,
    pub bench_running: bool,
    pub bench_rows: Vec<BenchRow>,
    pub bench_note: String,
    pub hw_summary: String,
    pub ffmpeg_ok: bool,
    pub ffprobe_ok: bool,
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
            backend: VideoBackend::Auto,
            nvenc_preset: NvencPreset::P5,
            cq: 28,
            scale: ScalePolicy::Preserve,
            image_cq: 28,
            image_scale: ScalePolicy::Preserve,
            image_preserve_format: true,
            keep_subs: true,
            all_audio: false,
            opus_bps: Some(shrinkr_core::pipeline::DEFAULT_OPUS_BPS),
            min_saving_pct: 10.0,
            skip_efficient: true,
            parallel: 2,
            replace: true,
            extra_args: String::new(),
            target_text: "2x".into(),
            target_solving: false,
            executing: false,
            exec_items: vec![],
            exec_total: 0,
            cancel: None,
            pending: vec![],
            exec_cfg: None,
            job_tx: None,
            log: vec![LogEntry::new("Add folders and/or files, then Scan.".into())],
            log_filter: LevelFilter::all(),
            bench_running: false,
            bench_rows: vec![],
            bench_note: String::new(),
            hw_summary: shrinkr_core::hw::caps().summary(),
            ffmpeg_ok: tool_ok("ffmpeg"),
            ffprobe_ok: tool_ok("ffprobe"),
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
    pub fn threshold(&self) -> f64 {
        if self.skip_efficient {
            self.min_saving_pct
        } else {
            0.0
        }
    }

    pub fn est_params(&self) -> EstParams {
        EstParams {
            backend: self.backend,
            preset: self.nvenc_preset,
            cq: self.cq,
            scale: self.scale,
            opus_bps: self.opus_bps,
            all_audio: self.all_audio,
            image_cq: self.image_cq,
            image_scale: self.image_scale,
            image_preserve_format: self.image_preserve_format,
        }
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
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(preflight_auto(f, &p, t), Preflight::Shrink { .. }))
            .map(|(i, _)| i)
            .collect()
    }

    /// (eligible_input_bytes, new_bytes, saved_bytes, est_seconds).
    /// Skipped files count toward neither input nor output — the headline
    /// describes exactly the bytes Shrink will touch. Images use their own
    /// size model and a flat per-file time (they encode in well under a
    /// second; the frames-per-second model doesn't apply to single frames).
    pub fn estimate(&self) -> (u64, u64, u64, f64) {
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
        let saved = total_b.saturating_sub(new_b);
        let fps = match self.backend {
            VideoBackend::CpuX264 => 90.0,
            VideoBackend::CpuX265 => 45.0,
            VideoBackend::CpuAv1 => 120.0, // SVT-AV1 preset 8, measured 145+ fps
            _ => 600.0,
        };
        let video_secs = total_frames / fps / (self.parallel.max(1) as f64);
        let image_secs = image_count as f64 * 0.35 / (self.parallel.max(1) as f64);
        (
            total_b,
            new_b,
            saved,
            video_secs + image_secs,
        )
    }

    /// (filename, resolution, will_shrink, reason) rows for the plan
    /// list. Resolution is "?" when the file has no dimensions.
    pub fn preflight_rows(&self) -> Vec<(String, String, bool, String)> {
        let p = self.est_params();
        let t = self.threshold();
        self.files
            .iter()
            .map(|f| {
                let name = f
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                let res = f.res_label();
                match preflight_auto(f, &p, t) {
                    Preflight::Shrink { reason } => (name, res, true, reason),
                    Preflight::Skip { reason } => (name, res, false, reason),
                }
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
