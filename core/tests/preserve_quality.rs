//! Real encoder checks; run explicitly with FFmpeg installed:
//! cargo test -p shrinkr-core --test preserve_quality -- --ignored

use shrinkr_core::{media::probe_file, pipeline::ScalePolicy, process::cmd, quality};
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc};

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

fn encode(source: &Path, output: &Path, filter: &str, audio: &[&str]) {
    let mut args = vec![
        "-i",
        source.to_str().unwrap(),
        "-map",
        "0:v:0",
        "-vf",
        filter,
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-crf",
        "0",
    ];
    args.extend_from_slice(audio);
    args.push(output.to_str().unwrap());
    ffmpeg(&args);
}

#[test]
#[ignore = "requires FFmpeg and performs full-video comparisons"]
fn accepts_clean_downscaling_and_rejects_damage_growth_and_audio_loss() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=12:duration=2",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=2",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-c:v",
        "ffv1",
        "-c:a",
        "pcm_s16le",
        source.to_str().unwrap(),
    ]);
    let media = probe_file(&source).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let output = dir.path().join("result.mkv");
    let audio = ["-map", "0:a", "-c:a", "copy"];

    encode(&source, &output, "scale=160:120:flags=bicubic", &audio);
    let score = quality::verify(&media, &output, &cancel).expect("clean downscale should pass");
    assert!(score > 0.999);

    encode(
        &source,
        &output,
        "scale=160:120:flags=bicubic,negate",
        &audio,
    );
    let error = quality::verify(&media, &output, &cancel).unwrap_err();
    assert!(error.contains("video differs"), "{error}");

    encode(&source, &output, "scale=160:120:flags=bicubic", &["-an"]);
    assert!(quality::verify(&media, &output, &cancel)
        .unwrap_err()
        .contains("track is missing"));

    encode(
        &source,
        &output,
        "scale=160:120:flags=bicubic",
        &["-map", "0:a", "-c:a", "libopus", "-b:a", "32k"],
    );
    assert!(quality::verify(&media, &output, &cancel)
        .unwrap_err()
        .contains("more than 10%"));

    std::fs::copy(&source, &output).unwrap();
    assert!(quality::verify(&media, &output, &cancel)
        .unwrap_err()
        .contains("not smaller"));
}

#[test]
#[ignore = "requires FFmpeg; cuts samples and encodes real candidates"]
fn auto_quality_finds_a_passing_cq_from_real_samples() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("search.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=24:duration=12",
        "-c:v",
        "ffv1",
        source.to_str().unwrap(),
    ]);
    let media = probe_file(&source).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let found = quality::auto_cq(
        &media,
        shrinkr_core::pipeline::VideoBackend::CpuX264,
        shrinkr_core::pipeline::NvencPreset::P5,
        ScalePolicy::Preserve,
        0,
        shrinkr_core::hw::caps(),
        dir.path(),
        &cancel,
    )
    .expect("a passing candidate exists");
    assert!(
        (quality::VIDEO_CQ + 1..=quality::MAX_SEARCH_CQ).contains(&found.cq),
        "{}",
        found.cq
    );
    assert!(found.sample_ssim >= 0.992, "{}", found.sample_ssim);
    assert!(found.probes > 0);
    // Search cleaned up its staging folder.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("auto-quality-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
