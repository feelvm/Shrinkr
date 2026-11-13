//! Background orchestration for the Dioxus frontend.
//!
//! Worker threads never touch UI state: they send [`JobMsg`] over a
//! futures channel to a UI-thread coroutine that applies them to the
//! `Signal<AppState>`. (Dioxus 0.7 signals use unsync storage, so this
//! separation is required — and it keeps every race in one place.)

use crate::state::{pop_next_segment, tool_ok, AppState};
use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;
use shrinkr_core::bench;
use shrinkr_core::ffmpeg::{place_output, EncodeBackend, FfmpegBackend};
use shrinkr_core::media::{human_bytes, probe_file, MediaFile};
use shrinkr_core::pipeline::{preflight_auto_with, EstParams, Preflight, ThresholdPolicy};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering, Arc, Mutex};
use std::thread;
use std::time::Instant;

#[derive(Debug)]
pub enum JobMsg {
    TargetsAdded(Vec<PathBuf>),
    ScanDone {
        gen: u64,
        paths: Vec<PathBuf>,
    },
    ProbeOne {
        gen: u64,
        done: usize,
        total: usize,
        file: Option<MediaFile>,
        err: Option<String>,
    },
    ProbeDone {
        gen: u64,
    },
    ExecProgress {
        job: usize,
        frac: f64,
        fps: f64,
    },
    /// A retry (or the first real encode after quality sampling) starts
    /// from zero; progress fractions otherwise only move forward.
    ExecRestart {
        job: usize,
    },
    ExecCheckingQuality {
        job: usize,
        name: String,
    },
    /// Preserve-quality mode measured short sample encodes before the real
    /// one; the row keeps its place and the log says what is happening.
    ExecSearchingQuality {
        job: usize,
        name: String,
    },
    /// Frame analysis + trial encodes are choosing an SVT grain strength.
    ExecAnalyzingGrain {
        job: usize,
        name: String,
    },
    /// A queued file is below the min-saving threshold but the user chose
    /// to run it anyway; surfaces the warning without failing the job.
    ExecWarn {
        msg: String,
    },
    /// The planner refused the file before encoding (explained skip, e.g.
    /// an audio-only pass on a file with no audio track). Not a failure.
    ExecSkipped {
        job: usize,
        detail: String,
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
    /// A file was still encoding when the user cancelled. Neither success
    /// nor failure — there is
    /// nothing wrong to report, so it must not surface as an error.
    ExecAborted {
        job: usize,
    },
    /// The user skipped or removed this file from the queue mid-run, and
    /// its turn came: the worker stepped over it without encoding.
    /// Bookkeeping treats it like an explained skip — done, not failed.
    ExecUserSkip {
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
    OptimizeAdded(Vec<PathBuf>),
    OptimizeScanStarted {
        n: usize,
    },
    OptimizeOne {
        item: crate::state::OptimizeItem,
        err: Option<String>,
    },
    OptimizeScanDone,
    OptimizeProgress {
        job: usize,
        frac: f64,
    },
    OptimizeFileDone {
        job: usize,
        detail: String,
        output_bytes: u64,
    },
    OptimizeNoGain {
        job: usize,
        detail: String,
    },
    OptimizeDone {
        elapsed_s: f64,
    },
    OptimizeError {
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
/// Keeps scan→probe ordering without UI-thread round-trips. `gen` is the
/// selection's scan token; Clear bumps it and the UI drops everything
/// this worker reports afterwards.
pub fn start_scan(tx: UnboundedSender<JobMsg>, targets: Vec<PathBuf>, gen: u64) {
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
        send(
            &tx,
            JobMsg::ScanDone {
                gen,
                paths: hits.clone(),
            },
        );
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
                            gen,
                            done: n,
                            total,
                            file: Some(mf),
                            err: None,
                        },
                    ),
                    Err(e) => send(
                        &tx,
                        JobMsg::ProbeOne {
                            gen,
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
        send(&tx, JobMsg::ProbeDone { gen });
    });
}

/// Result of one file: either an output was placed, or the encode was
/// thrown away because it didn't beat the source. Both are "expected"
/// outcomes — only `Err` is a failure.
enum JobOutcome {
    Placed {
        saved: i64,
        detail: String,
    },
    NoGain {
        detail: String,
    },
    /// The planner refused the file up front (e.g. an audio-only pass on a
    /// file with no audio track). Intentional and explained — the row must
    /// not read as a failed encode.
    Skipped {
        detail: String,
    },
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
    let params = params.for_media(&f);
    let keep_subs = keep_subs || params.preserve_quality;
    let extra_args = if params.preserve_quality {
        vec![]
    } else {
        extra_args
    };
    match preflight_auto_with(&f, &params, threshold, threshold_policy) {
        Preflight::Skip { reason } => {
            return Ok(JobOutcome::Skipped {
                detail: format!("skip {}: {}", f.path.display(), reason),
            });
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
    // Frame analysis decides whether this source actually has grain and how
    // hard SVT should denoise it. Density alone is a poor proxy: clean
    // high-bitrate sources were getting synthetic grain they never had.
    let grain_strength = if params.film_grain
        && params.backend == shrinkr_core::pipeline::VideoBackend::CpuAv1
        && !params.preserve_quality
    {
        send(
            tx,
            JobMsg::ExecAnalyzingGrain {
                job: job_idx,
                name: fname.clone(),
            },
        );
        let choice = shrinkr_core::grain::choose_strength(
            &f,
            params.cq,
            params.preset,
            params.scale,
            shrinkr_core::hw::caps(),
            tmp_root,
            cancel,
        );
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        choice.strength
    } else {
        0
    };
    // Preserve quality no longer fixes CRF 18 for everyone. Encode a few
    // short windows at increasing CQ and keep the strongest one that still
    // clears a slightly stricter sample gate; the complete video is then
    // verified exactly as before. Anything unsupported or too short falls
    // back to the conservative fixed value.
    let searched = params.preserve_quality
        && params.backend != shrinkr_core::pipeline::VideoBackend::CopyVideo;
    let auto = if searched {
        send(
            tx,
            JobMsg::ExecSearchingQuality {
                job: job_idx,
                name: fname.clone(),
            },
        );
        let found = shrinkr_core::quality::auto_cq(
            &f,
            params.backend,
            params.preset,
            params.scale,
            0,
            shrinkr_core::hw::caps(),
            tmp_root,
            cancel,
        );
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        found
    } else {
        None
    };
    let mut cq = auto.map_or(params.cq, |a| a.cq);
    send(tx, JobMsg::ExecRestart { job: job_idx });
    let mut retried = false;
    let mut gain_retries = 0u32;
    let (er, score) = loop {
        let r = be.encode(
            &f,
            &tmp_out,
            params.backend,
            params.preset,
            cq,
            params.scale,
            keep_subs,
            &extra_args,
            params.opus_bps,
            params.all_audio,
            grain_strength,
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
        // The preflight estimate is only a prediction. Never replace a
        // source with a file that isn't actually smaller — warn mode
        // deliberately queues below-threshold files, and the no-gain guard
        // keeps that safe. Unprotected audio-only passes are the exception:
        // video is copied untouched and the audio was re-encoded on
        // request, so a larger file is the intended result, not a failed
        // encode.
        if (params.preserve_quality
            || params.backend != shrinkr_core::pipeline::VideoBackend::CopyVideo)
            && er.output_bytes >= f.bytes
        {
            let _ = std::fs::remove_file(&tmp_out);
            // Max-shrink SVT runs ask for the smallest output, so a result
            // that loses to the source earns a stronger CRF instead of a
            // silent discard. Other presets keep their chosen quality.
            if let Some(next) = shrinkr_core::pipeline::gain_retry_cq(
                params.backend,
                cq,
                params.preserve_quality,
                gain_retries,
            ) {
                send(
                    tx,
                    JobMsg::ExecWarn {
                        msg: format!(
                            "warn {fname}: CRF {cq} encoded {} but the source is {}; retrying at CRF {next}",
                            human_bytes(er.output_bytes),
                            human_bytes(f.bytes)
                        ),
                    },
                );
                cq = next;
                gain_retries += 1;
                send(tx, JobMsg::ExecRestart { job: job_idx });
                continue;
            }
            return Ok(JobOutcome::NoGain {
                detail: format!(
                    "skip {fname}: no gain — encoded {} but the source is {}; original kept. \
                     Re-encoding already-compressed video usually grows it.",
                    human_bytes(er.output_bytes),
                    human_bytes(f.bytes)
                ),
            });
        }
        if !params.preserve_quality {
            break (er, None);
        }
        send(
            tx,
            JobMsg::ExecCheckingQuality {
                job: job_idx,
                name: fname.clone(),
            },
        );
        match shrinkr_core::quality::verify(&f, &tmp_out, cancel) {
            Ok(score) => break (er, Some(score)),
            Err(reason) => {
                let _ = std::fs::remove_file(&tmp_out);
                if cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                // The sample gate sits above the full-file gate, but a
                // whole film can still fail where three windows passed.
                // Retry once at the old conservative baseline instead of
                // discarding a file the user asked to protect.
                if !retried
                    && cq > shrinkr_core::quality::VIDEO_CQ
                    && reason.contains("video differs too much")
                    && params.backend != shrinkr_core::pipeline::VideoBackend::CopyVideo
                {
                    retried = true;
                    send(
                        tx,
                        JobMsg::ExecWarn {
                            msg: format!(
                                "warn {fname}: auto CRF {cq} missed the full-file gate ({reason}); \
                                 retrying at CRF {}",
                                shrinkr_core::quality::VIDEO_CQ
                            ),
                        },
                    );
                    cq = shrinkr_core::quality::VIDEO_CQ;
                    send(tx, JobMsg::ExecRestart { job: job_idx });
                    continue;
                }
                return Ok(JobOutcome::Skipped {
                    detail: format!("skip {fname}: quality protection — {reason}; original kept"),
                });
            }
        }
    };
    let quality_note = match score {
        Some(score) if auto.is_some_and(|a| a.cq == cq) => {
            format!(", quality checked (SSIM {score:.4}, auto CRF {cq})")
        }
        Some(score) => format!(", quality checked (SSIM {score:.4})"),
        None => String::new(),
    };
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&tmp_out);
        return Err("cancelled".into());
    }
    let scale_note = match er.scaled_to {
        Some((w, h)) => format!(" scale_cuda→{}x{}", w, h),
        None => String::new(),
    };
    let (saved, dest) = place_output(&f.path, &tmp_out, replace, "mkv")?;
    let dest_name = dest.file_name().and_then(|x| x.to_str()).unwrap_or("?");
    let mut detail = format!(
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
    detail.push_str(&quality_note);
    if gain_retries > 0 {
        detail.push_str(&format!(", raised to CRF {cq} after a no-gain attempt"));
    }
    if params.film_grain
        && params.backend == shrinkr_core::pipeline::VideoBackend::CpuAv1
        && !params.preserve_quality
    {
        if grain_strength > 0 {
            detail.push_str(&format!(", grain auto {grain_strength}"));
        } else {
            detail.push_str(", grain off");
        }
    }
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
    let mut plan = match plan_image(&f, params.image_preserve_format) {
        Ok(plan) => plan,
        Err(reason) => {
            return Ok(JobOutcome::Skipped {
                detail: format!("skip {fname}: {reason}"),
            });
        }
    };
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
    skips: Arc<Mutex<HashSet<PathBuf>>>,
) {
    // Caller guarantees non-empty files; the worker always ends with ExecDone.
    // A fresh batch never inherits a pause (chained legs start here too).
    shrinkr_core::pause::reset();
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
            let skips = skips.clone();
            handles.push(thread::spawn(move || loop {
                // Paused run: hold before claiming the next file until
                // resumed or cancelled — cancel wins so a pause left set
                // can't stall an aborted batch past its ExecDone.
                while shrinkr_core::pause::get() && !cancel.load(Ordering::Relaxed) {
                    thread::sleep(std::time::Duration::from_millis(150));
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= files.len() {
                    break;
                }
                if cancel.load(Ordering::Relaxed) {
                    continue;
                }
                // Skipped or removed from the queue mid-run: the file's
                // turn still comes (the claim is index-based), but no
                // encode happens. The UI already hid or flagged the row;
                // this closes the loop on the worker side.
                let user_skipped =
                    skips.lock().map(|g| g.contains(&files[i].path)).unwrap_or(false);
                if user_skipped {
                    let name = files[i]
                        .path
                        .file_name()
                        .and_then(|x| x.to_str())
                        .unwrap_or("?");
                    send(
                        &tx,
                        JobMsg::ExecUserSkip {
                            job: i,
                            detail: format!(
                                "skip {name}: skipped from the queue at the user's request — not encoded"
                            ),
                        },
                    );
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
                        freed.fetch_add(saved, Ordering::Relaxed);
                        send(&tx, JobMsg::ExecFileDone { job: i, detail });
                    }
                    Ok(JobOutcome::NoGain { detail }) => {
                        // No gain applies only to this file. Later files and
                        // queued batches must still get their own attempt.
                        send(&tx, JobMsg::ExecNoGain { job: i, detail });
                    }
                    Ok(JobOutcome::Skipped { detail }) => {
                        send(&tx, JobMsg::ExecSkipped { job: i, detail });
                    }
                    Err(e) if e.contains("cancelled") => {
                        // The user cancelled this file. Not an error, and the
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
        if cancel.load(Ordering::Relaxed) {
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
    film_grain: bool,
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
            film_grain,
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

/// Toggle the run pause: suspend in-flight ffmpeg children and stop
/// claiming new files (resume does the same flag flip in reverse).
pub fn pause_exec(mut state: Signal<AppState>) {
    let mut w = state.write();
    let paused = !w.paused;
    shrinkr_core::pause::set(paused);
    w.paused = paused;
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

/// Skip one waiting file mid-run: it will not encode when its turn comes
/// (the row reads "skipped"); it stays in the library. Works for the
/// running batch's unclaimed rows and for queued-behind-batch files via
/// the shared skip set — the worker checks it before each claimed file,
/// and the chain start filters it. Rows already encoding or finished
/// carry no skip control, so reaching here means the file was waiting.
pub fn skip_queued(mut state: Signal<AppState>, path: &Path) {
    let mut w = state.write();
    if !w.executing {
        return;
    }
    if let Some(set) = &w.exec_skips {
        if let Ok(mut g) = set.lock() {
            g.insert(path.to_path_buf());
        }
    }
    let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("?");
    w.push_log(format!(
        "skipped {name}: will not run — skipped from the queue (stays in the library)."
    ));
}

/// Delete one file from the queue mid-run (file stays in the library).
/// Queued-behind-batch files drop out of their segment immediately;
/// staged picks leave staging; a not-yet-claimed row of the running
/// batch is hidden at once and skipped by the worker when its turn
/// comes. In-flight or finished rows carry no delete control.
pub fn unqueue(mut state: Signal<AppState>, path: &Path) {
    let mut w = state.write();
    if !w.executing {
        return;
    }
    let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("?");
    // Waiting behind the batch: straight out of its segment.
    let mut was_pending = false;
    for seg in w.pending.iter_mut() {
        if let Some(pos) = seg.files.iter().position(|f| f.path == path) {
            seg.files.remove(pos);
            was_pending = true;
            break;
        }
    }
    if was_pending {
        // A segment emptied by this deletion disappears from the panel.
        w.pending.retain(|seg| !seg.files.is_empty());
        w.push_log(format!(
            "Removed {name} from the queue — it stays in the library."
        ));
        return;
    }
    // Staged mid-run picks: leaving staging (Add to queue never sees them).
    if let Some(pos) = w.staged.iter().position(|f| f.path == path) {
        w.staged.remove(pos);
        w.push_log(format!(
            "Removed {name} from staging — it stays in the library, not queued."
        ));
        return;
    }
    // Running batch, not yet claimed: hide the row and let the worker's
    // skip check close it out when the file's turn comes.
    if let Some(item) = w.exec_items.iter_mut().find(|i| i.path == path) {
        if !item.done && !item.failed && item.frac == 0.0 {
            item.removed = true;
            if let Some(set) = &w.exec_skips {
                if let Ok(mut g) = set.lock() {
                    g.insert(path.to_path_buf());
                }
            }
            w.push_log(format!(
                "Removed {name} from the queue — it stays in the library."
            ));
        }
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

/// Walk + classify dropped/picked paths for the Optimize tab. PDFs and
/// PNG/JPEGs are accepted directly; no probe is needed because neither
/// optimizer depends on media metadata.
pub fn start_optimize_scan(tx: UnboundedSender<JobMsg>, paths: Vec<PathBuf>) {
    use crate::state::{OptimizeItem, OptimizeKind};
    use shrinkr_core::{optimize, pdf};
    thread::spawn(move || {
        send(&tx, JobMsg::OptimizeScanStarted { n: paths.len() });
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut hits: Vec<PathBuf> = vec![];
        for p in &paths {
            match std::fs::metadata(p) {
                Ok(m) if m.is_dir() => {
                    for hit in pdf::collect_pdf_inputs(p)
                        .into_iter()
                        .chain(optimize::collect_optimize_inputs(p))
                    {
                        if seen.insert(hit.clone()) {
                            hits.push(hit);
                        }
                    }
                }
                Ok(_) => {
                    if pdf::is_pdf(p) || optimize::is_optimizable_image(p) {
                        if seen.insert(p.clone()) {
                            hits.push(p.clone());
                        }
                    } else {
                        send(
                            &tx,
                            JobMsg::OptimizeError {
                                job: None,
                                msg: format!("skip {}: not a PDF, PNG or JPEG", p.display()),
                            },
                        );
                    }
                }
                Err(_) => continue,
            }
        }
        hits.sort();
        hits.dedup();
        send(&tx, JobMsg::OptimizeAdded(hits.clone()));
        for p in hits {
            let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let kind = if pdf::is_pdf(&p) {
                OptimizeKind::Pdf
            } else {
                OptimizeKind::Image
            };
            send(
                &tx,
                JobMsg::OptimizeOne {
                    item: OptimizeItem::new(p, bytes, kind),
                    err: None,
                },
            );
        }
        send(&tx, JobMsg::OptimizeScanDone);
    });
}

enum OptimizeOutcome {
    Done { detail: String, output_bytes: u64 },
    NoGain { detail: String },
}

#[allow(clippy::too_many_arguments)]
fn run_one_optimize(
    tx: &UnboundedSender<JobMsg>,
    item: &crate::state::OptimizeItem,
    job_idx: usize,
    replace: bool,
    pdf_preset: shrinkr_core::pdf::PdfPreset,
    level: shrinkr_core::optimize::OptimizeLevel,
    tmp_root: &Path,
    cancel: &Arc<AtomicBool>,
) -> Result<OptimizeOutcome, String> {
    use crate::state::OptimizeKind;
    use shrinkr_core::{optimize, pdf};
    let ext = pdf::ext_of(&item.path);
    let name = item.name();
    let tmp = tmp_root.join(format!("opt-{job_idx}.{ext}"));
    send(
        tx,
        JobMsg::OptimizeProgress {
            job: job_idx,
            frac: 0.05,
        },
    );
    shrinkr_core::ffmpeg::ensure_output_dir(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    let (note, output_bytes) = match item.kind {
        OptimizeKind::Pdf => match pdf::compress_pdf(&item.path, pdf_preset, &tmp, cancel)? {
            pdf::PdfOutcome::Done(r) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err("cancelled".into());
                }
                (
                    format!(
                        "pdf {}: {} page(s), {}/{} image(s) re-encoded ({} downscaled), {} stream(s), {:.0}% smaller, {:.1}s",
                        r.preset.label().to_lowercase(),
                        r.pages,
                        r.images_recompressed,
                        r.images,
                        r.images_downscaled,
                        r.streams_recompressed,
                        (1.0 - r.ratio) * 100.0,
                        r.elapsed_s
                    ),
                    r.output_bytes,
                )
            }
            pdf::PdfOutcome::NoGain { output_bytes } => {
                let _ = std::fs::remove_file(&tmp);
                return Ok(OptimizeOutcome::NoGain {
                    detail: format!(
                        "skip {name}: no gain - {} vs {}; original kept",
                        human_bytes(output_bytes),
                        human_bytes(item.bytes)
                    ),
                });
            }
        },
        OptimizeKind::Image => match optimize::optimize_image(&item.path, &tmp, level, cancel)? {
            optimize::OptimizeOutcome::Done(r) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err("cancelled".into());
                }
                (
                    format!(
                        "{} {}: {:.0}% smaller, {:.1}s",
                        r.method,
                        r.note,
                        (1.0 - r.ratio) * 100.0,
                        r.elapsed_s
                    ),
                    r.output_bytes,
                )
            }
            optimize::OptimizeOutcome::NoGain { output_bytes } => {
                let _ = std::fs::remove_file(&tmp);
                return Ok(OptimizeOutcome::NoGain {
                    detail: format!(
                        "skip {name}: no gain - {} vs {}; original kept",
                        human_bytes(output_bytes),
                        human_bytes(item.bytes)
                    ),
                });
            }
        },
    };
    // Verified output is in the temp file; place it beside the source
    // (or over it when replacement was requested).
    let (_delta, dest) = place_output(&item.path, &tmp, replace, &ext)?;
    let _ = std::fs::remove_file(&tmp);
    let dest_name = dest.file_name().and_then(|x| x.to_str()).unwrap_or("?");
    let detail = format!(
        "done {name}: {} -> {} ({}), wrote {dest_name} [{note}]",
        human_bytes(item.bytes),
        human_bytes(output_bytes),
        if replace { "replaced" } else { "kept" }
    );
    Ok(OptimizeOutcome::Done {
        detail,
        output_bytes,
    })
}

/// Run the Optimize queue with two workers (PDF imaging and PNG DEFLATE
/// searches are CPU-bound; two covers the common case without starving
/// the UI thread's own work).
pub fn start_optimize_exec(
    tx: UnboundedSender<JobMsg>,
    files: Vec<crate::state::OptimizeItem>,
    indices: Vec<usize>,
    replace: bool,
    pdf_preset: shrinkr_core::pdf::PdfPreset,
    level: shrinkr_core::optimize::OptimizeLevel,
    cancel: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let t0 = Instant::now();
        let tmp_root = match shrinkr_core::ffmpeg::work_root() {
            Ok(d) => d.join("optimize"),
            Err(e) => {
                send(
                    &tx,
                    JobMsg::OptimizeError {
                        job: None,
                        msg: format!("work folder unusable: {e}"),
                    },
                );
                send(
                    &tx,
                    JobMsg::OptimizeDone {
                        elapsed_s: t0.elapsed().as_secs_f64(),
                    },
                );
                return;
            }
        };
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
            let tmp_root = tmp_root.clone();
            handles.push(thread::spawn(move || loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= queue.len() {
                    break;
                }
                let i = queue[k];
                if cancel.load(Ordering::Relaxed) {
                    continue;
                }
                match run_one_optimize(
                    &tx, &files[i], i, replace, pdf_preset, level, &tmp_root, &cancel,
                ) {
                    Ok(OptimizeOutcome::Done {
                        detail,
                        output_bytes,
                    }) => send(
                        &tx,
                        JobMsg::OptimizeFileDone {
                            job: i,
                            detail,
                            output_bytes,
                        },
                    ),
                    Ok(OptimizeOutcome::NoGain { detail }) => {
                        send(&tx, JobMsg::OptimizeNoGain { job: i, detail })
                    }
                    Err(e) => send(
                        &tx,
                        JobMsg::OptimizeError {
                            job: Some(i),
                            msg: if e == "cancelled" {
                                format!("{}: cancelled", files[i].name())
                            } else {
                                format!("{}: {e}", files[i].name())
                            },
                        },
                    ),
                }
            }));
        }
        for h in handles {
            if h.join().is_err() {
                send(
                    &tx,
                    JobMsg::OptimizeError {
                        job: None,
                        msg: "worker panic".into(),
                    },
                );
            }
        }
        if cancel.load(Ordering::Relaxed) {
            send(
                &tx,
                JobMsg::OptimizeError {
                    job: None,
                    msg: "Cancelled.".into(),
                },
            );
        }
        send(
            &tx,
            JobMsg::OptimizeDone {
                elapsed_s: t0.elapsed().as_secs_f64(),
            },
        );
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
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&tmp_out);
            return Err("cancelled".into());
        }
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
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&tmp_out);
        return Err("cancelled".into());
    }
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

/// Scan whatever was added to `targets` when the scanner is idle. Adding
/// files is the only scan gesture now — idle or mid-run — so targets that
/// arrive while another scan runs wait here and drain when it finishes.
/// Scans append to the library; Clear removes the selection.
fn start_pending_scan(s: &mut AppState) {
    if s.scanning || s.probing || s.targets.is_empty() {
        return;
    }
    let Some(tx) = s.job_tx.clone() else {
        return;
    };
    let targets = std::mem::take(&mut s.targets);
    s.scanning = true;
    s.push_log(format!("Scanning {} selected item(s) …", targets.len()));
    start_scan(tx, targets, s.scan_gen);
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
            // Adding is the scan gesture now: start immediately when the
            // scanner is idle. Additions append to the selection (Clear
            // removes it), so successive picks build one batch — and a
            // mid-run pick probes into `staged`, where the explicit
            // "Add to queue" click commits it with the current settings.
            start_pending_scan(&mut s);
        }
        JobMsg::ScanDone { gen, paths } => {
            // A Clear since this scan started: its results belong to a
            // selection the user already dismissed.
            if gen != s.scan_gen {
                return;
            }
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
            gen,
            done,
            total,
            file,
            err,
        } => {
            if gen != s.scan_gen {
                return;
            }
            s.probe_done = done;
            s.probe_total = total;
            if let Some(f) = file {
                // Same targets re-scanned would otherwise duplicate the
                // library and re-queue finished files.
                if !s.files.iter().any(|x| x.path == f.path) {
                    s.files.push(f.clone());
                    // Probed while a batch runs: stage it — the file is
                    // in the library, but joining the queue needs the
                    // explicit "Add to queue" click, which snapshots the
                    // user's picked settings for this batch. A codec
                    // muted in the Library never passes to the queue —
                    // not even via staging; the file stays greyed out.
                    if s.executing && !s.muted_codecs.contains(&f.vcodec) {
                        s.staged.push(f.clone());
                        let name = f
                            .path
                            .file_name()
                            .and_then(|x| x.to_str())
                            .unwrap_or("?")
                            .to_string();
                        let staged = s.staged.len();
                        s.push_log(format!(
                            "{name} staged ({staged} staged) — Add to queue runs them with the picked settings"
                        ));
                    }
                }
            }
            if let Some(e) = err {
                s.push_log(format!("probe warn: {}", e));
            }
        }
        JobMsg::ProbeDone { gen } => {
            if gen != s.scan_gen {
                return;
            }
            s.probing = false;
            if s.executing {
                let staged = s.staged.len();
                s.push_log(format!(
                    "Scan finished — {staged} new file(s) staged. Press Add to queue to run them with the picked settings."
                ));
            } else {
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
            // A second pick can wait in targets while this scan ran, idle
            // or mid-run: chain straight into it so auto-scan keeps up.
            start_pending_scan(&mut s);
        }
        JobMsg::ExecWarn { msg } => s.push_log(msg),
        JobMsg::ExecSearchingQuality { job, name } => {
            if let Some(item) = s.exec_items.get_mut(job) {
                item.checking_quality = true;
                item.fps = 0.0;
            }
            s.push_log(format!(
                "Finding the smallest encode that keeps {name}'s quality…"
            ));
        }
        JobMsg::ExecAnalyzingGrain { job, name } => {
            if let Some(item) = s.exec_items.get_mut(job) {
                item.checking_quality = true;
                item.fps = 0.0;
            }
            s.push_log(format!("Measuring grain for {name}…"));
        }
        JobMsg::ExecRestart { job } => {
            if let Some(item) = s.exec_items.get_mut(job) {
                item.frac = 0.0;
                item.fps = 0.0;
                item.checking_quality = false;
            }
        }
        JobMsg::ExecProgress { job, frac, fps } => {
            // Path cloned before the item borrows, so the skip-set fixup
            // below the borrows can get to it.
            let path = s.exec_items.get(job).map(|i| i.path.clone());
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
            // A skip/remove click that landed just after the worker
            // claimed the file loses the race: the encode is already
            // running. Restore the row and drop the skip entry so the
            // file finishes like any other.
            let started = s.exec_items.get(job).is_some_and(|i| i.frac > 0.0);
            if started {
                if let Some(item) = s.exec_items.get_mut(job) {
                    item.removed = false;
                }
                if let (Some(p), Some(set)) = (path.as_deref(), s.exec_skips.as_ref()) {
                    if let Ok(mut g) = set.lock() {
                        g.remove(p);
                    }
                }
            }
        }
        JobMsg::ExecCheckingQuality { job, name } => {
            if let Some(item) = s.exec_items.get_mut(job) {
                item.checking_quality = true;
                item.fps = 0.0;
            }
            s.push_log(format!("Checking picture and audio quality for {name}…"));
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
        JobMsg::ExecSkipped { job, detail } => {
            // Refused by the planner before any encode (reason in the log
            // line). A deliberate outcome — the row is done, not failed.
            if let Some(item) = s.exec_items.get_mut(job) {
                item.done = true;
                item.frac = 1.0;
            }
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
        JobMsg::ExecUserSkip { job, detail } => {
            // The user skipped or removed this file from the queue, and
            // the worker stepped over it when its turn came — done row,
            // one log line, never a failure.
            if let Some(item) = s.exec_items.get_mut(job) {
                item.done = true;
                item.frac = 1.0;
            }
            s.push_log(detail);
        }
        JobMsg::ExecDone { freed, elapsed_s } => {
            // Never leave a stale pause behind: the button disappears with
            // the run, and the next batch must start unpaused.
            shrinkr_core::pause::reset();
            s.paused = false;
            let cancelled = s.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
            if cancelled {
                // Pending files stay in the library, unrun.
                let n = s.pending_len();
                let staged = s.staged.len();
                s.pending.clear();
                s.staged.clear();
                s.executing = false;
                // The run's skip set dies with it; the next Process starts
                // with a fresh queue decision.
                s.exec_skips = None;
                let queued = if n == 0 {
                    String::new()
                } else {
                    format!(" {n} queued file(s) kept in the library, not run.")
                };
                let staged_note = if staged == 0 {
                    String::new()
                } else {
                    format!(" {staged} scanned file(s) stayed in the library — not queued.")
                };
                s.push_log(format!(
                    "Cancelled after {:.0}s. Freed {}.{queued}{staged_note}",
                    elapsed_s,
                    human_bytes(freed.max(0) as u64)
                ));
                return;
            }
            if !s.pending.is_empty() {
                // Chain: each mid-run queue addition carries the settings
                // snapshot taken when it was queued, so files picked with
                // different sidebar settings run in their own legs. The
                // next ExecDone drains whatever segments remain — minus
                // files the user skipped while they waited, which drop
                // here and stay in the library, unrun.
                let mut pending = std::mem::take(&mut s.pending);
                let (seg, skipped_waiting) = {
                    let guard = s.exec_skips.as_ref().and_then(|m| m.lock().ok());
                    match guard {
                        Some(g) => pop_next_segment(&mut pending, &g),
                        None => pop_next_segment(&mut pending, &HashSet::new()),
                    }
                };
                s.pending = pending;
                if skipped_waiting > 0 {
                    s.push_log(format!(
                        "Skipped {skipped_waiting} queued file(s) — they stay in the library, not run."
                    ));
                }
                if let Some(seg) = seg {
                    let Some(tx) = s.job_tx.clone() else {
                        s.executing = false;
                        s.push_log(format!(
                            "warn: {} queued file(s) lost — no job channel",
                            seg.files.len()
                        ));
                        return;
                    };
                    // Failure bookkeeping continues off this snapshot; the
                    // chained leg appends onto the run's failed list. The
                    // settings snapshot follows the leg now running, so the
                    // live queue ETA uses this leg's encoder and worker
                    // count (mid-run Queue also snapshots from here).
                    s.exec_cfg = Some(seg.cfg.clone());
                    s.exec_files = seg.files.clone();
                    s.exec_items = seg
                        .files
                        .iter()
                        .map(|f| {
                            crate::state::ExecItem::new(
                                f.path
                                    .file_name()
                                    .and_then(|x| x.to_str())
                                    .unwrap_or("?")
                                    .to_string(),
                                f.path.clone(),
                                f.res_label(),
                            )
                        })
                        .collect();
                    s.exec_total = seg.files.len();
                    s.exec_discarded = 0;
                    let cancel = Arc::new(AtomicBool::new(false));
                    s.cancel = Some(cancel.clone());
                    s.push_log(format!(
                        "Queued batch: starting {} file(s) with the queued settings{} …",
                        seg.files.len(),
                        if seg.warn {
                            " (warn mode: below-threshold files run after all)"
                        } else {
                            ""
                        }
                    ));
                    start_exec(
                        tx,
                        seg.files,
                        seg.cfg.params,
                        seg.cfg.keep_subs,
                        seg.cfg.parallel,
                        seg.cfg.replace,
                        seg.cfg.threshold,
                        if seg.warn {
                            ThresholdPolicy::Warn
                        } else {
                            seg.cfg.threshold_policy
                        },
                        seg.cfg.extra_args,
                        cancel,
                        s.exec_skips
                            .clone()
                            .unwrap_or_else(|| Arc::new(Mutex::new(HashSet::new()))),
                    );
                    return;
                }
                // Only skipped files waited: the run ends here instead of
                // starting a leg with nothing in it (the tail below closes
                // it out).
                s.exec_skips = None;
            }
            s.executing = false;
            s.exec_skips = None;
            if !s.staged.is_empty() {
                let staged_left = s.staged.len();
                s.staged.clear();
                s.push_log(format!(
                    "{staged_left} staged file(s) stayed in the library — not queued."
                ));
            }
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
                // Capture the full media record while state still holds
                // the snapshot: the Retry button re-runs exactly what
                // failed, with fresh metadata.
                if let Some(f) = s.exec_files.get(job).cloned() {
                    s.failed_files.push(f);
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
        JobMsg::OptimizeScanStarted { n } => {
            s.optimize_scanning = true;
            if n > 0 {
                s.push_log(format!("Collecting {n} item(s) to optimize."));
            }
        }
        JobMsg::OptimizeAdded(paths) => {
            if paths.is_empty() && !s.optimize_scanning {
                s.push_log("No PDFs or PNG/JPEG images found.".into());
            } else if !paths.is_empty() {
                s.push_log(format!("Found {} file(s) to optimize.", paths.len()));
            }
        }
        JobMsg::OptimizeOne { item, err } => {
            if let Some(e) = err {
                s.push_log(format!("warn: {}: {}", item.name(), e));
            } else if !s.optimize_files.iter().any(|x| x.path == item.path) {
                s.optimize_files.push(item);
            }
        }
        JobMsg::OptimizeScanDone => {
            s.optimize_scanning = false;
            let ready = s.optimize_files.iter().filter(|f| f.runnable()).count();
            s.push_log(format!("Optimize ready: {ready} file(s)."));
        }
        JobMsg::OptimizeProgress { job, frac } => {
            if let Some(item) = s.optimize_files.get_mut(job) {
                item.frac = frac.clamp(0.0, 1.0).max(item.frac);
            }
        }
        JobMsg::OptimizeFileDone {
            job,
            detail,
            output_bytes,
        } => {
            if let Some(item) = s.optimize_files.get_mut(job) {
                item.done = true;
                item.frac = 1.0;
                item.output_bytes = output_bytes;
                item.no_gain = false;
            }
            s.optimize_done += 1;
            s.push_log(detail);
        }
        JobMsg::OptimizeNoGain { job, detail } => {
            if let Some(item) = s.optimize_files.get_mut(job) {
                item.done = true;
                item.frac = 1.0;
                item.no_gain = true;
            }
            s.optimize_done += 1;
            s.push_log(detail);
        }
        JobMsg::OptimizeDone { elapsed_s } => {
            let cancelled = s
                .optimize_cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed));
            s.optimize_executing = false;
            if cancelled {
                s.push_log(format!("Optimize cancelled after {:.0}s.", elapsed_s));
            } else {
                let done = s.optimize_files.iter().filter(|f| f.done).count();
                let failed = s.optimize_files.iter().filter(|f| f.failed).count();
                let summary = if failed == 0 {
                    format!("done: {done} file(s) optimized in {elapsed_s:.0}s.")
                } else {
                    format!("done: {done} optimized, {failed} failed in {elapsed_s:.0}s.")
                };
                s.push_log(summary);
            }
        }
        JobMsg::OptimizeError { job, msg } => {
            if let Some(job) = job {
                if let Some(item) = s.optimize_files.get_mut(job) {
                    item.failed = true;
                    item.frac = 1.0;
                }
                s.optimize_done += 1;
            }
            s.push_log(format!("warn: {msg}"));
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

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[test]
    #[ignore = "requires FFmpeg; encodes a mixed batch"]
    fn no_gain_files_do_not_cancel_later_files() {
        let dir = tempfile::tempdir().unwrap();
        let unique = dir.path().file_name().unwrap().to_string_lossy();
        let source = dir.path().join(format!("batch-regression-{unique}.mkv"));
        let output = shrinkr_core::process::cmd("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x360:rate=24:duration=4",
                "-c:v",
                "ffv1",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let media = probe_file(&source).unwrap();
        // Force the real post-encode no-gain branch without depending on the
        // relative efficiency of a particular FFmpeg build. The final file
        // retains its real byte count and can actually shrink.
        let mut no_gain = media.clone();
        no_gain.bytes = 1;
        let mut files = vec![no_gain; 11];
        files.push(media);
        let mut params = crate::state::AppState::default().est_params();
        params.preserve_quality = false;
        params.backend = shrinkr_core::pipeline::VideoBackend::CpuX264;
        params.cq = 18;
        params.opus_bps = None;
        params.film_grain = false;
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = futures_channel::mpsc::unbounded();
        start_exec(
            tx,
            files,
            params,
            false,
            1,
            false,
            0.0,
            ThresholdPolicy::Warn,
            vec![],
            cancel.clone(),
            Arc::new(Mutex::new(HashSet::new())),
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let mut discarded = 0;
        let mut completed = vec![];
        let mut errors = vec![];
        loop {
            assert!(
                Instant::now() < deadline,
                "batch did not finish: {errors:?}"
            );
            match rx.try_recv() {
                Ok(JobMsg::ExecNoGain { .. }) => discarded += 1,
                Ok(JobMsg::ExecFileDone { job, .. }) => completed.push(job),
                Ok(JobMsg::ExecError { msg, .. }) => errors.push(msg),
                Ok(JobMsg::ExecDone { .. }) => break,
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(discarded, 11);
        assert_eq!(completed, [11]);
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(source.exists());
    }

    #[test]
    #[ignore = "requires FFmpeg; runs the preserve-quality search end to end"]
    fn preserve_quality_search_encodes_and_places_a_checked_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("auto-quality-source.mkv");
        let output = shrinkr_core::process::cmd("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=24:duration=12",
                "-c:v",
                "ffv1",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let media = probe_file(&source).unwrap();
        let mut params = crate::state::AppState::default().est_params();
        params.backend = shrinkr_core::pipeline::VideoBackend::CpuX264;
        params.preset = shrinkr_core::pipeline::NvencPreset::P5;
        params.opus_bps = None;
        params.film_grain = false;
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = futures_channel::mpsc::unbounded();
        let outcome = run_one_job(
            &tx,
            media,
            params,
            false,
            0.0,
            ThresholdPolicy::Warn,
            dir.path(),
            0,
            false,
            vec![],
            &cancel,
        )
        .unwrap();
        let mut searched = false;
        let mut restarted = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                JobMsg::ExecSearchingQuality { .. } => searched = true,
                JobMsg::ExecRestart { .. } => restarted = true,
                _ => {}
            }
        }
        assert!(searched && restarted);
        match outcome {
            JobOutcome::Placed { detail, .. } => {
                assert!(detail.contains("auto CRF"), "{detail}");
                assert!(detail.contains("quality checked"), "{detail}");
            }
            JobOutcome::NoGain { detail } | JobOutcome::Skipped { detail } => {
                panic!("expected a placed file: {detail}")
            }
        }
        let placed: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "mkv"))
            .filter(|e| e.path() != source)
            .collect();
        assert_eq!(placed.len(), 1, "{placed:?}");
    }

    #[test]
    #[ignore = "requires FFmpeg; analyzes real frames and encodes SVT-AV1"]
    fn adaptive_grain_picks_a_strength_and_encodes_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("grain-source.mkv");
        let output = shrinkr_core::process::cmd("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "smptebars=size=640x360:rate=24:duration=6",
                "-vf",
                "noise=alls=24:allf=t+u",
                "-c:v",
                "ffv1",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let media = probe_file(&source).unwrap();
        let mut params = crate::state::AppState::default().est_params();
        params.preserve_quality = false;
        params.backend = shrinkr_core::pipeline::VideoBackend::CpuAv1;
        params.preset = shrinkr_core::pipeline::NvencPreset::P5;
        params.cq = 32;
        params.opus_bps = None;
        params.film_grain = true;
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, _rx) = futures_channel::mpsc::unbounded();
        let outcome = run_one_job(
            &tx,
            media,
            params,
            false,
            0.0,
            ThresholdPolicy::Warn,
            dir.path(),
            0,
            false,
            vec![],
            &cancel,
        )
        .unwrap();
        match outcome {
            JobOutcome::Placed { detail, .. } => {
                assert!(detail.contains("grain auto"), "{detail}");
            }
            JobOutcome::NoGain { detail } | JobOutcome::Skipped { detail } => {
                panic!("expected a placed grainy file: {detail}")
            }
        }
    }
}
