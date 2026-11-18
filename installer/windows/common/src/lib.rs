//! Shared by the native Shrinkr setup and uninstaller: install paths, the
//! uninstall registry entry, PATH handling and TaskDialog dialogs.

mod dialog;
mod path_env;
mod paths;
mod registry;
mod wide;
mod win32;

use anyhow::{Context, Result};

pub use dialog::{
    Button, CommonButtons, DIALOG_CANCEL, Dialog, DialogIcon, DialogResult, ask_yes_no, show,
    show_error,
};
pub use path_env::{path_add, path_contains, path_remove};
pub use paths::{desktop_shortcut, install_dir, start_menu_shortcut};
pub use registry::{
    Installed, find_installed, installed_desktop_shortcut, installed_dir, installed_path_added,
    installed_version, legacy_install_dir, remove_legacy_uninstall_entry, remove_uninstall_entry,
    write_uninstall_entry,
};
pub use win32::{
    broadcast_environment_change, is_windows_10_or_later, open_url, webview2_installed,
};

pub const APP_NAME: &str = "Shrinkr";
pub const APP_EXE: &str = "shrinkr.exe";
pub const CLI_EXE: &str = "shrinkr-cli.exe";
pub const UNINSTALLER_EXE: &str = "uninstall.exe";
pub const APP_ID: &str = "{4D8134AB-7963-468F-82AC-FCB570190ED3}";
/// Uninstall key the previous Inno Setup installer wrote; removed on upgrade.
pub const LEGACY_APP_ID: &str = "{4D8134AB-7963-468F-82AC-FCB570190ED3}_is1";
pub const PUBLISHER: &str = "Shrinkr";
pub const PROJECT_URL: &str = "https://github.com/feelvm/Shrinkr";
pub const WEBVIEW2_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

/// Stamped by `build.rs` from `SHRINKR_VERSION` (or the GUI manifest).
pub const VERSION: &str = env!("SHRINKR_VERSION");

pub fn launch_app(dir: &std::path::Path) -> Result<()> {
    std::process::Command::new(dir.join(APP_EXE))
        .current_dir(dir)
        .spawn()
        .with_context(|| format!("starting {}", dir.join(APP_EXE).display()))?;
    Ok(())
}
