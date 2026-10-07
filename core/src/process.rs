//! Child-process spawning without flashing console windows on Windows.
//!
//! The GUI binary ships with `windows_subsystem = "windows"` (no console of
//! its own). Without `CREATE_NO_WINDOW`, every `ffmpeg` / `ffprobe` /
//! `soffice` / `nvidia-smi` spawn would briefly pop up its own console
//! window. This helper sets that flag on Windows and is a plain
//! `Command::new` everywhere else. Output piping is unaffected, so the CLI
//! can share it too.

use std::process::Command;

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
}
