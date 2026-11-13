//! Real-frame grain analysis checks; run explicitly with FFmpeg installed:
//! cargo test -p shrinkr-core --test grain_analysis -- --ignored

use shrinkr_core::{
    grain,
    media::probe_file,
    pipeline::{NvencPreset, ScalePolicy},
    process::cmd,
};
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc};

fn make_source(path: &Path, video_filter: Option<&str>) {
    let mut command = cmd("ffmpeg");
    command.args([
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "smptebars=size=640x360:rate=24:duration=6",
    ]);
    if let Some(filter) = video_filter {
        command.args(["-vf", filter]);
    }
    let output = command.args(["-c:v", "ffv1"]).arg(path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "requires FFmpeg; decodes real frames and runs SVT-AV1 samples"]
fn analysis_separates_clean_from_grainy_and_tests_strengths() {
    let dir = tempfile::tempdir().unwrap();
    let clean = dir.path().join("clean.mkv");
    let grainy = dir.path().join("grainy.mkv");
    make_source(&clean, None);
    make_source(&grainy, Some("noise=alls=24:allf=t+u"));
    let cancel = Arc::new(AtomicBool::new(false));

    let clean_media = probe_file(&clean).unwrap();
    let clean_profile = grain::analyze(&clean_media, dir.path(), &cancel)
        .expect("clean bars have plenty of flat area");
    assert!(
        clean_profile.flat_fraction > 0.1,
        "{}",
        clean_profile.flat_fraction
    );
    assert!(
        clean_profile.sigma < 1.5,
        "clean sigma {}",
        clean_profile.sigma
    );
    assert_eq!(clean_profile.strength, 0);

    let grainy_media = probe_file(&grainy).unwrap();
    let grainy_profile =
        grain::analyze(&grainy_media, dir.path(), &cancel).expect("grainy bars are measurable");
    eprintln!(
        "grainy sigma {:.2}, flat {:.2}, strength {}",
        grainy_profile.sigma, grainy_profile.flat_fraction, grainy_profile.strength
    );
    assert!(
        grainy_profile.sigma > 2.0,
        "grainy sigma {}",
        grainy_profile.sigma
    );
    assert!(grainy_profile.strength >= 6);

    let choice = grain::choose_strength(
        &grainy_media,
        32,
        NvencPreset::P5,
        ScalePolicy::Preserve,
        shrinkr_core::hw::caps(),
        dir.path(),
        &cancel,
    );
    eprintln!(
        "chosen strength {} (measured {})",
        choice.strength, choice.measured
    );
    assert!(choice.measured, "SVT analysis should run");
    assert!(
        choice.strength > 0,
        "a synthetic-grain source should keep synthesis, not smooth it"
    );
    assert!(choice.profile.map(|p| p.strength).unwrap_or(0) > 0);
}
