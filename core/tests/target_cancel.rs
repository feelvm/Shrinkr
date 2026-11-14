//! Cancellation plumbing for the target-size solver and the benchmark
//! matrix: cargo test -p shrinkr-core --test target_cancel -- --ignored

use shrinkr_core::{
    media::probe_file,
    pipeline::{NvencPreset, ScalePolicy, VideoBackend},
    process::cmd,
    target::{parse_target, solve_crf_for_file},
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn ffmpeg(args: &[&str]) {
    let output = cmd("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A 3-second test clip (long enough for one 45 s-window plan to clamp to
/// the whole duration; probes take seconds, not minutes).
fn fixture(dir: &Path) -> std::path::PathBuf {
    let source = dir.join("source.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x120:rate=12:duration=3",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=3",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-c:v",
        "ffv1",
        "-c:a",
        "flac",
        source.to_str().unwrap(),
    ]);
    source
}

#[test]
#[ignore = "requires FFmpeg; runs a real solve probe"]
fn a_cancelled_solver_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture(dir.path());
    let m = probe_file(&source).unwrap();
    let spec = parse_target("2x").unwrap();
    let cancel = Arc::new(AtomicBool::new(true));
    let r = solve_crf_for_file(
        &m,
        VideoBackend::CpuX264,
        NvencPreset::P5,
        28,
        ScalePolicy::Preserve,
        true,
        &[],
        None,
        false,
        false,
        &spec,
        &dir.path().join("work"),
        &cancel,
        &|_| {},
    );
    assert_eq!(r.unwrap_err(), "cancelled");
}

#[test]
#[ignore = "requires FFmpeg; encodes real samples"]
fn a_cancel_flag_raised_mid_run_stops_between_probes() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture(dir.path());
    let m = probe_file(&source).unwrap();
    let spec = parse_target("2x").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    // Raise the flag from another thread a moment after the solve starts:
    // whichever step boundary comes first (after the sample cuts or after
    // the first probe) must surface "cancelled" instead of completing.
    let cancel2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        cancel2.store(true, Ordering::Relaxed);
    });
    let r = solve_crf_for_file(
        &m,
        VideoBackend::CpuX264,
        NvencPreset::P5,
        28,
        ScalePolicy::Preserve,
        true,
        &[],
        None,
        false,
        false,
        &spec,
        &dir.path().join("work"),
        &cancel,
        &|_| {},
    );
    let err = r.unwrap_err();
    assert!(
        err == "cancelled" || err.contains("probe encode at"),
        "unexpected error: {err}"
    );
    // No solver artifacts may survive a cancelled solve: neither the
    // sample cuts nor the probe encodes it was killed inside.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("work"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.contains("-probe") || n.contains("-sample")
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "cancelled solve left artifacts behind: {leftovers:?}"
    );
}
