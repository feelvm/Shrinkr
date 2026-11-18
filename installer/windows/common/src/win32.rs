use crate::wide::wide;
use std::mem::size_of;
use windows_sys::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SW_SHOWNORMAL, SendMessageTimeoutW, WM_SETTINGCHANGE,
};

/// Opens `url` with the user's default browser.
pub fn open_url(url: &str) {
    let verb = wide("open");
    let url = wide(url);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Tells Explorer and newly started processes that the user environment
/// changed, so a just-added PATH entry is picked up without a sign-out.
pub fn broadcast_environment_change() {
    let mut environment = wide("Environment");
    let mut result = 0usize;
    // SAFETY: the string stays alive for the duration of the broadcast.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            environment.as_mut_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
}

/// Shrinkr's GUI needs WebView2, which ships with Windows 10/11 but can be
/// absent on stripped or older images.
pub fn webview2_installed() -> bool {
    for name in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        let Some(base) = std::env::var_os(name) else {
            continue;
        };
        if std::path::Path::new(&base)
            .join("Microsoft")
            .join("EdgeWebView")
            .join("Application")
            .is_dir()
        {
            return true;
        }
    }
    false
}

/// `MinVersion=10.0` from the old installer. The embedded manifest declares
/// Windows 10 support, so GetVersionExW reports the real version.
pub fn is_windows_10_or_later() -> bool {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: the struct is zeroed, correctly sized, and owned by this frame.
    let ok = unsafe { GetVersionExW(&mut info) };
    ok == 0 || info.dwMajorVersion >= 10
}
