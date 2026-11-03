//! Pause: a run-wide switch plus process suspend/resume for in-flight encodes.
//!
//! Two halves:
//!
//! * A global [`FLAG`] — the worker threads poll it before claiming the next
//!   file, so a paused run never starts new work. It is process-global (not
//!   passed through encoder signatures) because pausing is a run-level
//!   condition, like the cancel flag, but one that spans every in-flight
//!   child rather than one encode.
//! * [`spawn_watcher`] — one best-effort thread per ffmpeg child that
//!   suspends the process while the flag is set (`NtSuspendProcess` on
//!   Windows, `SIGSTOP` on Unix) and resumes it when the flag clears.
//!   Suspending freezes the encode exactly where it is: state lives in the
//!   OS scheduler, nothing is killed, and on resume ffmpeg continues from
//!   the same output position. Cancel still wins: `kill` works on a
//!   suspended process (TerminateProcess / SIGKILL), and the watcher
//!   resumes before giving up anyway so a cancel-while-paused drains.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How often the worker threads / watchers re-check the flag.
const POLL_MS: u64 = 150;

static FLAG: AtomicBool = AtomicBool::new(false);

/// Pause everything running under this batch. New files are not claimed and
/// in-flight ffmpeg children are suspended mid-encode.
pub fn set(paused: bool) {
    FLAG.store(paused, Ordering::Relaxed);
}

/// Is the run paused right now?
pub fn get() -> bool {
    FLAG.load(Ordering::Relaxed)
}

/// Clear the flag (batch start, batch done — never leave a stale pause
/// behind for a chained or retried batch).
pub fn reset() {
    FLAG.store(false, Ordering::Relaxed);
}

/// Watch one ffmpeg child for the duration of its encode: suspend it while
/// [`get`] is true, resume it when false. Exits when `live` is dropped to
/// false (encode over) or `cancel` trips; if it exits while suspended the
/// process is resumed first so the cancel kill / normal exit path drains.
pub fn spawn_watcher(pid: u32, cancel: Arc<AtomicBool>, live: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut suspended = false;
        loop {
            if cancel.load(Ordering::Relaxed) || !live.load(Ordering::Relaxed) {
                break;
            }
            let want = FLAG.load(Ordering::Relaxed);
            if want != suspended {
                if want {
                    suspend_process(pid);
                } else {
                    resume_process(pid);
                }
                suspended = want;
            }
            thread::sleep(Duration::from_millis(POLL_MS));
        }
        if suspended {
            resume_process(pid);
        }
    })
}

/// Suspend a process (best-effort; a no-op when the pid is already gone or
/// the OS call is unavailable — pausing then only stops new files).
pub fn suspend_process(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let Some(suspend) = windows::suspend() else {
            return;
        };
        let Some(h) = windows::open_process(pid) else {
            return;
        };
        unsafe { suspend(h) };
        windows::close(h);
    }
    #[cfg(not(target_os = "windows"))]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        // libc is linked by std on every unix target.
        let _ = unsafe { kill(pid as i32, SIGSTOP) };
    }
}

/// Resume a previously suspended process (best-effort mirror of
/// [`suspend_process`]).
pub fn resume_process(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let Some(resume) = windows::resume() else {
            return;
        };
        let Some(h) = windows::open_process(pid) else {
            return;
        };
        unsafe { resume(h) };
        windows::close(h);
    }
    #[cfg(not(target_os = "windows"))]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let _ = unsafe { kill(pid as i32, SIGCONT) };
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    /// PROCESS_SUSPEND_RESUME: the minimal access right for
    /// NtSuspendProcess / NtResumeProcess.
    const PROCESS_SUSPEND_RESUME: u32 = 0x0800;

    type NtFn = unsafe extern "system" fn(*mut c_void) -> i32;

    extern "system" {
        fn OpenProcess(dwdesiredaccess: u32, binherithandle: i32, dwprocessid: u32) -> *mut c_void;
        fn CloseHandle(hobject: *mut c_void) -> i32;
        fn LoadLibraryA(lpfilename: *const c_char) -> *mut c_void;
        fn GetProcAddress(hmodule: *mut c_void, lpprocname: *const c_char) -> *mut c_void;
    }

    struct Ntdll {
        suspend: Option<NtFn>,
        resume: Option<NtFn>,
    }

    /// Resolve NtSuspendProcess / NtResumeProcess from ntdll once. Runtime
    /// lookup instead of a link-time dependency so the CLI/GUI binaries
    /// never need ntdll.lib; a missing export degrades to "pause only stops
    /// new files", never to a crash.
    fn fns() -> &'static Ntdll {
        static FNS: OnceLock<Ntdll> = OnceLock::new();
        FNS.get_or_init(|| {
            // SAFETY: ntdll.dll is always loaded in the process, LoadLibrary
            // just bumps its refcount; the returned pointers are kept alive
            // for the process lifetime in `FNS`.
            unsafe {
                let ntdll = LoadLibraryA(c"ntdll.dll".as_ptr());
                if ntdll.is_null() {
                    return Ntdll {
                        suspend: None,
                        resume: None,
                    };
                }
                Ntdll {
                    suspend: lookup(ntdll, c"NtSuspendProcess".as_ptr()),
                    resume: lookup(ntdll, c"NtResumeProcess".as_ptr()),
                }
            }
        })
    }

    unsafe fn lookup(module: *mut c_void, name: *const c_char) -> Option<NtFn> {
        let raw = unsafe { GetProcAddress(module, name) };
        if raw.is_null() {
            None
        } else {
            // SAFETY: GetProcAddress returned a function pointer whose real
            // signature is `NTSTATUS(VOID*)` — exactly `NtFn`.
            Some(unsafe { std::mem::transmute::<*mut c_void, NtFn>(raw) })
        }
    }

    pub fn suspend() -> Option<NtFn> {
        fns().suspend
    }

    pub fn resume() -> Option<NtFn> {
        fns().resume
    }

    /// Open the pid for suspend/resume, if it still exists.
    pub fn open_process(pid: u32) -> Option<*mut c_void> {
        let h = unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid) };
        if h.is_null() {
            None
        } else {
            Some(h)
        }
    }

    pub fn close(handle: *mut c_void) {
        unsafe { CloseHandle(handle) };
    }
}

// Signal numbers are not stable across unixes: Linux and the BSD/macOS line
// disagree on both of these.
#[cfg(target_os = "macos")]
const SIGSTOP: i32 = 17;
#[cfg(target_os = "macos")]
const SIGCONT: i32 = 19;
#[cfg(all(unix, not(target_os = "macos")))]
const SIGSTOP: i32 = 19;
#[cfg(all(unix, not(target_os = "macos")))]
const SIGCONT: i32 = 18;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_roundtrip() {
        reset();
        assert!(!get());
        set(true);
        assert!(get());
        reset();
        assert!(!get());
    }
}
