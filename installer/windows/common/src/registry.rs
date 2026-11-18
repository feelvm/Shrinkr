use crate::{
    APP_EXE, APP_ID, APP_NAME, LEGACY_APP_ID, PROJECT_URL, PUBLISHER, UNINSTALLER_EXE, VERSION,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

#[derive(Debug, Clone)]
pub struct Installed {
    pub dir: PathBuf,
}

pub fn uninstall_key_path() -> String {
    format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{APP_ID}")
}

fn legacy_uninstall_key_path() -> String {
    format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{LEGACY_APP_ID}")
}

fn hive() -> RegKey {
    RegKey::predef(HKEY_CURRENT_USER)
}

/// Finds an existing install, including one made by the old Inno Setup build.
pub fn find_installed() -> Option<Installed> {
    for key in [uninstall_key_path(), legacy_uninstall_key_path()] {
        if let Some(dir) = read_install_dir(&hive(), &key) {
            return Some(Installed { dir });
        }
    }
    None
}

pub fn installed_dir() -> Option<PathBuf> {
    read_install_dir(&hive(), &uninstall_key_path())
}

/// Install directory recorded by the previous Inno Setup build.
pub fn legacy_install_dir() -> Option<PathBuf> {
    read_install_dir(&hive(), &legacy_uninstall_key_path())
}

/// Version recorded by this setup or the old Inno Setup uninstaller.
pub fn installed_version() -> Option<String> {
    for key in [uninstall_key_path(), legacy_uninstall_key_path()] {
        let Ok(key) = hive().open_subkey_with_flags(key, KEY_READ) else {
            continue;
        };
        if let Ok(version) = key.get_value("DisplayVersion") {
            return Some(version);
        }
    }
    None
}

fn read_install_dir(hive: &RegKey, key: &str) -> Option<PathBuf> {
    let key = hive.open_subkey_with_flags(key, KEY_READ).ok()?;

    let location: Option<String> = key.get_value("InstallLocation").ok();
    if let Some(location) = location.filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(location));
    }

    // Inno Setup's uninstall entry can omit InstallLocation; the app path it
    // records and the quoted uninstaller path both name the install directory.
    let app_path: Option<String> = key.get_value("Inno Setup: App Path").ok();
    if let Some(app_path) = app_path.filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(app_path));
    }

    let uninstall: String = key.get_value("UninstallString").ok()?;
    uninstall_dir(&uninstall)
}

fn uninstall_dir(uninstall: &str) -> Option<PathBuf> {
    let uninstall = uninstall.trim();
    let exe = if let Some(rest) = uninstall.strip_prefix('"') {
        &rest[..rest.find('"')?]
    } else {
        uninstall.split(' ').next()?
    };
    Path::new(exe).parent().map(Path::to_path_buf)
}

pub fn write_uninstall_entry(dir: &Path, size_kb: u32, desktop: bool, path: bool) -> Result<()> {
    let (key, _) = hive()
        .create_subkey(uninstall_key_path())
        .context("writing the uninstall registry entry")?;
    let exe = dir.join(APP_EXE);
    let uninstaller = dir.join(UNINSTALLER_EXE);

    key.set_value("DisplayName", &APP_NAME)?;
    key.set_value("DisplayVersion", &VERSION)?;
    key.set_value("DisplayIcon", &format!("\"{}\",0", exe.display()))?;
    key.set_value("InstallLocation", &dir.display().to_string())?;
    key.set_value("UninstallString", &format!("\"{}\"", uninstaller.display()))?;
    key.set_value(
        "QuietUninstallString",
        &format!("\"{}\" /S", uninstaller.display()),
    )?;
    key.set_value("Publisher", &PUBLISHER)?;
    key.set_value("URLInfoAbout", &PROJECT_URL)?;
    key.set_value("HelpLink", &PROJECT_URL)?;
    key.set_value("EstimatedSize", &size_kb)?;
    key.set_value("DesktopShortcut", &u32::from(desktop))?;
    key.set_value("PathAdded", &u32::from(path))?;
    key.set_value("NoModify", &1u32)?;
    key.set_value("NoRepair", &1u32)?;
    Ok(())
}

pub fn installed_desktop_shortcut() -> bool {
    hive()
        .open_subkey_with_flags(uninstall_key_path(), KEY_READ)
        .and_then(|key| key.get_value::<u32, _>("DesktopShortcut"))
        .is_ok_and(|value| value != 0)
}

pub fn installed_path_added() -> bool {
    hive()
        .open_subkey_with_flags(uninstall_key_path(), KEY_READ)
        .and_then(|key| key.get_value::<u32, _>("PathAdded"))
        .is_ok_and(|value| value != 0)
}

pub fn remove_uninstall_entry() {
    let hive = hive();
    let _ = hive.delete_subkey(uninstall_key_path());
    let _ = hive.delete_subkey(legacy_uninstall_key_path());
}

pub fn remove_legacy_uninstall_entry() {
    let _ = hive().delete_subkey(legacy_uninstall_key_path());
}
