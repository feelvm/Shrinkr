//! Background orchestration for the Dioxus frontend.
//!
//! Worker threads never touch UI state: they send [`JobMsg`] over a
//! futures channel to a UI-thread coroutine that applies them to the
//! `Signal<AppState>`. (Dioxus 0.7 signals use unsync storage, so this
//! separation is required — and it keeps every race in one place.)

use crate::state::AppState;
use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;
use shrinkr_core::bench;
use shrinkr_core::ffmpeg::{place_output, EncodeBackend, FfmpegBackend};
use shrinkr_core::media::{human_bytes, probe_file, MediaFile};
use shrinkr_core::pipeline::{preflight, EstParams, Preflight};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering, Arc};
use std::thread;
use std::time::Instant;

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
    ExecFileDone {
        job: usize,
        detail: String,
    },
    ExecDone {
        freed: i64,
        elapsed_s: f64,
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
                            if shrinkr_core::media::MEDIA_EXTS
                                .contains(&ext.to_lowercase().as_str())
                            {
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

#[allow(clippy::too_many_arguments)]
fn run_one_job(
    tx: &UnboundedSender<JobMsg>,
    f: MediaFile,
    params: EstParams,
    keep_subs: bool,
    threshold: f64,
    tmp_root: &Path,
    job_idx: usize,
    replace: bool,
    extra_args: Vec<String>,
    cancel: &Arc<AtomicBool>,
) -> Result<(i64, String), String> {
    if let Preflight::Skip { reason } = preflight(&f, &params, threshold) {
        return Err(format!("skip {}: {}", f.path.display(), reason));
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
    let scale_note = match er.scaled_to {
        Some((w, h)) => format!(" scale_cuda→{}x{}", w, h),
        None => String::new(),
    };
    let (saved, dest) = place_output(&f.path, &tmp_out, replace)?;
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
    Ok((saved, detail))
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
    extra_args: Vec<String>,
    cancel: Arc<AtomicBool>,
) {
    // Caller guarantees non-empty files; the worker always ends with ExecDone.
    thread::spawn(move || {
        let t0 = Instant::now();
        let tmp_root = std::env::temp_dir().join("shrinkr");
        let _ = std::fs::create_dir_all(&tmp_root);
        // Work-stealing pool: workers pull
        // file indices from a shared queue so stragglers can't idle a batch.
        use std::sync::atomic::AtomicI64;
        let files = Arc::new(files);
        let next = Arc::new(AtomicUsize::new(0));
        let freed = Arc::new(AtomicI64::new(0));
        let extra_args = Arc::new(extra_args);
        let mut handles = Vec::with_capacity(parallel.max(1));
        for _ in 0..parallel.max(1) {
            let tx = tx.clone();
            let files = files.clone();
            let next = next.clone();
            let freed = freed.clone();
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
                    &tmp_root,
                    i,
                    replace,
                    extra_args.as_ref().clone(),
                    &cancel,
                ) {
                    Ok((saved, detail)) => {
                        freed.fetch_add(saved, Ordering::Relaxed);
                        send(&tx, JobMsg::ExecFileDone { job: i, detail });
                    }
                    Err(e) => {
                        send(
                            &tx,
                            JobMsg::ExecError {
                                job: Some(i),
                                msg: e,
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
        if cancel.load(Ordering::Relaxed) {
            send(
                &tx,
                JobMsg::ExecError {
                    job: None,
                    msg: "Cancelled.".into(),
                },
            );
        }
        let elapsed = t0.elapsed().as_secs_f64();
        send(
            &tx,
            JobMsg::ExecDone {
                freed: freed.load(Ordering::Relaxed),
                elapsed_s: elapsed,
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
        let out_dir = std::env::temp_dir().join("shrinkr-bench");
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
        let work = std::env::temp_dir().join("shrinkr-target");
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
                            send(&tx, JobMsg::ConvertOne { item, err: Some(why) });
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
    let _ = std::fs::create_dir_all(&tmp_root);
    let tmp_out = tmp_root.join(format!("conv-{job_idx}.tmp.{}", item.target_ext));
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
        let (saved, dest) = place_converted_output(&item.path, &tmp_out, &item.target_ext, replace)?;
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
                    Err(e) => send(&tx, JobMsg::ConvertError { job: Some(i), msg: e }),
                }
            }));
        }
        for h in handles {
            if h.join().is_err() {
                send(
                    &tx,
                    JobMsg::ConvertError { job: None, msg: "worker panic".into() },
                );
            }
        }
        if cancel.load(Ordering::Relaxed) {
            send(
                &tx,
                JobMsg::ConvertError { job: None, msg: "Cancelled.".into() },
            );
        }
        send(
            &tx,
            JobMsg::ConvertDone { elapsed_s: t0.elapsed().as_secs_f64() },
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
                    // waiting for the next manual Shrink (preflight-filtered
                    // now; the worker re-checks at run time).
                    if s.executing {
                        let name = f
                            .path
                            .file_name()
                            .and_then(|x| x.to_str())
                            .unwrap_or("?")
                            .to_string();
                        let p = s.est_params();
                        let t = s.threshold();
                        if matches!(
                            shrinkr_core::pipeline::preflight(&f, &p, t),
                            shrinkr_core::pipeline::Preflight::Shrink { .. }
                        ) {
                            s.pending.push(f);
                            let waiting = s.pending.len();
                            s.push_log(format!(
                                "queued {name} ({waiting} waiting behind the running batch)"
                            ));
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
            let skipped = n_files - s.eligible().len();
            let total = s.total_bytes();
            let t = s.threshold();
            s.push_log(format!(
                "Probed {} files, {} total ({} would skip at {:.0}% threshold).",
                n_files,
                human_bytes(total),
                skipped,
                t
            ));
        }
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
        JobMsg::ExecDone { freed, elapsed_s } => {
            let cancelled = s.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
            if cancelled {
                // User-cancelled: pending files stay in the library, unrun.
                let n = s.pending.len();
                s.pending.clear();
                s.executing = false;
                s.push_log(format!(
                    "Cancelled after {:.0}s. Freed {}. {n} queued file(s) kept in the library, not run.",
                    elapsed_s,
                    human_bytes(freed.max(0) as u64)
                ));
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
                        )
                    })
                    .collect();
                s.exec_total = files.len();
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
            s.push_log(format!(
                "Convert ready: {} file(s) convertible.",
                ready
            ));
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
                s.push_log(format!(
                    "Convert done in {:.0}s: {done} converted, {failed} failed.",
                    elapsed_s
                ));
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
    }
}
