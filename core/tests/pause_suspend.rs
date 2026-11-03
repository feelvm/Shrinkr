//! Suspend/resume against a live ffmpeg encode. Ignored by default: it
//! needs ffmpeg on PATH and ~10s of wall clock. Run with
//! `cargo test -p shrinkr-core --test pause_suspend -- --ignored`.

#![cfg(target_os = "windows")]

use std::process::Stdio;
use std::time::Duration;

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Cumulative CPU time of a pid, in ticks, via PowerShell. A suspended
/// process accumulates none, which is how the test tells suspend from a
/// slow-but-running encode.
fn cpu_ticks(pid: u32) -> u128 {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).TotalProcessorTime.Ticks"),
        ])
        .output()
        .expect("powershell");
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse().expect("cpu ticks for pid")
}

#[test]
#[ignore]
fn suspend_freezes_a_running_encode_and_resume_unfreezes_it() {
    let child = KillOnDrop(
        shrinkr_core::process::cmd("ffmpeg")
            .args([
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1280x720:rate=30",
                "-t",
                "600",
                "-c:v",
                "libx264",
                "-preset",
                "slow",
                "-f",
                "null",
                "NUL",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg"),
    );
    let pid = child.0.id();
    // Let the encode actually get going before the first reading.
    std::thread::sleep(Duration::from_secs(2));

    let before = cpu_ticks(pid);
    shrinkr_core::pause::suspend_process(pid);
    std::thread::sleep(Duration::from_secs(3));
    let frozen = cpu_ticks(pid);
    shrinkr_core::pause::resume_process(pid);
    std::thread::sleep(Duration::from_secs(3));
    let after = cpu_ticks(pid);

    let frozen_ticks = frozen - before;
    let resumed_ticks = after - frozen;
    // Suspended: ~no CPU for 3s of wall clock. The reading latency leaves
    // a handful of ms of real encode work between the first query and the
    // suspend landing, so the frozen budget is set generously above that
    // (~50ms); a *running* x264-slow 720p encode burns orders of magnitude
    // more (several CPU-seconds over the same span).
    assert!(
        frozen_ticks < 500_000,
        "encode kept burning CPU while 'suspended': {frozen_ticks} ticks"
    );
    assert!(
        resumed_ticks > 1_000_000,
        "encode did not continue after resume: {resumed_ticks} ticks"
    );
}
