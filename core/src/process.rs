//! Child-process spawning without flashing console windows on Windows.
//!
//! The GUI binary ships with `windows_subsystem = "windows"` (no console of
//! its own). Without `CREATE_NO_WINDOW`, every `ffmpeg` / `ffprobe` /
//! `soffice` / `nvidia-smi` spawn would briefly pop up its own console
//! window. This helper sets that flag on Windows and is a plain
//! `Command::new` everywhere else. Output piping is unaffected, so the CLI
//! can share it too.

use std::process::Command;

/// Like `Command::new`, but the child never gets its own console window
/// on Windows (`CREATE_NO_WINDOW`).
pub fn cmd(prog: &str) -> Command {
    let mut c = Command::new(prog);
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
