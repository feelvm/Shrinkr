//! Background orchestration for the Dioxus frontend.
//!
//! Worker threads never touch UI state: they send [`JobMsg`] over a
//! futures channel to a UI-thread coroutine that applies them to the
//! `Signal<AppState>`. (Dioxus 0.7 signals use unsync storage, so this
//! separation is required — and it keeps every race in one place.)

use crate::state::{tool_ok, AppState};
use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;
use shrinkr_core::bench;
use shrinkr_core::ffmpeg::{place_output, EncodeBackend, FfmpegBackend};
use shrinkr_core::media::{human_bytes, probe_file, MediaFile};
use shrinkr_core::pipeline::{preflight_auto_with, EstParams, Preflight, ThresholdPolicy};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering, Arc};
use std::thread;
use std::time::Instant;

/// How many files in a row may be encoded and then discarded (result not
/// smaller than the source) before the batch stops itself. A library that
/// is already efficiently encoded hits this on every single file, and
/// grinding through the whole thing writes nothing — one real run spent
/// 7 hours that way. One success resets the streak, so a mixed library
/// still runs to completion.
const NO_GAIN_STOP_AFTER: usize = 10;

#[derive(Debug)]
pub enum JobMsg {
    TargetsAdded(Vec<PathBuf>),
    ScanDone(Vec<PathBuf>),
    ProbeOne {
        done: usize,
        total: usize,
        file: Option<MediaFile>,
        err: Option<String>,
    },
    ProbeDone,
    ExecProgress {
        job: usize,
        frac: f64,
        fps: f64,
    },
    /// A queued file is below the min-saving threshold but the user chose
    /// to run it anyway; surfaces the warning without failing the job.
    ExecWarn {
        msg: String,
    },
    ExecFileDone {
        job: usize,
        detail: String,
    },
    /// The encode finished but its result was not smaller than the source,
    /// so it was thrown away and the original kept. Distinct from an
    /// error: nothing went wrong, there was simply nothing to gain — the
    /// usual outcome for files that are already efficiently encoded.
    ExecNoGain {
        job: usize,
        detail: String,
    },
    /// A file was still encoding when the run ended (user cancel, or the
    /// repeated-no-gain stop). Neither success nor failure — there is
    /// nothing wrong to report, so it must not surface as an error.
    ExecAborted {
        job: usize,
    },
    ExecDone {
        freed: i64,
        elapsed_s: f64,
        /// True when the batch stopped itself after
        /// [`NO_GAIN_STOP_AFTER`] consecutive discards, rather than the
        /// user cancelling.
        stopped_no_gain: bool,
    },
    ExecError {
        job: Option<usize>,
        msg: String,
    },
    BenchLog {
        line: String,
    },
    BenchDone {
        rows: Vec<shrinkr_core::bench::BenchRow>,
        recommendation: Option<String>,
    },
    TargetLog {
        line: String,
    },
    TargetFailed {
        msg: String,
    },
    TargetDone {
        name: String,
        crf: u32,
        predicted: u64,
        reachable: bool,
    },
    ConvertAdded(Vec<PathBuf>),
    ConvertScanStarted {
        n: usize,
    },
    ConvertOne {
        item: crate::state::ConvertItem,
        err: Option<String>,
    },
    ConvertScanDone,
    ConvertProgress {
        job: usize,
        frac: f64,
    },
    ConvertFileDone {
        job: usize,
        detail: String,
    },
    ConvertDone {
        elapsed_s: f64,
    },
    ConvertError {
        job: Option<usize>,
        msg: String,
    },
    UpdateChecked(Result<Option<shrinkr_core::update::AvailableUpdate>, String>),
    UpdateInstalled(Result<String, String>),
    FfmpegFetchProgress(f64),
    FfmpegFetched(Result<String, String>),
}

fn send(tx: &UnboundedSender<JobMsg>, msg: JobMsg) {
    let _ = tx.unbounded_send(msg);
}

/// One worker: walk targets, report hits, then probe each.
/// Keeps scan→probe ordering without UI-thread round-trips.
pub fn start_scan(tx: UnboundedSender<JobMsg>, targets: Vec<PathBuf>) {
    thread::spawn(move || {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut hits: Vec<PathBuf> = vec![];
        for t in &targets {
            match std::fs::metadata(t) {
                Ok(m) if m.is_dir() => {
                    for e in walkdir::WalkDir::new(t).into_iter().filter_map(|e| e.ok()) {
                        if !e.file_type().is_file() {
                            continue;
                        }
                        if let Some(ext) = e.path().extension().and_then(|s| s.to_str()) {
                            let l = ext.to_lowercase();
                            let hit = shrinkr_core::media::MEDIA_EXTS.contains(&l.as_str())
                                || shrinkr_core::images::IMAGE_SHRINK_EXTS.contains(&l.as_str());
                            if hit {
                                if seen.insert(e.path().to_path_buf()) {
                                    hits.push(e.path().to_path_buf());
                                }
                            }
                        }
                    }
                }
                Ok(_) => {
                    if seen.insert(t.clone()) {
                        hits.push(t.clone());
                    }
                }
                Err(_) => continue,
            }
        }
        hits.sort();
        send(&tx, JobMsg::ScanDone(hits.clone()));
        // Parallel probe: ffprobe is
        // latency-bound, so a few workers hide it almost linearly.
        // `done` counts completions; arrival order may differ from path order.
        let total = hits.len();
        let workers = shrinkr_core::media::probe_parallelism().min(total.max(1));
        let next = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let hits = Arc::new(hits);
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let tx = tx.clone();
            let next = next.clone();
            let done = done.clone();
            let hits = hits.clone();
            handles.push(thread::spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= hits.len() {
                    break;
                }
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                match probe_file(&hits[i]) {
                    Ok(mf) => send(
                        &tx,
                        JobMsg::ProbeOne {
                            done: n,
                            total,
                            file: Some(mf),
                            err: None,
                        },
                    ),
                    Err(e) => send(
                        &tx,
                        JobMsg::ProbeOne {
                            done: n,
                            total,
                            file: None,
                            err: Some(format!("{}: {:#}", hits[i].display(), e)),
                        },
                    ),
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        send(&tx, JobMsg::ProbeDone);
    });
}

/// Result of one file: either an output was placed, or the encode was
/// thrown away because it didn't beat the source. Both are "expected"
/// outcomes — only `Err` is a failure.
enum JobOutcome {
    Placed { saved: i64, detail: String },
    NoGain { detail: String },
}

#[allow(clippy::too_many_arguments)]
fn run_one_job(
    tx: &UnboundedSender<JobMsg>,
    f: MediaFile,
    params: EstParams,
    keep_subs: bool,
    threshold: f64,
    threshold_policy: ThresholdPolicy,
    tmp_root: &Path,
    job_idx: usize,
    replace: bool,
    extra_args: Vec<String>,
    cancel: &Arc<AtomicBool>,
) -> Result<JobOutcome, String> {
    match preflight_auto_with(&f, &params, threshold, threshold_policy) {
        Preflight::Skip { reason } => {
            return Err(format!("skip {}: {}", f.path.display(), reason));
        }
        Preflight::Warn { reason } => send(
            tx,
            JobMsg::ExecWarn {
                msg: format!("warn {}: {}", f.path.display(), reason),
            },
        ),
        Preflight::Shrink { .. } => {}
    }
    // Still images take the image branch: dedicated encoder, temp
    // extension and placement ext; the video pipeline never sees them.
    if shrinkr_core::images::is_shrinkable_image(&f.path) {
        return run_one_image_job(tx, f, params, tmp_root, job_idx, replace, cancel);
    }
    let stem = f.path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let tmp_out = tmp_root.join(format!("{}-{}.mkv", stem, job_idx));
    let be = FfmpegBackend;
    let fname = f
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let r = be.encode(
        &f,
        &tmp_out,
        params.backend,
        params.preset,
        params.cq,
        params.scale,
        keep_subs,
        &extra_args,
        params.opus_bps,
        params.all_audio,
        &|p| {
            // frac>=0 carries position; fps-only lines (frac<0) refresh
            // the rate while keeping the last position.
            send(
                tx,
                JobMsg::ExecProgress {
                    job: job_idx,
                    frac: p.frac,
                    fps: p.fps,
                },
            );
        },
        cancel,
    );
    let er = r?;
    // The preflight estimate is only a prediction. Never replace a source
    // with a file that isn't actually smaller — warn mode deliberately
    // queues below-threshold files, and the no-gain guard keeps that safe.
    // Audio-only passes are the exception: the video is copied untouched
    // and the audio was re-encoded on request, so a larger file is the
    // intended result, not a failed encode.
    if params.backend != shrinkr_core::pipeline::VideoBackend::CopyVideo
        && er.output_bytes >= f.bytes
    {
        let _ = std::fs::remove_file(&tmp_out);
        return Ok(JobOutcome::NoGain {
            detail: format!(
                "skip {fname}: no gain — encoded {} but the source is {}; original kept. \
                 Re-encoding already-compressed video usually grows it.",
                human_bytes(er.output_bytes),
                human_bytes(f.bytes)
            ),
        });
    }
    let scale_note = match er.scaled_to {
        Some((w, h)) => format!(" scale_cuda→{}x{}", w, h),
        None => String::new(),
    };
    let (saved, dest) = place_output(&f.path, &tmp_out, replace, "mkv")?;
    let dest_name = dest.file_name().and_then(|x| x.to_str()).unwrap_or("?");
    let detail = format!(
        "done {}: {} → {} ({:.0}%) in {:.0}s, {:.0} fps, {:.2}x realtime [{} {}{}, hwdec={}, audio={}, orig={}]",
        fname,
        human_bytes(f.bytes),
        human_bytes(er.output_bytes),
        er.ratio * 100.0,
        er.elapsed_s,
        er.avg_fps,
        er.realtime,
        er.encoder,
        er.preset,
        scale_note,
        er.hw_decode,
        er.audio_mode,
        if replace {
            format!("replaced→{dest_name}")
        } else {
            format!("kept, wrote {dest_name}")
        }
    );
    Ok(JobOutcome::Placed { saved, detail })
}

/// One still image through the image pipeline: plan → temp encode →
/// place next to the original with the target extension. The caller's
/// preflight already said Shrink; a stale plan (impossible in-process)
/// degrades to a loud skip. No progress stream: stills encode in well
/// under a second.
fn run_one_image_job(
    _tx: &UnboundedSender<JobMsg>,
    f: MediaFile,
    params: EstParams,
    tmp_root: &Path,
    job_idx: usize,
    replace: bool,
    cancel: &Arc<AtomicBool>,
) -> Result<JobOutcome, String> {
    use shrinkr_core::images::{encode_image_with_fallback, plan_image, ImageOutcome};
    let fname = f
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let mut plan = plan_image(&f, params.image_preserve_format)
        .map_err(|reason| format!("skip {fname}: {reason}"))?;
    let stem = f.path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let tmp_out = tmp_root.join(format!("{}-{}.{}", stem, job_idx, plan.target_ext));
    let _ = std::fs::remove_file(&tmp_out);
    // The fallback may switch the plan to WebP; the returned path is the
    // temp file that actually holds the winning encode.
    let (tmp_final, outcome) = encode_image_with_fallback(
        &f,
        &mut plan,
        params.image_cq,
        params.image_scale,
        &tmp_out,
        cancel,
        params.image_preserve_format,
    )?;
    let res = match outcome {
        ImageOutcome::Done(r) => r,
        ImageOutcome::NoGain { output_bytes } => {
            let _ = std::fs::remove_file(&tmp_out);
            return Ok(JobOutcome::NoGain {
                detail: format!(
                    "skip {fname}: no gain — encoded {} but the source is {}; original kept",
                    human_bytes(output_bytes),
                    human_bytes(f.bytes)
                ),
            });
        }
    };
    let (saved, dest) = place_output(&f.path, &tmp_final, replace, plan.target_ext)?;
    let dest_name = dest.file_name().and_then(|x| x.to_str()).unwrap_or("?");
    let detail = format!(
        "done {}: {} → {} ({:.0}%) in {:.1}s [{} {}, {} orig={}]",
        fname,
        human_bytes(f.bytes),
        human_bytes(res.output_bytes),
        res.ratio * 100.0,
        res.elapsed_s,
        res.encoder,
        res.quality,
        plan.note,
        if replace {
            format!("replaced→{dest_name}")
        } else {
            format!("kept, wrote {dest_name}")
        }
    );
    Ok(JobOutcome::Placed { saved, detail })
}

#[allow(clippy::too_many_arguments)]
pub fn start_exec(
    tx: UnboundedSender<JobMsg>,
    files: Vec<MediaFile>,
    params: EstParams,
    keep_subs: bool,
    parallel: usize,
    replace: bool,
    threshold: f64,
    threshold_policy: ThresholdPolicy,
    extra_args: Vec<String>,
    cancel: Arc<AtomicBool>,
) {
    // Caller guarantees non-empty files; the worker always ends with ExecDone.
    thread::spawn(move || {
        let t0 = Instant::now();
        // Never start a batch into a folder this process can't write to:
        // every file would fail on its output instead. `work_root` probes
        // temp and app data, and explains a sandboxed/restricted launch
        // (which can write nowhere in the user profile) when both fail.
        let tmp_root = match shrinkr_core::ffmpeg::work_root() {
            Ok(d) => d,
            Err(e) => {
                send(
                    &tx,
                    JobMsg::ExecError {
                        job: None,
                        msg: format!("work folder unusable, so every encode would fail: {e}"),
                    },
                );
                send(
                    &tx,
                    JobMsg::ExecDone {
                        freed: 0,
                        elapsed_s: 0.0,
                        stopped_no_gain: false,
                    },
                );
                return;
            }
        };
        // Work-stealing pool: workers pull
        // file indices from a shared queue so stragglers can't idle a batch.
        use std::sync::atomic::AtomicI64;
        let files = Arc::new(files);
        let next = Arc::new(AtomicUsize::new(0));
        let freed = Arc::new(AtomicI64::new(0));
        let no_gain_streak = Arc::new(AtomicUsize::new(0));
        let stopped_no_gain = Arc::new(AtomicBool::new(false));
        let extra_args = Arc::new(extra_args);
        let mut handles = Vec::with_capacity(parallel.max(1));
        for _ in 0..parallel.max(1) {
            let tx = tx.clone();
            let files = files.clone();
            let next = next.clone();
            let freed = freed.clone();
            let no_gain_streak = no_gain_streak.clone();
            let stopped_no_gain = stopped_no_gain.clone();
            let extra_args = extra_args.clone();
            let tmp_root = tmp_root.clone();
            let cancel = cancel.clone();
            handles.push(thread::spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= files.len() {
                    break;
                }
                if cancel.load(Ordering::Relaxed) {
                    continue;
                }
                match run_one_job(
                    &tx,
                    files[i].clone(),
                    params,
                    keep_subs,
                    threshold,
                    threshold_policy,
                    &tmp_root,
                    i,
                    replace,
                    extra_args.as_ref().clone(),
                    &cancel,
                ) {
                    Ok(JobOutcome::Placed { saved, detail }) => {
                        no_gain_streak.store(0, Ordering::Relaxed);
                        freed.fetch_add(saved, Ordering::Relaxed);
                        send(&tx, JobMsg::ExecFileDone { job: i, detail });
                    }
                    Ok(JobOutcome::NoGain { detail }) => {
                        // A library that is already efficiently encoded
                        // discards *every* file: the first run of that kind
                        // burned 7 hours to write nothing. Stop shortly
                        // after the pattern is clear instead of encoding
                        // the rest for the same outcome.
                        let streak = no_gain_streak.fetch_add(1, Ordering::Relaxed) + 1;
                        send(&tx, JobMsg::ExecNoGain { job: i, detail });
                        if streak >= NO_GAIN_STOP_AFTER {
                            stopped_no_gain.store(true, Ordering::Relaxed);
                            cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(e) if e.contains("cancelled") => {
                        // The run ended under this file (user cancel or the
                        // repeated-no-gain stop). Not an error, and the
                        // batch summary already explains why.
                        send(&tx, JobMsg::ExecAborted { job: i });
                    }
                    Err(e) => {
                        // Name the file: "all backends failed: …" without a
                        // filename leaves no way to find the offender.
                        let name = files[i]
                            .path
                            .file_name()
                            .and_then(|x| x.to_str())
                            .unwrap_or("?");
                        send(
                            &tx,
                            JobMsg::ExecError {
                                job: Some(i),
                                msg: format!("{name}: {e}"),
                            },
                        );
                    }
                }
            }));
        }
        for h in handles {
            if h.join().is_err() {
                send(
                    &tx,
                    JobMsg::ExecError {
                        job: None,
                        msg: "worker panic".into(),
                    },
                );
            }
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let stopped = stopped_no_gain.load(Ordering::Relaxed);
        if cancel.load(Ordering::Relaxed) && !stopped {
            send(
                &tx,
                JobMsg::ExecError {
                    job: None,
                    msg: "Cancelled.".into(),
                },
            );
        }
        send(
            &tx,
            JobMsg::ExecDone {
                freed: freed.load(Ordering::Relaxed),
                elapsed_s: elapsed,
                stopped_no_gain: stopped,
            },
        );
    });
}

pub fn start_bench(
    tx: UnboundedSender<JobMsg>,
    m: MediaFile,
    cq: u32,
    scale: shrinkr_core::pipeline::ScalePolicy,
    keep_subs: bool,
) {
    thread::spawn(move || {
        let out_dir = shrinkr_core::ffmpeg::work_root()
            .map(|d| d.join("bench"))
            .unwrap_or_else(|_| std::env::temp_dir().join("shrinkr-bench"));
        let rows = bench::run_on_file(&m, &out_dir, cq, scale, keep_subs, false, &|line| {
            send(
                &tx,
                JobMsg::BenchLog {
                    line: line.to_string(),
                },
            );
        });
        let rec = bench::recommend(&rows);
        send(
            &tx,
            JobMsg::BenchDone {
                rows,
                recommendation: rec,
            },
        );
    });
}

#[allow(clippy::too_many_arguments)]
pub fn start_target_solve(
    tx: UnboundedSender<JobMsg>,
    file: MediaFile,
    backend: shrinkr_core::pipeline::VideoBackend,
    preset: shrinkr_core::pipeline::NvencPreset,
    cq: u32,
    scale: shrinkr_core::pipeline::ScalePolicy,
    keep_subs: bool,
    extra_args: Vec<String>,
    opus_bps: Option<u32>,
    all_audio: bool,
    spec: shrinkr_core::target::TargetSpec,
) {
    use shrinkr_core::target::solve_crf_for_file;
    thread::spawn(move || {
        let name = file
            .path
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("?")
            .to_string();
        let work = shrinkr_core::ffmpeg::work_root()
            .map(|d| d.join("target"))
            .unwrap_or_else(|_| std::env::temp_dir().join("shrinkr-target"));
        let r = solve_crf_for_file(
            &file,
            backend,
            preset,
            cq,
            scale,
            keep_subs,
            &extra_args,
            opus_bps,
            all_audio,
            &spec,
            &work,
            &|line| {
                send(
                    &tx,
                    JobMsg::TargetLog {
                        line: line.to_string(),
                    },
                );
            },
        );
        match r {
            Ok(o) => send(
                &tx,
                JobMsg::TargetDone {
                    name,
                    crf: o.crf,
                    predicted: o.predicted_bytes,
                    reachable: o.reachable,
                },
            ),
            Err(e) => send(
                &tx,
                JobMsg::TargetFailed {
                    msg: format!("target solve failed for {name}: {e}"),
                },
            ),
        }
    });
}

pub fn cancel_exec(state: Signal<AppState>) {
    if let Some(c) = state.read().cancel.clone() {
        c.store(true, Ordering::Relaxed);
    }
}

pub fn cancel_convert(state: Signal<AppState>) {
    if let Some(c) = state.read().convert_cancel.clone() {
        c.store(true, Ordering::Relaxed);
    }
}

/// Check GitHub Releases for a newer version (worker thread).
pub fn start_update_check(tx: UnboundedSender<JobMsg>, current_version: String) {
    thread::spawn(move || {
        let r = shrinkr_core::update::check_for_update(&current_version);
        match r {
            Ok(v) => send(&tx, JobMsg::UpdateChecked(Ok(v))),
            Err(e) => send(&tx, JobMsg::UpdateChecked(Err(format!("{e:#}")))),
        }
    });
}

/// Download + install the latest release in place (worker thread).
/// Caller must ensure no encode/convert batch is running.
pub fn start_update_install(tx: UnboundedSender<JobMsg>, current_version: String) {
    thread::spawn(move || {
        let r = shrinkr_core::update::install_update(&current_version);
        match r {
            Ok(v) => send(&tx, JobMsg::UpdateInstalled(Ok(v))),
            Err(e) => send(&tx, JobMsg::UpdateInstalled(Err(format!("{e:#}")))),
        }
    });
}

/// One-click FFmpeg fetch into the executable's directory (worker thread).
/// Streams progress; the shell cleanup happens in core.
pub fn start_ffmpeg_fetch(tx: UnboundedSender<JobMsg>) {
    thread::spawn(move || {
        let progress_tx = tx.clone();
        let r = shrinkr_core::ffmpeg_fetch::fetch(move |done, total| {
            if let Some(total) = total.filter(|t| *t > 0) {
                send(
                    &progress_tx,
                    JobMsg::FfmpegFetchProgress((done as f64 / total as f64).min(1.0)),
                );
            }
        });
        match r {
            Ok(p) => send(
                &tx,
                JobMsg::FfmpegFetched(Ok(p.to_string_lossy().into_owned())),
            ),
            Err(e) => send(&tx, JobMsg::FfmpegFetched(Err(format!("{e:#}")))),
        }
    });
}

/// Walk + probe dropped/picked paths for the Convert tab. Directories
/// expand to convertible files; unsupported files are reported and
/// skipped. Each probed file arrives as `ConvertOne` with its default
/// (first feasible) target already selected.
pub fn start_convert_scan(tx: UnboundedSender<JobMsg>, paths: Vec<PathBuf>) {
    use crate::state::ConvertItem;
    use shrinkr_core::convert::{collect_inputs, kind_for, CONVERT_EXTS};
    thread::spawn(move || {
        send(&tx, JobMsg::ConvertScanStarted { n: paths.len() });
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut hits: Vec<PathBuf> = vec![];
        for p in &paths {
            match std::fs::metadata(p) {
                Ok(m) if m.is_dir() => {
                    for hit in collect_inputs(p) {
                        if seen.insert(hit.clone()) {
                            hits.push(hit);
                        }
                    }
                }
                Ok(_) => {
                    let ext = p
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if CONVERT_EXTS.contains(&ext.as_str()) {
                        if seen.insert(p.clone()) {
                            hits.push(p.clone());
                        }
                    } else {
                        send(
                            &tx,
                            JobMsg::ConvertError {
                                job: None,
                                msg: format!(
                                    "skip {}: .{ext} is not a convertible media type",
                                    p.display()
                                ),
                            },
                        );
                    }
                }
                Err(_) => continue,
            }
        }
        hits.sort();
        send(&tx, JobMsg::ConvertAdded(hits.clone()));
        for p in hits {
            let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let probed = probe_file(&p).ok();
            let kind = kind_for(&p, probed.as_ref());
            match kind {
                Some(k) => {
                    let src_ext = p
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let mut item = ConvertItem::new(p, bytes, k, src_ext.clone());
                    item.media = probed;
                    // Default target: first feasible (remux-first order).
                    // Files with no feasible target are reported, not queued.
                    let exts = item.feasible_exts();
                    match exts.first() {
                        Some(t) => {
                            item.target_ext = t.clone();
                            send(&tx, JobMsg::ConvertOne { item, err: None });
                        }
                        None => {
                            let why = if k == shrinkr_core::convert::MediaKind::Document {
                                format!(
                                    "no convertible output for .{} — install LibreOffice (soffice on PATH) for document conversion",
                                    item.src_ext
                                )
                            } else {
                                format!(
                                    "no convertible output for .{} ({}) with this ffmpeg build",
                                    item.src_ext,
                                    k.label()
                                )
                            };
                            send(
                                &tx,
                                JobMsg::ConvertOne {
                                    item,
                                    err: Some(why),
                                },
                            );
                        }
                    }
                }
                None => send(
                    &tx,
                    JobMsg::ConvertError {
                        job: None,
                        msg: format!("skip {}: could not determine media type", p.display()),
                    },
                ),
            }
        }
        send(&tx, JobMsg::ConvertScanDone);
    });
}

#[allow(clippy::too_many_arguments)]
fn run_one_convert(
    tx: &UnboundedSender<JobMsg>,
    item: crate::state::ConvertItem,
    job_idx: usize,
    replace: bool,
    cancel: &Arc<AtomicBool>,
) -> Result<String, String> {
    use shrinkr_core::convert::{place_converted_output, run_convert, run_document_convert};
    if item.target_ext.is_empty() {
        return Err(format!("skip {}: no output type selected", item.name()));
    }
    // Feasibility re-check at run time (ffmpeg build can't change, but
    // the target could have gone stale): never attempt the impossible.
    let feasible = item.feasible_exts();
    if !feasible.contains(&item.target_ext) {
        return Err(format!(
            "skip {}: .{} is not convertible from .{} here",
            item.name(),
            item.target_ext,
            item.src_ext
        ));
    }
    let tmp_root = std::env::temp_dir().join("shrinkr-convert");
    let tmp_out = tmp_root.join(format!("conv-{job_idx}.tmp.{}", item.target_ext));
    shrinkr_core::ffmpeg::ensure_output_dir(&tmp_out)?;
    let _ = std::fs::remove_file(&tmp_out);
    // Documents need no probe (ffprobe can't read them) and run through
    // LibreOffice instead of ffmpeg.
    if item.kind == shrinkr_core::convert::MediaKind::Document {
        let er = run_document_convert(
            &item.path,
            &item.target_ext,
            &tmp_out,
            &|frac| {
                send(&tx, JobMsg::ConvertProgress { job: job_idx, frac });
            },
            cancel,
        )?;
        let (saved, dest) =
            place_converted_output(&item.path, &tmp_out, &item.target_ext, replace)?;
        let _ = std::fs::remove_file(&tmp_out);
        return Ok(format!(
            "done {}: {} → {} (office) in {:.0}s [{}] orig={}",
            item.name(),
            human_bytes(item.bytes),
            human_bytes(er.output_bytes),
            er.elapsed_s,
            dest.file_name().and_then(|x| x.to_str()).unwrap_or("?"),
            if replace {
                format!("replaced, saved {}", human_bytes(saved.max(0) as u64))
            } else {
                "kept".to_string()
            }
        ));
    }
    let Some(m) = item.media.clone() else {
        return Err(format!("skip {}: probe failed", item.name()));
    };
    let r = run_convert(
        &m,
        item.kind,
        &item.target_ext,
        &tmp_out,
        &|frac| {
            send(&tx, JobMsg::ConvertProgress { job: job_idx, frac });
        },
        cancel,
    );
    let er = r?;
    let (saved, dest) = place_converted_output(&item.path, &tmp_out, &item.target_ext, replace)?;
    let _ = std::fs::remove_file(&tmp_out);
    let mode = if shrinkr_core::convert::is_audio_extract(item.kind, &item.target_ext) {
        if er.copied {
            "extracted audio (remux)"
        } else {
            "extracted audio"
        }
    } else if er.copied {
        "remux"
    } else {
        "re-encoded"
    };
    Ok(format!(
        "done {}: {} → {} ({}) in {:.0}s [{}] orig={}",
        item.name(),
        human_bytes(item.bytes),
        human_bytes(er.output_bytes),
        mode,
        er.elapsed_s,
        dest.file_name().and_then(|x| x.to_str()).unwrap_or("?"),
        if replace {
            format!("replaced, saved {}", human_bytes(saved.max(0) as u64))
        } else {
            "kept".to_string()
        }
    ))
}

/// Run the Convert queue with a small work-stealing pool (conversion is
/// usually I/O- or encoder-bound; 2 workers mirror the shrink default).
pub fn start_convert_exec(
    tx: UnboundedSender<JobMsg>,
    files: Vec<crate::state::ConvertItem>,
    indices: Vec<usize>,
    replace: bool,
    cancel: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let t0 = Instant::now();
        let parallel = 2usize.min(indices.len().max(1));
        let files = Arc::new(files);
        let queue = Arc::new(indices);
        let next = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(parallel);
        for _ in 0..parallel {
            let tx = tx.clone();
            let files = files.clone();
            let queue = queue.clone();
            let next = next.clone();
            let cancel = cancel.clone();
            handles.push(thread::spawn(move || loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= queue.len() {
                    break;
                }
                let i = queue[k];
                if cancel.load(Ordering::Relaxed) {
                    continue;
                }
                match run_one_convert(&tx, files[i].clone(), i, replace, &cancel) {
                    Ok(detail) => send(&tx, JobMsg::ConvertFileDone { job: i, detail }),
                    Err(e) => send(
                        &tx,
                        JobMsg::ConvertError {
                            job: Some(i),
                            msg: e,
                        },
                    ),
                }
            }));
        }
        for h in handles {
            if h.join().is_err() {
                send(
                    &tx,
                    JobMsg::ConvertError {
                        job: None,
                        msg: "worker panic".into(),
                    },
                );
            }
        }
        if cancel.load(Ordering::Relaxed) {
            send(
                &tx,
                JobMsg::ConvertError {
                    job: None,
                    msg: "Cancelled.".into(),
                },
            );
        }
        send(
            &tx,
            JobMsg::ConvertDone {
                elapsed_s: t0.elapsed().as_secs_f64(),
            },
        );
    });
}

/// Apply one job message to UI state. Runs on the UI thread (coroutine).
pub fn apply_msg(state: &mut Signal<AppState>, msg: JobMsg) {
    let mut s = state.write();
    match msg {
        JobMsg::TargetsAdded(paths) => {
            for p in paths {
                if !s.targets.contains(&p) {
                    s.targets.push(p);
                }
            }
            // Never wipe the library mid-run: it backs the plan lists and
            // the pending queue holds its own clones.
            if !s.executing {
                s.files.clear();
            }
        }
        JobMsg::ScanDone(paths) => {
            s.scanning = false;
            if paths.is_empty() {
                s.push_log("No media files found.".into());
            } else {
                s.probing = true;
                s.probe_done = 0;
                s.probe_total = paths.len();
                s.push_log(format!(
                    "Found {} media files — probing codecs…",
                    paths.len()
                ));
            }
        }
        JobMsg::ProbeOne {
            done,
            total,
            file,
            err,
        } => {
            s.probe_done = done;
            s.probe_total = total;
            if let Some(f) = file {
                // Same targets re-scanned mid-run would otherwise duplicate
                // the library and re-queue finished files.
                if !s.files.iter().any(|x| x.path == f.path) {
                    s.files.push(f.clone());
                    // Probed while a batch runs: join its queue instead of
                    // waiting for the next manual Process (preflight-filtered
                    // now; the worker re-checks at run time).
                    if s.executing {
                        let name = f
                            .path
                            .file_name()
                            .and_then(|x| x.to_str())
                            .unwrap_or("?")
                            .to_string();
                        let p = s.est_params();
                        // Join with the running batch's own settings, so a
                        // mid-run scan can't flip warn-only into skipping.
                        let (t, policy) = s
                            .exec_cfg
                            .as_ref()
                            .map(|c| (c.threshold, c.threshold_policy))
                            .unwrap_or((s.threshold(), s.threshold_policy()));
                        let verdict = preflight_auto_with(&f, &p, t, policy);
                        if verdict.will_shrink() {
                            s.pending.push(f);
                            let waiting = s.pending.len();
                            if verdict.is_warn() {
                                s.push_log(format!(
                                    "queued {name} with a warning ({waiting} waiting): {}",
                                    verdict.reason()
                                ));
                            } else {
                                s.push_log(format!(
                                    "queued {name} ({waiting} waiting behind the running batch)"
                                ));
                            }
                        } else {
                            s.push_log(format!("{name} would skip — in library, not queued"));
                        }
                    }
                }
            }
            if let Some(e) = err {
                s.push_log(format!("probe warn: {}", e));
            }
        }
        JobMsg::ProbeDone => {
            s.probing = false;
            if s.executing {
                s.push_log("Scan finished — new files join the run queue.".into());
                return;
            }
            let n_files = s.files.len();
            let total = s.total_bytes();
            let t = s.threshold();
            let (_, warned, skipped) = s.plan_counts();
            if s.skip_efficient {
                s.push_log(format!(
                    "Probed {} files, {} total ({} would skip at {:.0}% threshold).",
                    n_files,
                    human_bytes(total),
                    skipped,
                    t
                ));
            } else {
                s.push_log(format!(
                    "Probed {} files, {} total ({} below {:.0}% will warn, {} skip).",
                    n_files,
                    human_bytes(total),
                    warned,
                    t,
                    skipped
                ));
            }
        }
        JobMsg::ExecWarn { msg } => s.push_log(msg),
        JobMsg::ExecProgress { job, frac, fps } => {
            if let Some(item) = s.exec_items.get_mut(job) {
                // Fractions only move forward; fps-only lines (frac<0)
                // refresh the rate and keep the last position.
                if frac >= 0.0 {
                    item.frac = frac.clamp(0.0, 1.0).max(item.frac);
                }
                if fps > 0.0 {
                    item.fps = fps;
                }
            }
        }
        JobMsg::ExecFileDone { job, detail } => {
            if let Some(item) = s.exec_items.get_mut(job) {
                item.done = true;
                item.frac = 1.0;
            }
            s.push_log(detail);
        }
        JobMsg::ExecNoGain { job, detail } => {
            // Encoded, but the result wasn't smaller, so nothing was placed.
            // Not an error — count it for the batch summary.
            if let Some(item) = s.exec_items.get_mut(job) {
                item.done = true;
                item.frac = 1.0;
            }
            s.exec_discarded += 1;
            s.push_log(detail);
        }
        JobMsg::ExecAborted { job } => {
            // Still running when the batch ended: no result to report and
            // nothing to explain per file (the summary does).
            if let Some(item) = s.exec_items.get_mut(job) {
                item.done = true;
                item.frac = 1.0;
            }
        }
        JobMsg::ExecDone {
            freed,
            elapsed_s,
            stopped_no_gain,
        } => {
            let cancelled =
                stopped_no_gain || s.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
            if cancelled {
                // Pending files stay in the library, unrun.
                let n = s.pending.len();
                s.pending.clear();
                s.executing = false;
                if stopped_no_gain {
                    let discarded = s.exec_discarded;
                    s.exec_discarded = 0;
                    // Only mention the queue when there is one.
                    let queued = if n == 0 {
                        String::new()
                    } else {
                        format!(" {n} queued file(s) kept in the library for later.")
                    };
                    s.push_log(format!(
                        "Stopped after {:.0}s: {} file(s) in a row were encoded and then discarded because each result was larger than its source. This library is already efficiently encoded — nothing here can shrink at these settings. Leave 'Skip already-efficient files' checked (or lower the quality target) before running it again.{queued}",
                        elapsed_s, discarded
                    ));
                } else {
                    let queued = if n == 0 {
                        String::new()
                    } else {
                        format!(" {n} queued file(s) kept in the library, not run.")
                    };
                    s.push_log(format!(
                        "Cancelled after {:.0}s. Freed {}.{queued}",
                        elapsed_s,
                        human_bytes(freed.max(0) as u64)
                    ));
                }
                return;
            }
            if !s.pending.is_empty() {
                // Chain: files queued mid-run go next, same settings.
                let files = std::mem::take(&mut s.pending);
                let Some(cfg) = s.exec_cfg.clone() else {
                    s.executing = false;
                    s.push_log(format!(
                        "warn: {} queued file(s) lost — no run config stored",
                        files.len()
                    ));
                    return;
                };
                let Some(tx) = s.job_tx.clone() else {
                    s.executing = false;
                    s.push_log(format!(
                        "warn: {} queued file(s) lost — no job channel",
                        files.len()
                    ));
                    return;
                };
                s.exec_items = files
                    .iter()
                    .map(|f| {
                        crate::state::ExecItem::new(
                            f.path
                                .file_name()
                                .and_then(|x| x.to_str())
                                .unwrap_or("?")
                                .to_string(),
                            f.res_label(),
                        )
                    })
                    .collect();
                s.exec_total = files.len();
                s.exec_discarded = 0;
                let cancel = Arc::new(AtomicBool::new(false));
                s.cancel = Some(cancel.clone());
                s.push_log(format!(
                    "Queued batch: starting {} more file(s) with the same settings …",
                    files.len()
                ));
                start_exec(
                    tx,
                    files,
                    cfg.params,
                    cfg.keep_subs,
                    cfg.parallel,
                    cfg.replace,
                    cfg.threshold,
                    cfg.threshold_policy,
                    cfg.extra_args,
                    cancel,
                );
                return;
            }
            s.executing = false;
            for item in &mut s.exec_items {
                // Belt and braces: a finished batch leaves no item half-done.
                if !item.failed {
                    item.done = true;
                    item.frac = 1.0;
                }
            }
            // The common confusing outcome: everything encoded, nothing
            // written. Say why, once, in the summary instead of leaving the
            // user with "finished but no files and no logs".
            if s.exec_discarded > 0 {
                let discarded = s.exec_discarded;
                let total = s.exec_total;
                s.push_log(format!(
                    "{} of {} file(s) were discarded after encoding: the result was not smaller than the source, so the original was kept. Re-encoding an already-efficient file usually grows it — keep 'Skip already-efficient files' checked to skip these up front.",
                    discarded, total
                ));
                s.exec_discarded = 0;
            }
            s.push_log(format!(
                "Done in {:.0}s. Freed {}. Re-scan to confirm.",
                elapsed_s,
                human_bytes(freed.max(0) as u64)
            ));
        }
        JobMsg::ExecError { job, msg } => {
            if let Some(job) = job {
                if let Some(item) = s.exec_items.get_mut(job) {
                    item.failed = true;
                }
            }
            s.push_log(format!("warn: {}", msg));
        }
        JobMsg::BenchLog { line } => {
            s.push_log(line);
        }
        JobMsg::TargetLog { line } => {
            s.push_log(line);
        }
        JobMsg::TargetFailed { msg } => {
            s.target_solving = false;
            s.push_log(format!("warn: {msg}"));
        }
        JobMsg::TargetDone {
            name,
            crf,
            predicted,
            reachable,
        } => {
            s.target_solving = false;
            s.cq = crf;
            s.push_log(format!(
                "done target solve on {name}: quality slider set to {crf}, predicted {}",
                human_bytes(predicted)
            ));
            if !reachable {
                s.push_log(
                    "warn: target out of reach at max quality cost — closest reported, raise the target or downscale".into(),
                );
            }
        }
        JobMsg::ConvertScanStarted { n } => {
            s.convert_scanning = true;
            if n > 0 {
                s.push_log(format!("Collecting {n} item(s) for conversion…"));
            }
        }
        JobMsg::ConvertAdded(paths) => {
            if paths.is_empty() && !s.convert_scanning {
                s.push_log("No convertible media files found.".into());
            } else if !paths.is_empty() {
                s.push_log(format!(
                    "Found {} convertible file(s) — probing…",
                    paths.len()
                ));
            }
        }
        JobMsg::ConvertOne { item, err } => {
            if let Some(e) = err {
                s.push_log(format!("probe warn: {}: {}", item.name(), e));
            } else {
                // Same path re-added mid-run: keep one row.
                if !s.convert_files.iter().any(|x| x.path == item.path) {
                    s.convert_files.push(item);
                }
            }
        }
        JobMsg::ConvertScanDone => {
            s.convert_scanning = false;
            let ready = s
                .convert_files
                .iter()
                .filter(|f| !f.target_ext.is_empty() && !f.done && !f.failed)
                .count();
            s.push_log(format!("Convert ready: {} file(s) convertible.", ready));
        }
        JobMsg::ConvertProgress { job, frac } => {
            if let Some(item) = s.convert_files.get_mut(job) {
                item.frac = frac.clamp(0.0, 1.0).max(item.frac);
            }
        }
        JobMsg::ConvertFileDone { job, detail } => {
            if let Some(item) = s.convert_files.get_mut(job) {
                item.done = true;
                item.frac = 1.0;
                item.detail = detail.clone();
            }
            s.convert_done += 1;
            s.push_log(detail);
        }
        JobMsg::ConvertDone { elapsed_s, .. } => {
            let cancelled = s
                .convert_cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed));
            s.convert_executing = false;
            if cancelled {
                s.push_log(format!("Convert cancelled after {:.0}s.", elapsed_s));
            } else {
                let done = s.convert_files.iter().filter(|f| f.done).count();
                let failed = s.convert_files.iter().filter(|f| f.failed).count();
                // Log lanes are classified from the text itself: "failed"
                // would put a clean run in the red ERR lane, while a "done"
                // prefix lands it green next to the per-file lines.
                let summary = if failed == 0 {
                    format!("done: {done} file(s) converted in {elapsed_s:.0}s.")
                } else {
                    format!("Convert done in {elapsed_s:.0}s: {done} converted, {failed} failed.")
                };
                s.push_log(summary);
            }
        }
        JobMsg::ConvertError { job, msg } => {
            if let Some(job) = job {
                if let Some(item) = s.convert_files.get_mut(job) {
                    item.failed = true;
                }
                s.convert_done += 1;
            }
            s.push_log(format!("warn: {}", msg));
        }
        JobMsg::BenchDone {
            rows,
            recommendation,
        } => {
            s.bench_running = false;
            s.bench_rows = rows;
            if let Some(r) = recommendation {
                s.bench_note = format!("Recommended default: {}", r);
                let note = s.bench_note.clone();
                s.push_log(note);
            } else {
                s.bench_note = "No NVENC row succeeded — check GPU/driver.".into();
                let note = s.bench_note.clone();
                s.push_log(note);
            }
        }
        JobMsg::UpdateChecked(r) => {
            s.update_checking = false;
            s.update_checked_once = true;
            match r {
                Ok(Some(u)) => {
                    s.update_available = Some(u.clone());
                    s.update_error = None;
                    s.push_log(format!(
                        "Update available: v{} (running v{}) — press Update to install.",
                        u.version,
                        env!("CARGO_PKG_VERSION")
                    ));
                }
                Ok(None) => {
                    s.update_available = None;
                    s.update_error = None;
                    s.push_log(format!("Up to date (v{}).", env!("CARGO_PKG_VERSION")));
                }
                Err(e) => {
                    s.update_error = Some(e.clone());
                    s.push_log(format!("warn: update check failed: {e}"));
                }
            }
        }
        JobMsg::UpdateInstalled(r) => {
            s.update_installing = false;
            match r {
                Ok(v) => {
                    s.update_ready = Some(v.clone());
                    s.update_available = None;
                    s.update_error = None;
                    s.push_log(format!(
                        "Update installed: v{v} — press Restart to relaunch into the new version."
                    ));
                }
                Err(e) => {
                    s.update_error = Some(e.clone());
                    s.push_log(format!("ERROR: update failed: {e}"));
                }
            }
        }
        JobMsg::FfmpegFetchProgress(p) => {
            s.ffmpeg_fetch_pct = p;
        }
        JobMsg::FfmpegFetched(r) => {
            s.ffmpeg_fetching = false;
            s.ffmpeg_fetch_pct = 0.0;
            match r {
                Ok(p) => {
                    // Re-check through the same path the app uses (sidecar
                    // resolution now finds the downloaded copies).
                    s.ffmpeg_ok = tool_ok("ffmpeg");
                    s.ffprobe_ok = tool_ok("ffprobe");
                    s.push_log(format!("FFmpeg downloaded to {p} — ready."));
                }
                Err(e) => {
                    s.push_log(format!("ERROR: ffmpeg download failed: {e}"));
                }
            }
        }
    }
}
