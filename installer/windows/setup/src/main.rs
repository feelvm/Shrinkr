#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod install;
mod progress;
mod shortcut;

use anyhow::{Result, bail};
use common::{APP_NAME, Button, DIALOG_CANCEL, Dialog, DialogIcon, VERSION, desktop_shortcut};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

const BTN_INSTALL: i32 = 1001;
const BTN_RETRY: i32 = 1002;
const BTN_LAUNCH: i32 = 1003;
const BTN_CLOSE: i32 = 1004;
const BTN_CANCEL: i32 = DIALOG_CANCEL;
const RADIO_PATH: i32 = 1;
const RADIO_NO_PATH: i32 = 2;

fn main() {
    let args = Args::parse();

    let mutex = acquire_setup_mutex();
    if mutex.already_running {
        fail(&args, "Shrinkr Setup is already running.");
        std::process::exit(1);
    }
    let _mutex_handle = mutex.handle;

    let code = match run(&args) {
        Ok(code) => code,
        Err(err) => {
            fail(&args, &format!("{err:#}"));
            1
        }
    };
    std::process::exit(code);
}

fn fail(args: &Args, message: &str) {
    if args.silent {
        // No console and no dialog in silent mode; leave a trail for
        // unattended installs that fail.
        let _ = std::fs::write(std::env::temp_dir().join("shrinkr-setup.log"), message);
    } else {
        common::show_error(message);
    }
}

struct SetupMutex {
    handle: Option<HANDLE>,
    already_running: bool,
}

fn acquire_setup_mutex() -> SetupMutex {
    let name: Vec<u16> = "ShrinkrSetup".encode_utf16().chain(Some(0)).collect();
    // SAFETY: the name buffer is valid for the call; the handle stays owned by
    // this process for the whole install.
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 1, name.as_ptr()) };
    if handle.is_null() {
        return SetupMutex {
            handle: None,
            already_running: false,
        };
    }
    // SAFETY: GetLastError only reads the calling thread's last error.
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    SetupMutex {
        handle: Some(handle),
        already_running,
    }
}

#[derive(Default)]
struct Args {
    silent: bool,
    desktop: Option<bool>,
    path: Option<bool>,
    dir: Option<PathBuf>,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self::default();
        for raw in std::env::args().skip(1) {
            let flag = raw.trim_start_matches(['/', '-']);
            let (name, value) = flag.split_once('=').map_or((flag, None), |(name, value)| {
                (name, Some(value.trim_matches('"')))
            });
            match name.to_ascii_lowercase().as_str() {
                "s" | "silent" | "verysilent" => args.silent = true,
                "desktopicon" => args.desktop = Some(true),
                "nodesktopicon" => args.desktop = Some(false),
                "path" | "addtopath" => args.path = Some(true),
                "nopath" => args.path = Some(false),
                "d" | "dir" => {
                    if let Some(value) = value.filter(|value| !value.is_empty()) {
                        args.dir = Some(PathBuf::from(value));
                    }
                }
                _ => {}
            }
        }
        args
    }
}

fn run(args: &Args) -> Result<i32> {
    if !common::is_windows_10_or_later() {
        bail!("Shrinkr requires Windows 10 or later.");
    }

    if !args.silent && !common::webview2_installed() {
        let open = common::ask_yes_no(
            "Shrinkr Setup",
            "Shrinkr needs the Microsoft Edge WebView2 runtime, which is not installed.\n\n\
             Open the Microsoft WebView2 download page?",
        );
        if open {
            common::open_url(common::WEBVIEW2_URL);
        }
    }

    let existing = common::find_installed();
    let dir = args
        .dir
        .clone()
        .or_else(|| existing.as_ref().map(|installed| installed.dir.clone()))
        .unwrap_or(common::install_dir()?);
    let desktop_lnk = desktop_shortcut().is_ok_and(|path| path.exists());
    // A custom /D target is a separate install: never treat the existing
    // desktop shortcut as ours, or an unchecked box would delete it.
    let had_desktop = args.dir.is_none()
        && (common::installed_desktop_shortcut() || (existing.is_some() && desktop_lnk));
    let desktop_default = if args.dir.is_some() {
        args.desktop.unwrap_or(false)
    } else {
        desktop_lnk
    };

    if args.silent {
        let desktop = args.desktop.unwrap_or(had_desktop);
        let add_path = args.path.unwrap_or(true);
        if !install_with_retry(&dir, desktop, add_path, had_desktop, false)? {
            return Ok(2);
        }
        return Ok(0);
    }

    let installed_version = common::installed_version().unwrap_or_default();
    let answer = common::show(&options_dialog(
        &dir,
        &installed_version,
        desktop_default,
        args.path.unwrap_or(true),
    ))?;
    if answer.button != BTN_INSTALL {
        return Ok(2);
    }

    let desktop = answer.checked;
    let add_path = answer.radio != RADIO_NO_PATH;
    if !install_with_retry(&dir, desktop, add_path, had_desktop, true)? {
        return Ok(2);
    }

    if common::show(&done_dialog(&dir))?.button == BTN_LAUNCH {
        common::launch_app(&dir)?;
    }
    Ok(0)
}

/// Returns false when the user canceled either the progress window or the
/// running-app retry.
fn install_with_retry(
    dir: &Path,
    desktop: bool,
    add_path: bool,
    had_desktop: bool,
    show_progress: bool,
) -> Result<bool> {
    loop {
        match install::install(dir, desktop, add_path, had_desktop, show_progress) {
            Ok(()) => return Ok(true),
            Err(err) if err.downcast_ref::<install::AppRunning>().is_some() => {
                if !show_progress {
                    return Err(err);
                }
                if common::show(&running_dialog())?.button != BTN_RETRY {
                    return Ok(false);
                }
            }
            Err(err) if err.downcast_ref::<install::Canceled>().is_some() => return Ok(false),
            Err(err) => return Err(err),
        }
    }
}

fn options_dialog(
    dir: &Path,
    installed_version: &str,
    desktop_default: bool,
    path_default: bool,
) -> Dialog {
    let action = if installed_version.is_empty() {
        "Install"
    } else if installed_version == VERSION {
        "Repair"
    } else {
        "Update"
    };
    let mut dialog = Dialog::new(APP_NAME, format!("{action} {APP_NAME} {VERSION}?"));
    dialog.content = dir.display().to_string();
    dialog.default_button = BTN_INSTALL;
    dialog.default_radio = if path_default {
        RADIO_PATH
    } else {
        RADIO_NO_PATH
    };
    dialog.verification = Some(("Create a desktop shortcut".to_owned(), desktop_default));
    dialog.radios = vec![
        Button {
            id: RADIO_PATH,
            text: "Add shrinkr-cli to PATH".to_owned(),
        },
        Button {
            id: RADIO_NO_PATH,
            text: "Don't change PATH".to_owned(),
        },
    ];
    dialog.buttons = vec![
        Button {
            id: BTN_INSTALL,
            text: action.to_owned(),
        },
        Button {
            id: BTN_CANCEL,
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
            id: BTN_CANCEL,
            text: "Cancel".to_owned(),
        },
    ];
    dialog
}

fn done_dialog(dir: &Path) -> Dialog {
    let mut dialog = Dialog::new(APP_NAME, format!("{APP_NAME} {VERSION} is installed"));
    dialog.content = dir.display().to_string();
    dialog.default_button = BTN_LAUNCH;
    dialog.buttons = vec![
        Button {
            id: BTN_LAUNCH,
            text: "Launch".to_owned(),
        },
        Button {
            id: BTN_CLOSE,
            text: "Close".to_owned(),
        },
    ];
    dialog
}
