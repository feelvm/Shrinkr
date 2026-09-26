//! Native dialog helpers.
//!
//! Every rfd dialog is parented to the main window: the dialog stays
//! modal and centered on the app instead of fighting it for z-order.
//! The HWND is captured on the UI thread (as plain numbers) and moved
//! into the dialog worker thread — the only thread-safe direction.

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
};
use std::num::NonZeroIsize;

#[derive(Clone, Copy, Debug)]
pub struct DialogParent {
    hwnd: NonZeroIsize,
}

impl DialogParent {
    /// Capture the main window. Call on the UI thread during render;
    /// returns None if the handle is unavailable (dialog still works,
    /// just unparented).
    pub fn capture(ctx: &dioxus::desktop::DesktopContext) -> Option<Self> {
        let raw = ctx.window.window_handle().ok()?.as_raw();
        match raw {
            RawWindowHandle::Win32(h) => Some(Self { hwnd: h.hwnd }),
            _ => None,
        }
    }
}

impl HasWindowHandle for DialogParent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let h = Win32WindowHandle::new(self.hwnd);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(h)) })
    }
}

impl HasDisplayHandle for DialogParent {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe {
            DisplayHandle::borrow_raw(RawDisplayHandle::Windows(WindowsDisplayHandle::new()))
        })
    }
}
