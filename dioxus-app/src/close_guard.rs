//! Close-while-busy guard: closing the window during a running batch
//! silently killed the encodes (and orphaned the ffmpeg children on
//! Windows) with no warning. Two platform shapes, one rule:
//!
//! * Windows — the main window's `WM_CLOSE` is intercepted by a native
//!   message box before tao ever sees it. The user stays in the app on
//!   "No"; "Yes" forwards the message and the app exits normally. No
//!   window-flash, no event-loop tricks.
//! * macOS/Linux — tao has no preventable close, so the handler flips
//!   dioxus' close behaviour to "hide" while anything is busy (the OS
//!   close attempt hides the window instead of destroying it), asks in an
//!   rfd dialog on a worker thread, and the pump resolves the answer
//!   (`CloseGuardResolved`): quit = restore "closes" + close, stay =
//!   re-show the hidden window. Hide-behaviour flips happen on the UI
//!   thread only (the cell behind `set_close_behavior` is not `Sync`).
//!
//! The "busy" definition lives in one static, set by an App effect from
//! the state signal (shrink / convert / optimize runs and the update
//! install all replace or produce files).

use std::sync::atomic::{AtomicBool, Ordering};

static BUSY: AtomicBool = AtomicBool::new(false);

/// True while any run or update install is in flight. Read from the
/// window-proc / event-handler paths, written by the App effect.
pub fn is_busy() -> bool {
    BUSY.load(Ordering::Relaxed)
}

pub fn set_busy(busy: bool) {
    BUSY.store(busy, Ordering::Relaxed);
}

/// The confirmation question, shared by both platform shapes.
pub const CONFIRM_TITLE: &str = "Shrinkr";
pub const CONFIRM_TEXT: &str = "A batch is still running.\n\nQuitting now stops the running encodes; queued files stay unprocessed.\n\nQuit anyway?";

// ── Windows: WM_CLOSE subclass ───────────────────────────────────────

#[cfg(target_os = "windows")]
mod win {
    // Win32 window subclassing round-trips proc pointers through isize
    // (SetWindowLongPtrW's interface); the fn-pointer cast and the one
    // transmute below are that interface, not type confusion.
    #![allow(clippy::fn_to_numeric_cast, clippy::missing_transmute_annotations)]
    use super::{is_busy, CONFIRM_TEXT, CONFIRM_TITLE};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, MessageBoxW, SetWindowLongPtrW, GWLP_WNDPROC, IDYES,
        MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO, WM_CLOSE, WNDPROC,
    };

    static ORIG_PROC: OnceLock<WNDPROC> = OnceLock::new();
    static DIALOG_OPEN: AtomicBool = AtomicBool::new(false);

    /// Replace the window procedure with one that asks before closing.
    /// Called once, from the UI thread, after the main window exists.
    /// Everything else the original procedure did is preserved verbatim
    /// via `CallWindowProcW`.
    pub fn install(hwnd: isize) {
        if ORIG_PROC.get().is_some() {
            return;
        }
        let h: HWND = hwnd as HWND;
        unsafe {
            let prev = SetWindowLongPtrW(h, GWLP_WNDPROC, guard_proc as *const () as isize);
            if prev != 0 {
                let _ = ORIG_PROC.set(std::mem::transmute(prev));
            }
        }
    }

    unsafe extern "system" fn guard_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> isize {
        if msg == WM_CLOSE && is_busy() {
            // One dialog at a time; further close clicks while it is open
            // are swallowed instead of stacking.
            if !DIALOG_OPEN.swap(true, Ordering::Relaxed) {
                let title: Vec<u16> = CONFIRM_TITLE.encode_utf16().chain([0]).collect();
                let text: Vec<u16> = CONFIRM_TEXT.encode_utf16().chain([0]).collect();
                let choice = MessageBoxW(
                    hwnd,
                    text.as_ptr(),
                    title.as_ptr(),
                    MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND | MB_DEFBUTTON2,
                );
                DIALOG_OPEN.store(false, Ordering::Relaxed);
                if choice != IDYES {
                    return 0; // stay running: the close never happens
                }
                // "Yes": fall through to the original procedure, which
                // raises the close request the app was built to handle.
            } else {
                return 0;
            }
        }
        match ORIG_PROC.get() {
            Some(orig) => CallWindowProcW(*orig, hwnd, msg, wparam, lparam),
            // Subclass installed without a captured proc: fall back to
            // the default handling rather than eating messages.
            None => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(target_os = "windows")]
pub use win::install;

// ── macOS / Linux: dialog on a worker, resolution via the pump ──────

#[cfg(not(target_os = "windows"))]
mod unix {
    use super::{is_busy, CONFIRM_TEXT, CONFIRM_TITLE};
    use std::sync::atomic::{AtomicBool, Ordering};

    static DIALOG_PENDING: AtomicBool = AtomicBool::new(false);

    /// The OS raised a close request while busy. dioxus' close behaviour
    /// has already been flipped to "hide" by the App effect, so the
    /// window survives; ask the user on a worker thread and route the
    /// answer through the pump (which alone may touch close behaviour —
    /// it runs on the UI thread).
    pub fn spawn_confirm_dialog(tx: futures_channel::mpsc::UnboundedSender<crate::jobs::JobMsg>) {
        // One dialog at a time: further close clicks while it is open are
        // ignored instead of stacking dialogs.
        if DIALOG_PENDING.swap(true, Ordering::Relaxed) {
            return;
        }
        std::thread::spawn(move || {
            let quit = rfd::MessageDialog::new()
                .set_title(CONFIRM_TITLE)
                .set_description(CONFIRM_TEXT)
                .set_buttons(rfd::MessageButtons::YesNo)
                .set_level(rfd::MessageLevel::Warning)
                .show();
            DIALOG_PENDING.store(false, Ordering::Relaxed);
            let _ = tx.unbounded_send(crate::jobs::JobMsg::CloseGuardResolved { quit });
        });
    }

    /// Whether this close attempt should be intercepted (busy, and not
    /// already mid-dialog).
    pub fn should_intercept() -> bool {
        is_busy() && !DIALOG_PENDING.load(Ordering::Relaxed)
    }
}

#[cfg(not(target_os = "windows"))]
pub use unix::{should_intercept, spawn_confirm_dialog};

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn busy_flag_round_trips() {
        set_busy(true);
        assert!(is_busy());
        set_busy(false);
        assert!(!is_busy());
    }
}
