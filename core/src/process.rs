//! Child-process spawning without flashing console windows on Windows.
//!
//! The GUI binary ships with `windows_subsystem = "windows"` (no console of
//! its own). Without `CREATE_NO_WINDOW`, every `ffmpeg` / `ffprobe` /
//! `soffice` / `nvidia-smi` spawn would briefly pop up its own console
//! window. This helper sets that flag on Windows and is a plain
//! `Command::new` everywhere else. Output piping is unaffected, so the CLI
//! can share it too.

use std::io::{BufRead, BufReader};
use std::process::Command;
use std::process::{ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stderr_tail: String,
}

/// Drain pipes on readers while the calling thread independently polls the
/// process and cancellation flag. Progress callbacks run on the calling thread;
/// they need no Send/'static bounds. Every exit path reaps the child and joins
/// its readers, including cancellation before the first output line.
pub fn run_with_progress(
    command: &mut Command,
    cancel: &Arc<AtomicBool>,
    pausable: bool,
    mut on_line: impl FnMut(&str),
) -> Result<ProcessOutput, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn process: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(128);
    let output_reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let failed = line.is_err();
            if tx.send(line).is_err() || failed {
                break;
            }
        }
    });
    let error_reader = std::thread::spawn(move || {
        let mut tail = std::collections::VecDeque::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tail.len() == 40 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail.into_iter().collect::<Vec<_>>().join("\n")
    });
    let live = Arc::new(AtomicBool::new(true));
    let watcher =
        pausable.then(|| crate::pause::spawn_watcher(child.id(), cancel.clone(), live.clone()));
    let mut status = None;
    let result = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err("cancelled".to_string());
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(e) => break Err(format!("wait process: {e}")),
            }
        }
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(Ok(line)) => on_line(&line),
            Ok(Err(e)) => break Err(format!("read process progress: {e}")),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(status) = status {
                    break Ok(status);
                }
                // A process can close stdout and keep running. Continue
                // polling rather than blocking in wait() or spinning.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    if result.is_err() {
        let _ = child.kill();
    }
    // Even a killed or already-exited process must be reaped.
    let _ = child.wait();
    live.store(false, Ordering::Relaxed);
    if let Some(watcher) = watcher {
        let _ = watcher.join();
    }
    drop(rx); // unblock a reader sending queued progress during cancellation
    let _ = output_reader.join();
    let stderr_tail = error_reader.join().unwrap_or_default();
    Ok(ProcessOutput {
        status: result?,
        stderr_tail,
    })
}

/// Tools the release archives ship next to the binary. When a copy is
/// found there it wins over any PATH lookup, so what Shrinkr runs is the
/// exact build it was tested with. Everything else (nvidia-smi, soffice,
/// …) resolves through the OS as before.
const SIDECARS: &[&str] = &["ffmpeg", "ffprobe"];

/// Prefer the bundled copy next to the running executable; fall back to
/// the bare program name (PATH lookup). The GUI's "Download FFmpeg"
/// button also installs into the executable's directory, so it feeds the
/// same lookup.
fn resolve(prog: &str) -> String {
    if SIDECARS.contains(&prog) {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let sidecar = dir.join(format!("{prog}{}", std::env::consts::EXE_SUFFIX));
                if sidecar.is_file() {
                    return sidecar.to_string_lossy().into_owned();
                }
            }
        }
    }
    prog.to_string()
}

/// Like `Command::new`, but the child never gets its own console window
/// on Windows (`CREATE_NO_WINDOW`). `ffmpeg` / `ffprobe` are resolved
/// against the executable's directory first (see [`resolve`]).
pub fn cmd(prog: &str) -> Command {
    let mut c = Command::new(resolve(prog));
    no_window(&mut c);
    c
}

/// Apply the no-console-window flag to an already-built `Command`.
/// Returns the command for chaining.
pub fn no_window(c: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the child keeps piped stdio but no visible console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_sidecar_programs_pass_through() {
        assert_eq!(resolve("nvidia-smi"), "nvidia-smi");
        assert_eq!(resolve("soffice"), "soffice");
        // Names used by tests elsewhere in the workspace.
        assert_eq!(resolve("cmd"), "cmd");
        assert_eq!(resolve("false"), "false");
    }

    #[test]
    fn sidecars_fall_back_to_path_when_not_bundled() {
        // Test binaries run from target/*/deps — no sidecars next to
        // them, so resolution must keep the bare PATH name.
        assert_eq!(resolve("ffmpeg"), "ffmpeg");
        assert_eq!(resolve("ffprobe"), "ffprobe");
    }

    #[test]
    fn quiet_child_fixture() {
        match std::env::var("SHRINKR_PROCESS_FIXTURE").as_deref() {
            Ok("end") => println!("progress=end"),
            Ok("silent") => {}
            Ok("finish") => {
                println!("out_time_ms=12345");
                eprintln!("last diagnostic");
                std::process::exit(7);
            }
            _ => return,
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        no_window(&mut command);
        command
            .args([
                "--exact",
                "process::tests::quiet_child_fixture",
                "--nocapture",
            ])
            .env("SHRINKR_PROCESS_FIXTURE", mode);
        command
    }

    #[test]
    fn cancel_works_without_progress_and_after_progress_end() {
        for mode in ["silent", "end"] {
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            let trigger = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                flag.store(true, Ordering::Relaxed);
            });
            let started = std::time::Instant::now();
            let result = run_with_progress(&mut fixture(mode), &cancel, true, |_| {});
            trigger.join().unwrap();
            assert!(result.is_err_and(|e| e == "cancelled"));
            assert!(started.elapsed() < std::time::Duration::from_secs(3));
        }
    }

    #[test]
    fn progress_and_final_stderr_are_drained_before_returning() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut lines = vec![];
        let result = run_with_progress(&mut fixture("finish"), &cancel, false, |line| {
            lines.push(line.to_string())
        })
        .unwrap();
        assert_eq!(result.status.code(), Some(7));
        assert!(result.stderr_tail.contains("last diagnostic"));
        assert!(lines.iter().any(|line| line == "out_time_ms=12345"));
    }
}
