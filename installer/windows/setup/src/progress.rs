use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Shell::{
    CLSID_ProgressDialog, IProgressDialog, PROGDLG_AUTOTIME, PROGDLG_NORMAL,
};
use windows::core::{IUnknown, PCWSTR};

/// The shell's copy-style progress window. It runs independently of our
/// thread, so it is enough to keep it updated between decompression chunks.
pub struct Progress {
    dialog: Option<IProgressDialog>,
    done: u64,
    total: u64,
}

impl Progress {
    pub fn start(title: &str, total: u64, visible: bool) -> Self {
        if !visible {
            return Self {
                dialog: None,
                done: 0,
                total,
            };
        }

        // SAFETY: CoInitializeEx is called once for this setup thread; a
        // repeated call only returns S_FALSE, which is fine.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let dialog: Option<IProgressDialog> =
            unsafe { CoCreateInstance(&CLSID_ProgressDialog, None, CLSCTX_INPROC_SERVER).ok() };
        if let Some(dialog) = &dialog {
            let title = wide(title);
            let installing = wide("Preparing files...");
            // SAFETY: the string buffers live for each call; IProgressDialog
            // copies them into its own controls.
            unsafe {
                let _ = dialog.SetTitle(PCWSTR(title.as_ptr()));
                let _ = dialog.SetLine(1, PCWSTR(installing.as_ptr()), false, None);
                let _ = dialog.StartProgressDialog(
                    None,
                    None::<&IUnknown>,
                    PROGDLG_NORMAL | PROGDLG_AUTOTIME,
                    None,
                );
                let _ = dialog.SetProgress64(0, total.max(1));
            }
        }

        Self {
            dialog,
            done: 0,
            total: total.max(1),
        }
    }

    pub fn set_line(&self, text: &str) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let text = wide(text);
        // SAFETY: the string is NUL-terminated and outlives the call.
        unsafe {
            let _ = dialog.SetLine(2, PCWSTR(text.as_ptr()), false, None);
        }
    }

    pub fn advance(&mut self, bytes: u64) {
        self.done = (self.done + bytes).min(self.total);
        if let Some(dialog) = &self.dialog {
            // SAFETY: the COM object is alive while `self` is.
            unsafe {
                let _ = dialog.SetProgress64(self.done, self.total);
            }
        }
    }

    pub fn cancelled(&self) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| unsafe { dialog.HasUserCancelled().as_bool() })
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(dialog) = &self.dialog {
            // SAFETY: the COM object is still alive at this point.
            unsafe {
                let _ = dialog.StopProgressDialog();
            }
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
