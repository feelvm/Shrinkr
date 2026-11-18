use crate::APP_NAME;
use anyhow::{Context, Result, bail};
use std::env;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs, SHGetKnownFolderPath};
use windows_sys::core::GUID;

/// Per-user install directory. It stays writable for the in-app updater,
/// which replaces the binaries and FFmpeg sidecars in place.
pub fn install_dir() -> Result<PathBuf> {
    Ok(env_path("LOCALAPPDATA")?.join("Programs").join(APP_NAME))
}

pub fn start_menu_shortcut() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_Programs)?.join(shortcut_name()))
}

pub fn desktop_shortcut() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_Desktop)?.join(shortcut_name()))
}

fn shortcut_name() -> String {
    format!("{APP_NAME}.lnk")
}

fn env_path(name: &str) -> Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .with_context(|| format!("{name} is not set"))
}

fn known_folder(id: &GUID) -> Result<PathBuf> {
    // SAFETY: on success SHGetKnownFolderPath hands back one null-terminated
    // UTF-16 path allocated with CoTaskMemAlloc, which is copied out and freed
    // exactly once below.
    unsafe {
        let mut raw: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw);
        if hr < 0 || raw.is_null() {
            bail!("resolving a shell folder failed (HRESULT {hr:#x})");
        }
        let mut len = 0;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, len)));
        CoTaskMemFree(raw.cast());
        Ok(path)
    }
}
