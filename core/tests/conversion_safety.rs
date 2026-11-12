//! Real-media checks: cargo test -p shrinkr-core --test conversion_safety -- --ignored

use shrinkr_core::{
    convert::{place_converted_output, run_convert, MediaKind},
    media::probe_file,
    process::cmd,
};
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

fn fixture(dir: &Path) -> std::path::PathBuf {
    let subtitles = dir.join("captions.srt");
    std::fs::write(
        &subtitles,
        "1\n00:00:00,000 --> 00:00:02,000\nA complete caption\n",
    )
    .unwrap();
    let source = dir.join("source.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x120:rate=12:duration=2",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=2",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=880:sample_rate=48000:duration=2",
        "-i",
        subtitles.to_str().unwrap(),
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map",
        "2:a",
        "-map",
        "3:s",
        "-c:v",
        "ffv1",
        "-c:a",
        "flac",
        "-c:s",
        "srt",
        source.to_str().unwrap(),
    ]);
    source
}

#[test]
#[ignore = "requires FFmpeg"]
fn conversion_keeps_both_audio_tracks_and_captions_before_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture(dir.path());
    let media = probe_file(&source).unwrap();
    let output = dir.path().join("temp.mp4");
    let cancel = Arc::new(AtomicBool::new(false));
    let result = run_convert(&media, MediaKind::Video, "mp4", &output, &|_| {}, &cancel).unwrap();
    assert!(!result.copied, "fixture must exercise re-encoding");
    let checked = probe_file(&output).unwrap();
    assert_eq!(checked.audio_stream_count, 2);
    assert_eq!(checked.sub_count, 1);
    assert!((checked.duration_s - media.duration_s).abs() <= 0.25);
    assert!(
        source.exists(),
        "verification itself never deletes the source"
    );
    let (_, destination) = place_converted_output(&source, &output, "mp4", true).unwrap();
    assert!(!source.exists());
    assert_eq!(probe_file(&destination).unwrap().audio_stream_count, 2);
}

#[test]
#[ignore = "requires FFmpeg"]
fn failed_verification_removes_temp_and_keeps_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture(dir.path());
    let original = std::fs::read(&source).unwrap();
    let media = probe_file(&source).unwrap();
    let output = dir.path().join("temp.mp4");
    let cancel = Arc::new(AtomicBool::new(false));
    // Simulate a source whose advertised timeline exceeds its real content.
    let mut incomplete = media.clone();
    incomplete.duration_s += 10.0;
    let error = run_convert(
        &incomplete,
        MediaKind::Video,
        "mp4",
        &output,
        &|_| {},
        &cancel,
    )
    .unwrap_err();
    assert!(error.contains("duration"), "{error}");
    assert!(!output.exists());
    assert_eq!(std::fs::read(&source).unwrap(), original);
    // An expected audio track must not disappear behind a successful encode.
    incomplete = media.clone();
    incomplete.audio_stream_count += 1;
    let error = run_convert(
        &incomplete,
        MediaKind::Video,
        "mp4",
        &output,
        &|_| {},
        &cancel,
    )
    .unwrap_err();
    assert!(error.contains("streams"), "{error}");
    assert!(!output.exists());
    assert_eq!(std::fs::read(&source).unwrap(), original);
    // Reject in-place calls before cleanup can touch the input.
    assert!(run_convert(&media, MediaKind::Video, "mp4", &source, &|_| {}, &cancel).is_err());
    assert_eq!(std::fs::read(&source).unwrap(), original);
}

#[test]
#[ignore = "requires FFmpeg"]
fn extraction_verifies_the_selected_audio_instead_of_the_video_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("longer-video.mp4");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x120:rate=12:duration=2",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=1",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-c:v",
        "libx264",
        "-c:a",
        "aac",
        source.to_str().unwrap(),
    ]);
    let media = probe_file(&source).unwrap();
    let output = dir.path().join("audio.m4a");
    run_convert(
        &media,
        MediaKind::Video,
        "m4a",
        &output,
        &|_| {},
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let audio = probe_file(&output).unwrap();
    assert_eq!(audio.video_stream_count, 0);
    assert_eq!(audio.audio_stream_count, 1);
    assert!((audio.duration_s - 1.0).abs() < 0.25);
}
