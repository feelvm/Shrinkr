#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use anyhow::{Context, Result, bail};
use common::{APP_EXE, APP_NAME, Button, CLI_EXE, Dialog, DialogIcon, UNINSTALLER_EXE};
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const BTN_UNINSTALL: i32 = 2001;
const BTN_RETRY: i32 = 2002;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // The interactive uninstaller copies itself to %TEMP% and starts this mode
    // so the install directory (including uninstall.exe) can be deleted.
    if args.iter().any(|arg| arg.eq_ignore_ascii_case("--cleanup")) {
        if let Some(dir) = cleanup_target(&args) {
            cleanup(&dir);
        }
        return;
    }

    let silent = args
        .iter()
        .any(|arg| is_flag(arg, &["s", "silent", "verysilent"]));
    let code = match run(silent) {
        Ok(code) => code,
        Err(err) => {
            let message = format!("{err:#}");
            if silent {
                let _ =
                    std::fs::write(std::env::temp_dir().join("shrinkr-uninstall.log"), &message);
            } else {
                common::show_error(&message);
            }
            1
        }
    };
    std::process::exit(code);
}

fn is_flag(arg: &str, names: &[&str]) -> bool {
    let flag = arg.trim_start_matches(['/', '-']);
    names.iter().any(|name| flag.eq_ignore_ascii_case(name))
}

/// Resolves the directory passed to the temporary cleanup copy. The flag is
/// only honored by a process running from %TEMP% under the name this program
/// copies itself to, and only for a folder that still looks like an install,
/// so an installed `uninstall.exe --cleanup <dir>` cannot wipe an arbitrary
/// directory.
fn cleanup_target(args: &[String]) -> Option<PathBuf> {
    let index = args
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case("--cleanup"))?;
    let dir = PathBuf::from(args.get(index + 1)?);

    let exe = std::env::current_exe().ok()?;
    let name = exe.file_name()?.to_string_lossy();
    if !name.starts_with("shrinkr-uninstall-") || exe.parent()? != std::env::temp_dir() {
        return None;
    }
    if !dir.join(APP_EXE).exists() {
        return None;
    }
    Some(dir)
}

fn run(silent: bool) -> Result<i32> {
    let dir = std::env::current_exe()?
        .parent()
        .context("locating the install directory")?
        .to_path_buf();

    let registered = common::installed_dir().as_deref() == Some(dir.as_path())
        || common::legacy_install_dir().as_deref() == Some(dir.as_path());
    if !registered && !(dir.join(APP_EXE).exists() && dir.join(UNINSTALLER_EXE).exists()) {
        bail!("this folder is not a Shrinkr installation");
    }

    if !silent && common::show(&confirm_dialog(&dir))?.button != BTN_UNINSTALL {
        return Ok(2);
    }

    if is_busy(&dir) {
        if silent {
            bail!("Shrinkr or an encode is running; close it before uninstalling");
        }
        while is_busy(&dir) {
            if common::show(&running_dialog())?.button != BTN_RETRY {
                return Ok(2);
            }
        }
    }

    remove_shortcuts();
    let _ = common::path_remove(&dir);
    common::remove_uninstall_entry();
    spawn_cleanup(&dir)?;
    Ok(0)
}

fn is_busy(dir: &Path) -> bool {
    [APP_EXE, CLI_EXE, "ffmpeg.exe", "ffprobe.exe"]
        .iter()
        .any(|name| is_running(&dir.join(name)))
}

fn is_running(exe: &Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(exe) {
        Ok(_) => false,
        // Windows reports a file held open without write sharing as a sharing
        // or lock violation; `ErrorKind` maps both to `Uncategorized`.
        Err(err) => matches!(err.raw_os_error(), Some(32) | Some(33)),
    }
}

fn remove_shortcuts() {
    if let Ok(path) = common::start_menu_shortcut() {
        let _ = fs::remove_file(path);
    }
    if let Ok(path) = common::desktop_shortcut() {
        let _ = fs::remove_file(path);
    }
}

fn spawn_cleanup(dir: &Path) -> Result<()> {
    let temp = std::env::temp_dir().join(format!("shrinkr-uninstall-{}.exe", std::process::id()));
    fs::copy(std::env::current_exe()?, &temp)
        .with_context(|| format!("copying the uninstaller to {}", temp.display()))?;
    Command::new(&temp)
        .arg("--cleanup")
        .arg(dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("starting {}", temp.display()))?;
    Ok(())
}

fn cleanup(dir: &Path) {
    // Let the original uninstaller process exit before removing its folder.
    std::thread::sleep(Duration::from_millis(400));
    for _ in 0..40 {
        if !dir.exists() || fs::remove_dir_all(dir).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    remove_self();
}

fn remove_self() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // raw_arg keeps the inner quotes intact; args() would wrap the whole
    // command string and cmd only deletes the first quoted path.
    let _ = Command::new("cmd")
        .raw_arg(format!(
            "/C ping -n 3 127.0.0.1 >NUL & del /F /Q \"{}\"",
            exe.display()
        ))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

fn confirm_dialog(dir: &Path) -> Dialog {
    let mut dialog = Dialog::new(APP_NAME, format!("Uninstall {APP_NAME}?"));
    dialog.content = dir.display().to_string();
    dialog.icon = DialogIcon::Warning;
    dialog.default_button = common::DIALOG_CANCEL;
    dialog.buttons = vec![
        Button {
            id: BTN_UNINSTALL,
            text: "Uninstall".to_owned(),
        },
        Button {
            id: common::DIALOG_CANCEL,
            text: "Cancel".to_owned(),
        },
    ];
    dialog
}

fn running_dialog() -> Dialog {
    let mut dialog = Dialog::new(APP_NAME, format!("{APP_NAME} is running"));
    dialog.content = "Close Shrinkr (and any running encode), then choose Retry.".to_owned();
    dialog.icon = DialogIcon::Warning;
    dialog.default_button = BTN_RETRY;
    dialog.buttons = vec![
        Button {
            id: BTN_RETRY,
            text: "Retry".to_owned(),
        },
        Button {
            id: common::DIALOG_CANCEL,
            text: "Cancel".to_owned(),
        },
    ];
    dialog
}
