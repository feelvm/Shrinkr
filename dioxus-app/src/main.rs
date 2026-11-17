//! Shrinkr — Dioxus desktop frontend.
//! shadcn-style dark UI from vendored rust-ui components + Tailwind.
//! Backend + orchestration live in `shrinkr-core` / `jobs`.
//! Threading rule: worker threads only send `JobMsg`; the UI-thread
//! coroutine applies them to the (unsync) state signal.

// This is a GUI app: never own a console window on Windows, in any build
// profile. Without this, launching the .exe pops up a terminal that owns
// the process (closing it kills the app). All diagnostics live in the
// in-app Log panel, so nothing is lost by detaching.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod close_guard;
mod components;
mod dialog;
mod jobs;
mod persist;
mod state;

use components::ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Badge, BadgeVariant, Button, ButtonVariant,
    Callout, CalloutVariant, Card, CardContent, CardHeader, CardTitle, Checkbox, Collapsible,
    Empty, EmptyDescription, EmptyTitle, Label, Progress, Slider, Spinner, Tabs, TabsContent,
    TabsList, TabsTrigger,
};
use dioxus::document;
use dioxus::html::{DragEvent, HasFileData};
use dioxus::prelude::*;
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures_util::StreamExt;
use jobs::JobMsg;
use shrinkr_core::images::{jpeg_q_for, webp_quality_for};
use shrinkr_core::log::{Level, LogEntry};
use shrinkr_core::media::human_bytes;
use shrinkr_core::pipeline::{
    preflight_auto_with, size_change_label, NvencPreset, Preflight, ScalePolicy, VideoBackend,
};
use state::{AppState, Goal};
use std::collections::HashSet;
use std::sync::{atomic::AtomicBool, Arc};

/// 64x64 RGBA dump of assets/icons/icon.png; decoded at compile time to
/// avoid pulling in an image codec just for the window icon.
const APP_ICON_RGBA: &[u8] = include_bytes!("../assets/icons/icon_64.rgba");

fn app_icon() -> dioxus::desktop::tao::window::Icon {
    dioxus::desktop::tao::window::Icon::from_rgba(APP_ICON_RGBA.to_vec(), 64, 64)
        .expect("bundled window icon has valid dimensions")
}

fn main() {
    // A panic in a Dioxus event/coroutine takes the window down with it,
    // and the log panel is memory-only — so record panics (with a
    // backtrace when the build carries one) in the on-disk log before the
    // process dies. This is what makes "it just closes, no logs" diagnosable.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The debounced saver may still be sleeping on the user's last
        // change; keep it before the process goes down.
        crate::persist::flush();
        let bt = std::backtrace::Backtrace::force_capture();
        shrinkr_core::log::to_file(&format!("PANIC: {info}\n{bt}"));
        default_hook(info);
    }));
    if let Some(path) = shrinkr_core::log::log_file() {
        shrinkr_core::log::to_file(&format!(
            "--- Shrinkr {} starting (log: {}) ---",
            env!("CARGO_PKG_VERSION"),
            path.display()
        ));
    }
    // Recover gigabytes a crashed run can leave behind (staged outputs are
    // full-size and Windows never cleans them up); background so startup
    // never waits on disk deletes.
    std::thread::spawn(shrinkr_core::ffmpeg::sweep_stale_work_roots);
    // NOTE: dioxus-desktop forces always-on-top in DEBUG builds unless told
    // otherwise ("so we can see it while we work"). A shrink tool that pins
    // itself above file dialogs is unusable, so spell out a normal window.
    use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
    let config = Config::new().with_window(
        WindowBuilder::new()
            .with_title("Shrinkr")
            .with_window_icon(Some(app_icon()))
            .with_inner_size(LogicalSize::new(1280.0, 900.0))
            .with_min_inner_size(LogicalSize::new(1024.0, 700.0))
            .with_decorations(true)
            .with_always_on_top(false),
    );
    #[cfg(target_os = "windows")]
    let config = match webview_data_dir() {
        Some(dir) => config.with_data_directory(dir),
        None => config,
    };
    // The desktop event loop calls `process::exit` when it ends, so this
    // call never returns; the last-used settings are written by the
    // close/`LoopDestroyed` event handler in `App`.
    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .launch(App);
}

/// WebView2 keeps its profile in a user-data directory. A profile left by an
/// older runtime, an elevated run, or a sandboxed run can make controller
/// creation fail with `ERROR_BUSY`; Dioxus then waits forever for a window
/// that never appears. Use a fresh, versioned profile for every launch and
/// fall back to the temp directory (then the executable's directory) if the
/// preferred location isn't writable.
#[cfg(target_os = "windows")]
fn webview_data_dir() -> Option<std::path::PathBuf> {
    let version = webview2_runtime_version().unwrap_or_else(|| "unknown".to_string());
    // A fresh profile per launch means a stale lock or crashed WebView2
    // process can never leave the next launch waiting forever.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let unique = format!("shrinkr-webview-{version}-{}-{stamp}", std::process::id());

    let mut bases = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        bases.push(std::path::PathBuf::from(local));
    }
    bases.push(std::env::temp_dir());
    let exe_base = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));

    let mut chosen = None;
    for base in &bases {
        let dir = base.join(&unique);
        if writable_dir(&dir) {
            chosen = Some(dir);
            break;
        }
    }
    let dir = chosen?;
    if let Some(exe_base) = exe_base {
        bases.push(exe_base);
    }
    // Every launch leaves a profile behind and each is tens of megabytes,
    // so the sweep runs against every candidate base (a previous launch
    // may have fallen back to a different one), in the background so
    // startup never waits on a multi-hundred-MB delete.
    let sweep_dir = dir.clone();
    std::thread::spawn(move || {
        for base in &bases {
            cleanup_old_webview_profiles(base, &sweep_dir);
        }
    });
    Some(dir)
}

/// Best-effort cleanup: every earlier launch leaves a WebView2 profile
/// behind and each is tens of megabytes, so anything older than an hour
/// is removed. The age floor spares a concurrently running instance's
/// profile; files still locked by a live webview simply fail the delete
/// and stay for the next sweep.
#[cfg(target_os = "windows")]
fn cleanup_old_webview_profiles(base: &std::path::Path, current: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path == current || !name.starts_with("shrinkr-webview-") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .map(|age| age.as_secs() > 60 * 60)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Installed WebView2 runtime version, used to version its profile directory.
#[cfg(target_os = "windows")]
fn webview2_runtime_version() -> Option<String> {
    let mut roots = Vec::new();
    for key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base) = std::env::var_os(key) {
            roots.push(std::path::PathBuf::from(base).join(r"Microsoft\EdgeWebView\Application"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(std::path::PathBuf::from(local).join(r"Microsoft\EdgeWebView\Application"));
    }

    let mut versions = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.chars().next().is_some_and(|c| c.is_ascii_digit())
                && path.join("msedgewebview2.exe").is_file()
            {
                versions.push(name);
            }
        }
    }
    versions.sort_by_key(|v| {
        v.split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    });
    versions.pop()
}

#[cfg(target_os = "windows")]
fn writable_dir(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Styles are inlined (rather than linked as manganis assets) so the
/// window is styled even without the `dx` asset server. Rebuild
/// `assets/tailwind.css` with the Tailwind CLI after markup changes —
/// the new output is picked up automatically at the next `cargo build`.
const TAILWIND_CSS: &str = include_str!("../assets/tailwind.css");
const SLIDER_CSS: &str = include_str!("../assets/slider.css");

#[component]
fn App() -> Element {
    // Load the persisted state once, before the first render (file reads,
    // not subprocesses — cheap enough for the first paint to stay fast).
    // Both loads happen in hooks: the disk is touched on the first render
    // only, not on every re-render this component goes through.
    let restored_queue = use_hook(persist::load_queue);
    let queue_for_initial = restored_queue.clone();
    let mut state = use_signal(move || {
        let mut initial = AppState::default();
        if let Some(saved) = persist::load() {
            saved.apply(&mut initial);
        }
        initial.history_count = persist::history_count();
        if let Some(snapshot) = &queue_for_initial {
            initial.queue_restoring = true;
            initial.push_log(format!(
                "Restoring {} unfinished file(s) from the previous session.",
                snapshot.file_count()
            ));
        }
        initial
    });
    use_context_provider(|| state);
    let pump = use_coroutine(move |mut rx: UnboundedReceiver<JobMsg>| async move {
        while let Some(m) = rx.next().await {
            // The close-guard answer needs the window handle (UI thread
            // only) — resolve it here, never inside apply_msg.
            if let JobMsg::CloseGuardResolved { quit } = m {
                let ctx = dioxus::desktop::window();
                use dioxus::desktop::WindowCloseBehaviour;
                if quit {
                    // The settings saver is debounced; closing right
                    // after a change must still keep that change.
                    persist::flush();
                    ctx.set_close_behavior(WindowCloseBehaviour::WindowCloses);
                    ctx.close();
                } else {
                    // Keep "hide while busy" active; just bring the
                    // window back from the close attempt.
                    ctx.window.set_visible(true);
                }
                continue;
            }
            jobs::apply_msg(&mut state, m);
        }
    });
    // Stored sender lets job handlers start chained batches on ExecDone.
    state.write().job_tx = Some(pump.tx());
    // Restore the unfinished queue written by the previous session. The
    // saved settings are already applied above; the queue snapshot keeps
    // its own per-segment settings, so a resume runs exactly what was
    // queued even if the sidebar knobs changed since.
    let mut restore_started = use_signal(|| false);
    use_effect(move || {
        if !restore_started() {
            restore_started.set(true);
            if let Some(snapshot) = restored_queue.clone() {
                crate::jobs::start_queue_restore(pump.tx(), snapshot);
            }
        }
    });
    // One silent update check shortly after launch (worker reports back
    // via JobMsg::UpdateChecked; result lands in the Header + Log).
    let mut auto_checked = use_signal(|| false);
    use_effect(move || {
        if !auto_checked() {
            auto_checked.set(true);
            state.write().update_checking = true;
            let tx = pump.tx();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(3));
                crate::jobs::start_update_check(tx, env!("CARGO_PKG_VERSION").to_string());
            });
        }
    });
    // Startup warmup, off the UI thread: tool probes + GPU/convert
    // capability detection used to run synchronously during the first
    // render and froze the window for seconds (a hung driver: up to 80).
    let mut warmed = use_signal(|| false);
    use_effect(move || {
        if !warmed() {
            warmed.set(true);
            let tx = pump.tx();
            crate::jobs::start_warmup(tx);
            persist::spawn_saver();
        }
    });
    // Close guard: mirror "anything running" into the static the
    // window-proc / event-handler paths read, and on non-Windows flip
    // dioxus' close behaviour to hide-while-busy (the dialog resolution
    // runs through the pump above). Windows intercepts WM_CLOSE natively
    // and needs no behaviour flip.
    use_effect(move || {
        let s = state.read();
        let busy =
            s.executing || s.convert_executing || s.optimize_executing || s.update_installing;
        drop(s);
        close_guard::set_busy(busy);
        #[cfg(not(target_os = "windows"))]
        {
            let ctx = dioxus::desktop::window();
            use dioxus::desktop::WindowCloseBehaviour;
            ctx.set_close_behavior(if busy {
                WindowCloseBehaviour::WindowHides
            } else {
                WindowCloseBehaviour::WindowCloses
            });
        }
    });
    // Every platform: a close request is the last moment the live state
    // is readable, so snapshot the settings and write them before the
    // window can go away. `LoopDestroyed` is the final event before tao
    // calls `process::exit` (desktop `launch` never returns), which is
    // the last chance to persist. This is what guarantees "I picked it,
    // closed it, and it is still there on the next launch": the
    // background saver is debounced and the settings effect can still be
    // a frame behind.
    //
    // macOS/Linux also ask here before closing a busy app: tao's close
    // request can't be prevented synchronously, so the "hide while busy"
    // behaviour set above keeps the window alive until the pump receives
    // the answer.
    {
        use dioxus::desktop::tao::event::{Event, WindowEvent};
        let s = state;
        #[cfg(not(target_os = "windows"))]
        let tx = pump.tx();
        dioxus::desktop::use_wry_event_handler(move |event, _target| match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested { .. },
                ..
            } => {
                // peek: the event handler is not a reactive context, and a
                // close must not register the whole state as a dependency.
                persist::offer(persist::collect(&s.peek()));
                persist::flush();
                #[cfg(not(target_os = "windows"))]
                if close_guard::should_intercept() {
                    close_guard::spawn_confirm_dialog(tx.clone());
                }
            }
            // The close request already snapshotted the live state; this
            // only makes sure the (debounced) write has happened before
            // tao replaces the process.
            Event::LoopDestroyed => persist::flush(),
            _ => {}
        });
    }
    // Windows-only: intercept the main window's WM_CLOSE while busy.
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::HasWindowHandle;
        let ctx = dioxus::desktop::window();
        let hwnd = ctx
            .window
            .window_handle()
            .ok()
            .and_then(|h| match h.as_raw() {
                raw_window_handle::RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
                _ => None,
            });
        if let Some(hwnd) = hwnd {
            close_guard::install(hwnd);
        }
    }
    // Settings saver: every state change refreshes the pending snapshot;
    // the background thread writes at most once per second. Collecting
    // ~25 small fields per message is far cheaper than the render it
    // rides on.
    use_effect(move || {
        let snapshot = persist::collect(&state.read());
        persist::offer(snapshot);
    });
    rsx! {
        style { dangerous_inner_html: TAILWIND_CSS }
        style { dangerous_inner_html: SLIDER_CSS }
        div { class: "dark flex h-screen w-screen flex-col overflow-hidden bg-background text-foreground",
            Header {}
            div { class: "flex min-h-0 flex-1",
                Sidebar {}
                Main {}
            }
        }
    }
}

/// Pump sender for click handlers. Read from state (set once by App)
/// instead of resolving the coroutine hook: calling a hook from inside an
/// event closure silently grew a hook slot per click.
fn pump_tx(state: &Signal<AppState>) -> UnboundedSender<JobMsg> {
    state
        .read()
        .job_tx
        .clone()
        .expect("pump sender is set once App mounts")
}

/// OS file-drop paths from an HTML5 drop event. On desktop the webview
/// synthesises the drop with real disk paths (see dioxus-desktop's
/// file-drop handler), so this covers Explorer/Finder drops on all OSes.
fn dropped_paths(evt: &DragEvent) -> Vec<std::path::PathBuf> {
    evt.data().files().into_iter().map(|f| f.path()).collect()
}

#[component]
fn Header() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let s = state.read();
    let tools_ok = s.ffmpeg_ok && s.ffprobe_ok;
    let checking = s.update_checking;
    let installing = s.update_installing;
    let available = s.update_available.clone();
    let ready = s.update_ready.clone();
    let busy = s.executing || s.convert_executing || s.optimize_executing;
    let fetching = s.ffmpeg_fetching;
    let fetch_pct = s.ffmpeg_fetch_pct;
    let fetch_supported = shrinkr_core::ffmpeg_fetch::supported();
    // GPU label + capability gaps come from the state copy (background
    // detection); reading hw::caps() directly here would block the first
    // paint on the trial-encode detection.
    let caps = s.caps;
    // Compile-time version: the header always names the running build.
    let version = env!("CARGO_PKG_VERSION");
    drop(s);
    // Say only what this machine can't do, and only for features the UI
    // exposes — everything supported is selectable (and greyed out when
    // missing) in the Pipeline dropdown, so no capability dump is needed.
    let (gpu_label, missing_note) = match caps {
        Some(caps) => {
            let mut missing: Vec<&str> = vec![];
            if !caps.hevc_nvenc {
                missing.push("NVENC HEVC");
            }
            if !caps.h264_nvenc {
                missing.push("NVENC H.264");
            }
            if !caps.svt_av1 {
                missing.push("CPU AV1");
            }
            let missing_note = if missing.is_empty() {
                String::new()
            } else {
                format!("unavailable: {}", missing.join(", "))
            };
            (caps.gpu_label.clone(), missing_note)
        }
        // Detection still running — the badge arrives with the result.
        None => (String::new(), String::new()),
    };
    let gpu_badge = !matches!(gpu_label.as_str(), "" | "none" | "unknown");
    rsx! {
        header { class: "relative flex shrink-0 items-center justify-between border-b border-border bg-card px-5 py-3",
            div {
                h1 { class: "text-lg font-bold tracking-tight",
                    "Shrinkr "
                    span { class: "text-[10px] font-medium text-muted-foreground tabular-nums", "v{version}" }
                }
            }
            div { class: "pointer-events-none absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2",
                if !tools_ok {
                    Badge { variant: BadgeVariant::Destructive, "ffmpeg missing" }
                }
            }
            div { class: "flex items-center gap-2",
                if !tools_ok {
                    if fetching {
                        div { class: "flex items-center gap-2 text-xs text-muted-foreground",
                            Spinner {}
                            "Downloading ffmpeg… {(fetch_pct * 100.0) as u32}%"
                        }
                    } else if fetch_supported {
                        Button {
                            variant: ButtonVariant::Default,
                            disabled: busy,
                            onclick: move |_| fetch_clicked(state),
                            "Download FFmpeg"
                        }
                    } else {
                        div { class: "text-xs text-muted-foreground",
                            "Get the release archive — it bundles ffmpeg"
                        }
                    }
                }
                if gpu_badge {
                    Badge { variant: BadgeVariant::Secondary, "{gpu_label}" }
                }
                if !missing_note.is_empty() {
                    Badge { variant: BadgeVariant::Destructive, "{missing_note}" }
                }
                if let Some(v) = ready {
                    Badge { variant: BadgeVariant::Success, "v{v} installed" }
                    Button {
                        variant: ButtonVariant::Default,
                        onclick: move |_| {
                            // restart_now exits this process; don't lose
                            // the knobs changed since the last saver tick.
                            crate::persist::flush();
                            if let Err(e) = shrinkr_core::update::restart_now() {
                                state.write().push_log(format!("ERROR: restart failed: {e:#}"));
                            }
                        },
                        "Restart now"
                    }
                } else if let Some(u) = available {
                    Badge { variant: BadgeVariant::Secondary, "v{u.version} available" }
                    if installing {
                        div { class: "flex items-center gap-2 text-xs text-muted-foreground",
                            Spinner {}
                            "Installing…"
                        }
                    } else {
                        Button {
                            variant: ButtonVariant::Default,
                            disabled: busy,
                            onclick: move |_| update_clicked(state),
                            "Update"
                        }
                    }
                } else if checking {
                    div { class: "flex items-center gap-2 text-xs text-muted-foreground",
                        Spinner {}
                        "Checking…"
                    }
                } else if installing {
                    div { class: "flex items-center gap-2 text-xs text-muted-foreground",
                        Spinner {}
                        "Installing…"
                    }
                } else {
                    Button {
                        variant: ButtonVariant::Ghost,
                        onclick: move |_| check_clicked(state),
                        "Check for updates"
                    }
                }
            }
        }
    }
}

/// UI-thread part of "Download FFmpeg": flag + worker handoff.
fn fetch_clicked(mut state: Signal<AppState>) {
    state.write().ffmpeg_fetching = true;
    state.write().ffmpeg_fetch_pct = 0.0;
    state.write().push_log(
        "Downloading FFmpeg (bundled builds come from github.com/BtbN/FFmpeg-Builds)…".into(),
    );
    crate::jobs::start_ffmpeg_fetch(pump_tx(&state));
}

/// UI-thread part of "Check for updates": flag + worker handoff.
fn check_clicked(mut state: Signal<AppState>) {
    state.write().update_checking = true;
    state.write().update_error = None;
    crate::jobs::start_update_check(pump_tx(&state), env!("CARGO_PKG_VERSION").to_string());
}

/// UI-thread part of "Update": refuse while a batch runs (the running
/// exe is being replaced), otherwise flag + worker handoff.
fn update_clicked(mut state: Signal<AppState>) {
    {
        let s = state.read();
        if s.executing || s.convert_executing || s.optimize_executing {
            drop(s);
            state.write().push_log(
                "Update deferred — a batch is running. Wait for it to finish, then press Update again.".into(),
            );
            return;
        }
    }
    state.write().update_installing = true;
    state.write().update_error = None;
    state
        .write()
        .push_log("Downloading + installing update…".into());
    crate::jobs::start_update_install(pump_tx(&state), env!("CARGO_PKG_VERSION").to_string());
}

#[component]
fn Sidebar() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let s = state.read();
    let files_summary = crate::sidebar_summary(&s);
    // Codec circles' checked state (unchecked = muted): the rows read it
    // directly, so a click greys the type out on the next frame.
    let muted_set = s.muted_codecs.clone();
    // History controls live in their own always-visible card: a scan that
    // skips every file must not hide the toggle that caused it.
    let skip_processed = s.skip_processed;
    let history_count = s.history_count;
    // Run state — progress, pause/cancel and the waiting queue live at the
    // bottom of this sidebar; start buttons sit in their workbench tabs.
    let executing = s.executing;
    let paused = s.paused;
    let queue_restoring = s.queue_restoring;
    // Optimize shares the run controls: its batch either sits queued
    // behind the shrink run or owns the workers.
    let optimize_executing = s.optimize_executing;
    let optimize_paused = s.optimize_paused;
    let optimize_done = s.optimize_done;
    let optimize_total = s.optimize_run.len();
    let optimize_queued = s.optimize_queued.as_ref().map(|b| b.indices.len());
    let exec_done = s.exec_done();
    let exec_total = s.exec_total;
    let exec_overall = s.exec_overall();
    let exec_items = s.exec_items.clone();
    // Live queue ETA + size estimate: the running batch plus every
    // waiting segment. Both grow when files are queued mid-run, since
    // these reads happen on the render that applies the addition.
    let queue_eta = s.queue_remaining_secs();
    let queue_size = s.queue_size_estimate();
    // The run's skip set (paths the user skipped or deleted from the
    // queue): rows read their "skipped" badge from it, so a click shows
    // on the next frame without waiting for the worker's confirmation
    // message. Cloning the whole set per render (a fresh HashSet of every
    // queued path, under the workers' lock) is replaced by two boolean
    // vectors resolved inside one short lock scope.
    let (exec_skipped, pending_skipped): (Vec<bool>, Vec<bool>) = {
        let guard = s.exec_skips.as_ref().and_then(|m| m.lock().ok());
        let in_set = |p: &std::path::Path| guard.as_ref().is_some_and(|g| g.contains(p));
        (
            s.exec_items.iter().map(|i| in_set(&i.path)).collect(),
            s.pending
                .iter()
                .flat_map(|seg| seg.files.iter())
                .map(|f| in_set(&f.path))
                .collect(),
        )
    };
    // Files already waiting behind the running batch (queued via the
    // Queue button or committed with Add to queue). They live in
    // `pending` until the batch drains, and must be visible here —
    // otherwise a fully-queued run reads as nothing happening. A skipped
    // one stays in its segment (it proves the decision to the user) but
    // no longer counts as waiting.
    let pending_files: Vec<(String, String, std::path::PathBuf, bool)> = s
        .pending
        .iter()
        .flat_map(|seg| seg.files.iter())
        .zip(&pending_skipped)
        .map(|(f, skipped)| {
            (
                f.path
                    .file_name()
                    .and_then(|x| x.to_str())
                    .unwrap_or("?")
                    .to_string(),
                f.res_label(),
                f.path.clone(),
                *skipped,
            )
        })
        .collect();
    // Mid-run scan results staged for the pick→settings→Add-to-queue
    // flow: visible in the library, not yet committed to the queue.
    let staged_files: Vec<(String, String, std::path::PathBuf)> = s
        .staged
        .iter()
        .map(|f| {
            (
                f.path
                    .file_name()
                    .and_then(|x| x.to_str())
                    .unwrap_or("?")
                    .to_string(),
                f.res_label(),
                f.path.clone(),
            )
        })
        .collect();
    let n_staged = staged_files.len();
    // Files addable to the running batch queue (the mid-run Queue
    // button): below-threshold files count too — the click is explicit.
    let n_queueable = s.queueable().len();
    drop(s);
    let waiting = pending_files.iter().filter(|(.., sk)| !*sk).count();
    let mut progress_label = format!(
        "Working {exec_done}/{exec_total} — {:.0}%",
        exec_overall * 100.0
    );
    if waiting > 0 {
        progress_label.push_str(&format!(" (+{waiting} waiting)"));
    }
    if paused {
        progress_label.push_str(" · paused");
    }
    rsx! {
        aside {
            class: "flex w-80 shrink-0 flex-col gap-4 overflow-y-auto border-r border-border bg-card p-4",
            Card {
                CardHeader {
                    CardTitle { "Library" }
                }
                CardContent {
                    for (codec , n , bytes) in files_summary {
                        {
                            // Round checkbox per codec type: unchecked mutes
                            // the type — the row greys out and its files
                            // never pass to the queue.
                            let muted = muted_set.contains(&codec);
                            let codec_toggle = codec.clone();
                            rsx! {
                                div { class: "flex items-center justify-between gap-2 py-0.5 text-xs",
                                    div { class: "flex min-w-0 items-center gap-2",
                                        Checkbox {
                                            class: "rounded-full",
                                            checked: !muted,
                                            aria_label: "process {codec}",
                                            on_checked_change: move |on| {
                                                toggle_codec(state, &codec_toggle, on);
                                            },
                                        }
                                        span {
                                            class: if muted {
                                                "truncate text-muted-foreground/60 line-through"
                                            } else {
                                                "truncate"
                                            },
                                            "{codec}: {n} files"
                                        }
                                    }
                                    span { class: "text-muted-foreground", "{human_bytes(bytes)}" }
                                }
                            }
                        }
                    }
                }
            }
            Card {
                CardHeader {
                    CardTitle { "History" }
                }
                CardContent {
                    div { class: "flex flex-col gap-2",
                        label { class: "flex items-center gap-2 text-sm",
                            Checkbox {
                                checked: skip_processed,
                                on_checked_change: move |v| state.write().skip_processed = v,
                            }
                            "Skip already-processed"
                        }
                        if history_count > 0 {
                            div { class: "flex items-center justify-between gap-2",
                                span { class: "text-xs text-muted-foreground", "{history_count} file(s)" }
                                Button {
                                    variant: ButtonVariant::Ghost,
                                    class: "h-6 px-2 text-xs",
                                    onclick: move |_| clear_history_clicked(state),
                                    "Clear"
                                }
                            }
                        }
                    }
                }
            }
            // Run control: optimize progress when it owns the workers,
            // otherwise the shrink batch progress + pause/cancel. Start
            // buttons sit in their workbench tabs; this panel only
            // monitors and stops whatever is running.
            div { class: "flex flex-col gap-2",
                if optimize_executing {
                    {
                        let pct = optimize_done as f64 / optimize_total.max(1) as f64 * 100.0;
                        rsx! {
                            Progress { value: pct }
                            p { class: "text-xs text-muted-foreground",
                                if optimize_paused {
                                    "Optimizing {optimize_done}/{optimize_total} · paused"
                                } else {
                                    "Optimizing {optimize_done}/{optimize_total}."
                                }
                            }
                            Button {
                                variant: ButtonVariant::Secondary,
                                class: "w-full",
                                onclick: move |_| crate::jobs::pause_optimize_exec(state),
                                if optimize_paused { "Resume" } else { "Pause" }
                            }
                            Button {
                                variant: ButtonVariant::Destructive,
                                class: "w-full",
                                onclick: move |_| crate::jobs::cancel_optimize(state),
                                "Cancel"
                            }
                        }
                    }
                } else if !executing {
                    if queue_restoring {
                        div { class: "flex items-center gap-2 text-xs text-muted-foreground",
                            Spinner {}
                            "Restoring queue."
                        }
                    }
                    if !pending_files.is_empty() {
                        div { class: "flex flex-col gap-2 rounded-md border border-border p-2",
                            p { class: "text-xs font-semibold", "Restored queue ({waiting})" }
                            div { class: "flex max-h-40 flex-col gap-1 overflow-y-auto",
                                for (p_name , p_res , p_path , p_skipped) in pending_files {
                                    {
                                        let p_del = p_path.clone();
                                        rsx! {
                                            div { class: "flex items-center gap-2",
                                                span { class: "min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground/80", "{p_name}" }
                                                if p_res != "?" {
                                                    span { class: "shrink-0 text-xs text-muted-foreground/60", "{p_res}" }
                                                }
                                                span { class: "shrink-0 text-xs text-muted-foreground/60",
                                                    if p_skipped { "skipped" } else { "waiting" }
                                                }
                                                button {
                                                    class: "shrink-0 rounded px-1 text-xs text-muted-foreground/60 hover:bg-accent hover:text-accent-foreground",
                                                    title: "Remove from the restored queue",
                                                    onclick: move |_| {
                                                        let mut w = state.write();
                                                        for seg in w.pending.iter_mut() {
                                                            seg.files.retain(|f| f.path != p_del);
                                                        }
                                                        w.pending.retain(|seg| !seg.files.is_empty());
                                                        crate::persist::save_queue(&w);
                                                    },
                                                    "✕"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Button {
                                variant: ButtonVariant::Default,
                                class: "w-full",
                                onclick: move |_| resume_queue_clicked(state, pump_tx(&state)),
                                "Resume queue"
                            }
                            Button {
                                variant: ButtonVariant::Ghost,
                                class: "w-full",
                                onclick: move |_| discard_restored(state),
                                "Discard"
                            }
                        }
                    }
                } else {
                    Progress { value: exec_overall * 100.0 }
                    p { class: "text-xs text-muted-foreground", "{progress_label}" }
                    if let Some(n) = optimize_queued {
                        div { class: "flex items-center justify-between gap-2 rounded-md border border-border px-2 py-1",
                            span { class: "text-xs text-muted-foreground", "{n} optimize file(s) queued" }
                            button {
                                class: "shrink-0 rounded px-1 text-xs text-muted-foreground hover:bg-accent hover:text-accent-foreground",
                                onclick: move |_| crate::jobs::cancel_queued_optimize(state),
                                "✕"
                            }
                        }
                    }
                    if let Some(secs) = queue_eta {
                        p { class: "text-xs text-muted-foreground",
                            "About {fmt_secs(secs)} left in the queue"
                        }
                    }
                    if let Some((src, out)) = queue_size {
                        p { class: "text-xs text-muted-foreground",
                            "{human_bytes(src)} → {human_bytes(out)} ({fmt_size_change(src, out)})"
                        }
                    }
                    p { class: "text-xs font-semibold", "Per-file progress ({exec_done}/{exec_total})" }
                    div { class: "flex max-h-40 flex-col gap-2 overflow-y-auto",
                        for (row , item) in exec_items.iter().enumerate() {
                            {
                                // Deleted from the queue before its turn: gone
                                // from the panel (the log line remembers it).
                                let skipped = exec_skipped[row];
                                let path = item.path.clone();
                                // One PathBuf clone per control: the move
                                // closures each own their file target.
                                let path_del = path.clone();
                                // In-flight (progress started) and finished
                                // rows can no longer be skipped or deleted.
                                let actionable = !item.done && !item.failed && item.frac == 0.0;
                                rsx! {
                                    if !item.removed {
                                        div { class: "flex flex-col gap-1",
                                            div { class: "flex items-center gap-2",
                                                span { class: "min-w-0 flex-1 truncate font-mono text-xs", "{item.name}" }
                                                if item.res != "?" {
                                                    span { class: "shrink-0 text-xs text-muted-foreground", "{item.res}" }
                                                }
                                                span { class: "shrink-0 text-xs text-muted-foreground",
                                                    if skipped { "skipped" } else { "{item.status_text()}" }
                                                }
                                                if actionable && !skipped {
                                                    button {
                                                        class: "shrink-0 rounded px-1 text-xs text-muted-foreground hover:bg-accent hover:text-accent-foreground",
                                                        title: "Skip — this file will not be encoded",
                                                        onclick: move |_| crate::jobs::skip_queued(state, &path),
                                                        "Skip"
                                                    }
                                                    button {
                                                        class: "shrink-0 rounded px-1 text-xs text-muted-foreground hover:bg-accent hover:text-accent-foreground",
                                                        title: "Delete from queue",
                                                        onclick: move |_| crate::jobs::unqueue(state, &path_del),
                                                        "✕"
                                                    }
                                                }
                                            }
                                            Progress { value: item.frac * 100.0 }
                                        }
                                    }
                                }
                            }
                        }
                        // Files queued behind the running batch (queued in
                        // via the Queue button or the Add to queue click).
                        // Same row but with no bar: they start when the
                        // current batch drains (logs say when each joined).
                        // Skip/delete act straight on the waiting list.
                        for (p_name , p_res , p_path , p_skipped) in pending_files {
                            {
                                // One PathBuf clone per control: the move
                                // closures each own their file target.
                                let p_del = p_path.clone();
                                rsx! {
                                    div { class: "flex flex-col gap-1",
                                        div { class: "flex items-center gap-2",
                                            span { class: "min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground/60", "{p_name}" }
                                            if p_res != "?" {
                                                span { class: "shrink-0 text-xs text-muted-foreground/60", "{p_res}" }
                                            }
                                            span { class: "shrink-0 text-xs text-muted-foreground/60",
                                                if p_skipped { "skipped" } else { "waiting" }
                                            }
                                            if !p_skipped {
                                                button {
                                                    class: "shrink-0 rounded px-1 text-xs text-muted-foreground/60 hover:bg-accent hover:text-accent-foreground",
                                                    title: "Skip — this file will not be encoded",
                                                    onclick: move |_| crate::jobs::skip_queued(state, &p_path),
                                                    "Skip"
                                                }
                                                button {
                                                    class: "shrink-0 rounded px-1 text-xs text-muted-foreground/60 hover:bg-accent hover:text-accent-foreground",
                                                    title: "Delete from queue",
                                                    onclick: move |_| crate::jobs::unqueue(state, &p_del),
                                                    "✕"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Mid-run picks scanned into the library but not
                        // yet committed: they queue only via the button
                        // below, with the settings picked at that moment.
                        // Deleting here just leaves staging.
                        for (s_name , s_res , s_path) in staged_files {
                            div { class: "flex flex-col gap-1",
                                div { class: "flex items-center gap-2",
                                    span { class: "min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground/60", "{s_name}" }
                                    if s_res != "?" {
                                        span { class: "shrink-0 text-xs text-muted-foreground/60", "{s_res}" }
                                    }
                                    span { class: "shrink-0 text-xs text-muted-foreground/60", "staged — add to queue" }
                                    button {
                                        class: "shrink-0 rounded px-1 text-xs text-muted-foreground/60 hover:bg-accent hover:text-accent-foreground",
                                        title: "Delete from queue",
                                        onclick: move |_| crate::jobs::unqueue(state, &s_path),
                                        "✕"
                                    }
                                }
                            }
                        }
                    }
                    // Commit staged picks: their files encode with the
                    // settings picked at this moment (explicit click ⇒
                    // below-threshold files run with a warning).
                    if n_staged > 0 {
                        Button {
                            variant: ButtonVariant::Default,
                            class: "w-full",
                            onclick: move |_| add_staged_clicked(state),
                            "Add {n_staged} to queue"
                        }
                    }
                    // Mid-run manual queue: everything preflight can run
                    // on joins the waiting queue (below-threshold files
                    // run with a warning; the no-gain guard keeps
                    // originals safe when estimates are optimistic).
                    Button {
                        variant: ButtonVariant::Default,
                        class: "w-full",
                        disabled: n_queueable == 0,
                        onclick: move |_| queue_clicked(state),
                        "Queue {n_queueable} more file(s)"
                    }
                    Button {
                        variant: ButtonVariant::Secondary,
                        class: "w-full",
                        onclick: move |_| crate::jobs::pause_exec(state),
                        // Pause freezes the in-flight encodes in place and
                        // holds the waiting files; the same button resumes.
                        if paused { "Resume" } else { "Pause" }
                    }
                    Button {
                        variant: ButtonVariant::Destructive,
                        class: "w-full",
                        onclick: move |_| crate::jobs::cancel_exec(state),
                        "Cancel"
                    }
                }
            }
        }
    }
}

/// (codec, count, bytes) sorted by bytes desc.
fn sidebar_summary(s: &AppState) -> Vec<(String, usize, u64)> {
    use std::collections::HashMap;
    let mut by: HashMap<String, (usize, u64)> = HashMap::new();
    for f in &s.files {
        let e = by.entry(f.vcodec.clone()).or_insert((0, 0));
        e.0 += 1;
        e.1 += f.bytes;
    }
    let mut rows: Vec<_> = by.into_iter().map(|(c, (n, b))| (c, n, b)).collect();
    rows.sort_by_key(|(_, _, b)| std::cmp::Reverse(*b));
    rows
}

/// Library codec circle toggled: unchecked mutes the type — its files
/// grey out in the library and every queue gate drops them (Process,
/// mid-run Queue, mid-run staging). Files already passed to the queue
/// keep running; the mute governs what gets queued from now on.
fn toggle_codec(mut state: Signal<AppState>, codec: &str, on: bool) {
    let mut w = state.write();
    if on {
        w.muted_codecs.remove(codec);
    } else {
        w.muted_codecs.insert(codec.to_string());
    }
    let n = w.files.iter().filter(|f| f.vcodec == codec).count();
    if on {
        w.push_log(format!(
            "{codec} re-enabled — {n} file(s) back in the plan."
        ));
    } else {
        let mut line =
            format!("{codec} muted — {n} file(s) stay in the library and won't be queued.");
        if w.executing {
            line.push_str(" Files already queued keep running.");
        }
        w.push_log(line);
    }
}

#[component]
fn Main() -> Element {
    let state = use_context::<Signal<AppState>>();
    let empty = state.read().files.is_empty();
    let tools_ok = {
        let s = state.read();
        s.ffmpeg_ok && s.ffprobe_ok
    };
    // The benchmark matrix is video-only, so the card follows the
    // library like the video knobs: no video loaded, no card.
    let has_video = state.read().has_videos();
    rsx! {
        main { class: "min-w-0 flex-1 overflow-y-auto p-5",
            div { class: "mx-auto flex max-w-4xl flex-col gap-4",
                if !tools_ok {
                    Alert { variant: AlertVariant::Destructive,
                        AlertTitle { "Tools missing" }
                        AlertDescription {
                            "ffmpeg / ffprobe were not found on PATH. Install them and restart."
                        }
                    }
                }
                Tabs { default_value: "shrink",
                    TabsList {
                        TabsTrigger { value: "shrink", "Shrink" }
                        TabsTrigger { value: "convert", "Convert" }
                        TabsTrigger { value: "optimize", "Optimize" }
                    }
                    TabsContent { value: "shrink",
                        div { class: "flex flex-col gap-4",
                            ImportBar { mode: ImportMode::Shrink }
                            if empty {
                                ShrinkEmpty {}
                            } else {
                                PipelineCard {}
                                EstimateCard {}
                                if has_video {
                                    BenchCard {}
                                }
                            }
                        }
                    }
                    TabsContent { value: "convert",
                        ConvertCard {}
                    }
                    TabsContent { value: "optimize",
                        div { class: "flex flex-col gap-4",
                            ImportBar { mode: ImportMode::Optimize }
                            OptimizeCard {}
                            OptimizeSummaryCard {}
                            OptimizeFilesCard {}
                        }
                    }
                }
                LogCard {}
            }
        }
    }
}

/// Which pipeline a workspace import strip feeds.
#[derive(Clone, Copy, PartialEq)]
enum ImportMode {
    Shrink,
    Optimize,
}

/// Shared import strip at the top of a workbench tab: the same buttons and
/// native dialogs everywhere, with the tab deciding the filter and where
/// the files land. Also the tab's drop target.
#[component]
fn ImportBar(mode: ImportMode) -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let parent = crate::dialog::DialogParent::capture(&dioxus::desktop::use_window());
    let s = state.read();
    let optimize = mode == ImportMode::Optimize;
    let targets: Vec<std::path::PathBuf> = s.targets.clone();
    let scanning = s.scanning;
    let probing = s.probing;
    let probe_done = s.probe_done;
    let probe_total = s.probe_total;
    // Clear exists on both sides: shrink owns the library + pending scan
    // list; optimize owns its picked files.
    let has_selection =
        !s.files.is_empty() || !targets.is_empty() || !s.staged.is_empty() || scanning || probing;
    let opt_count = s.optimize_files.len();
    let opt_scanning = s.optimize_scanning;
    let opt_locked = s.optimize_executing || s.optimize_queued.is_some();
    drop(s);
    let mut drag_over = use_signal(|| false);
    let drag_cls = if drag_over() {
        "ring-2 ring-inset ring-primary"
    } else {
        ""
    };
    let probe_pct = if probe_total > 0 {
        probe_done as f64 / probe_total as f64 * 100.0
    } else {
        0.0
    };
    let probe_label = format!("Probing {probe_done}/{probe_total} …");
    let status = if optimize {
        if opt_scanning {
            "Scanning.".to_string()
        } else if opt_count > 0 {
            format!("{opt_count} file(s)")
        } else {
            String::new()
        }
    } else if scanning {
        "Walking subdirectories…".to_string()
    } else if !targets.is_empty() {
        format!("{} waiting to scan", targets.len())
    } else {
        String::new()
    };
    let clear_disabled = if optimize {
        opt_count == 0 || opt_locked
    } else {
        !has_selection
    };
    let import_disabled = optimize && opt_locked;
    let add_label = if optimize {
        "Add images/PDFs…"
    } else {
        "Add files…"
    };
    rsx! {
        div {
            class: "rounded-xl border border-border bg-card p-3 shadow-sm {drag_cls}",
            ondragenter: move |_| drag_over.set(true),
            ondragleave: move |_| drag_over.set(false),
            ondragover: move |evt| {
                evt.prevent_default();
                drag_over.set(true);
            },
            ondrop: move |evt| {
                evt.prevent_default();
                drag_over.set(false);
                let paths = dropped_paths(&evt);
                if paths.is_empty() {
                    return;
                }
                if optimize {
                    let w = state.read();
                    let locked = w.optimize_executing || w.optimize_queued.is_some();
                    drop(w);
                    if !locked {
                        crate::jobs::start_optimize_scan(pump_tx(&state), paths);
                    }
                } else {
                    let _ = pump_tx(&state).unbounded_send(JobMsg::TargetsAdded(paths));
                }
            },
            div { class: "flex flex-wrap items-center gap-2",
                Button {
                    variant: ButtonVariant::Outline,
                    disabled: import_disabled,
                    onclick: move |_| {
                        let tx = pump_tx(&state);
                        std::thread::spawn(move || {
                            let mut dlg = rfd::FileDialog::new();
                            if let Some(p) = &parent {
                                dlg = dlg.set_parent(p);
                            }
                            if optimize {
                                if let Some(paths) = dlg
                                    .add_filter("optimize", &["pdf", "png", "jpg", "jpeg"])
                                    .pick_files()
                                {
                                    crate::jobs::start_optimize_scan(tx, paths);
                                }
                            } else if let Some(paths) = dlg
                                .add_filter("media", shrinkr_core::media::MEDIA_EXTS)
                                .add_filter("images", shrinkr_core::images::IMAGE_SHRINK_EXTS)
                                .pick_files()
                            {
                                let _ = tx.unbounded_send(JobMsg::TargetsAdded(paths));
                            }
                        });
                    },
                    "{add_label}"
                }
                Button {
                    variant: ButtonVariant::Outline,
                    disabled: import_disabled,
                    onclick: move |_| {
                        let tx = pump_tx(&state);
                        std::thread::spawn(move || {
                            let mut dlg = rfd::FileDialog::new();
                            if let Some(p) = &parent {
                                dlg = dlg.set_parent(p);
                            }
                            if let Some(paths) = dlg.pick_folders() {
                                if optimize {
                                    crate::jobs::start_optimize_scan(tx, paths);
                                } else {
                                    let _ = tx.unbounded_send(JobMsg::TargetsAdded(paths));
                                }
                            }
                        });
                    },
                    "Add folders…"
                }
                Button {
                    variant: ButtonVariant::Ghost,
                    disabled: clear_disabled,
                    onclick: move |_| {
                        if optimize {
                            let mut w = state.write();
                            if !w.optimize_executing && w.optimize_queued.is_none() {
                                w.optimize_files.clear();
                                w.optimize_done = 0;
                                w.optimize_run.clear();
                                w.optimize_summary = None;
                            }
                        } else {
                            crate::clear_clicked(state);
                        }
                    },
                    "Clear"
                }
                if !status.is_empty() {
                    span { class: "ml-auto flex items-center gap-2 text-xs text-muted-foreground",
                        if scanning || opt_scanning {
                            Spinner {}
                        }
                        "{status}"
                    }
                }
            }
            if !optimize {
                if !targets.is_empty() {
                    div { class: "mt-2 max-h-24 overflow-y-auto rounded-md border border-border p-2",
                        for (i , t) in targets.iter().enumerate() {
                            div { class: "flex items-center justify-between gap-2 py-0.5",
                                span { class: "truncate text-xs text-muted-foreground",
                                    "{t.display()}"
                                }
                                button {
                                    class: "shrink-0 rounded px-1 text-xs text-muted-foreground hover:bg-accent hover:text-accent-foreground",
                                    onclick: move |_| {
                                        let mut w = state.write();
                                        if i < w.targets.len() {
                                            w.targets.remove(i);
                                        }
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                }
                if probing {
                    div { class: "mt-2",
                        Progress { value: probe_pct }
                        p { class: "mt-1 text-xs text-muted-foreground", "{probe_label}" }
                    }
                }
            }
        }
    }
}

/// Shrink-tab empty state; the import strip above does the adding.
#[component]
fn ShrinkEmpty() -> Element {
    rsx! {
        Card {
            CardContent {
                div { class: "rounded-md border border-dashed border-border p-6 text-center",
                    Empty {
                        EmptyTitle { "No files yet" }
                        EmptyDescription {
                            "Add folders or files above — scanning starts automatically."
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn PipelineCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let s = state.read();
    let preserve_quality = s.preserve_quality;
    let backend = s.backend;
    let preset = s.nvenc_preset;
    // Protection pins video quality and preserves tracks; resolution and
    // audio targets remain editable and resolve separately for each file.
    let scale = s.effective_scale();
    let cq = s.effective_cq();
    let image_cq = s.image_cq;
    let image_scale = s.image_scale;
    let image_preserve_format = s.image_preserve_format;
    let keep_subs = s.effective_keep_subs();
    let all_audio = preserve_quality || s.all_audio;
    let film_grain = !preserve_quality && s.film_grain;
    let skip_efficient = s.skip_efficient;
    let replace = s.replace;
    let min_saving = s.min_saving_pct;
    let parallel = s.parallel;
    let extra_args = s.extra_args.clone();
    let opus_bps = s.effective_opus();
    // Audio-only pass (copy video + an explicit Opus target): size
    // thresholds don't apply, so the threshold controls disable themselves.
    let audio_only = backend == VideoBackend::CopyVideo && opus_bps.is_some();
    // Sections follow the library: video knobs only when a video is
    // loaded, image knobs only when an image is — no NVENC/audio rows
    // for a folder of photos.
    let has_video = s.has_videos();
    let has_image = s.has_images();
    // Same total the Estimate card reports, lifted into the header so the
    // source → output number stays visible while tuning the pipeline.
    let (est_in, est_out, _, _) = s.estimate();
    // Capabilities come from the state copy, not a blocking hw::caps().
    let caps = s.caps;
    // Run controls: the Process/Retry buttons live here with the settings
    // they apply; progress and Pause/Cancel stay in the sidebar.
    let executing = s.executing;
    let scan_busy = s.scanning || s.probing;
    let queue_restoring = s.queue_restoring;
    let pending_waiting = !s.pending.is_empty();
    let optimize_busy = s.optimize_executing || s.optimize_queued.is_some();
    let n_eligible = s.eligible().len();
    let n_retry = s.retry_files().len();
    drop(s);
    let cq_label = if preserve_quality {
        "Auto".to_string()
    } else {
        format!("{cq}")
    };
    let image_cq_label = format!("{image_cq}");
    let image_quality_note = format!(
        "Maps to JPEG q{} / WebP q{} — lower = smaller files.",
        jpeg_q_for(image_cq),
        webp_quality_for(image_cq)
    );
    let saving_label = format!("{min_saving:.0}");
    let parallel_label = format!("{parallel}");
    let process_label = format!("Process {n_eligible} file(s)");
    let retry_file_word = if n_retry == 1 { "file" } else { "files" };
    let estimate_label = (est_in > 0).then(|| {
        format!(
            "{} → {} • {}",
            human_bytes(est_in),
            human_bytes(est_out),
            size_change_label(est_in, est_out)
        )
    });
    // Capability gating: hardware backends this machine can't run are
    // absent from the dropdown entirely (detection trials the AV1
    // variants with the real flag set) — the menu never offers an option
    // that would fail over to a fallback. Until background detection
    // lands, only the always-available CPU options show; the hardware
    // entries appear with the result.
    let (av1_nvenc, hevc_nvenc, h264_nvenc, vt_hevc, qsv_hevc, qsv_av1, amf_hevc, amf_av1, svt_av1) =
        match caps {
            Some(c) => (
                c.av1_nvenc_usable,
                c.hevc_nvenc,
                c.h264_nvenc,
                c.vt_hevc,
                c.qsv_hevc,
                c.qsv_av1_usable,
                c.amf_hevc,
                c.amf_av1_usable,
                c.svt_av1,
            ),
            None => (
                false, false, false, false, false, false, false, false, false,
            ),
        };
    rsx! {
        Card {
            CardHeader {
                div { class: "flex w-full items-center justify-between gap-3",
                    CardTitle { "Pipeline" }
                    if let Some(label) = estimate_label {
                        span { class: "text-right text-xs text-muted-foreground tabular-nums",
                            "{label}"
                        }
                    }
                }
            }
            CardContent {
                div { class: "flex flex-col gap-4",
                    if has_video {
                        GoalPicker {}
                        div { class: "flex flex-col gap-1",
                            label { class: "flex items-center gap-2 text-sm",
                                Checkbox {
                                    checked: preserve_quality,
                                    on_checked_change: move |v| state.write().preserve_quality = v,
                                }
                                "Preserve quality"
                            }
                        }
                    }
                    div { class: "grid grid-cols-1 gap-3 md:grid-cols-3",
                        div { class: "flex flex-col gap-1",
                            Label { "Backend" }
                            select {
                                class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm",
                                onchange: move |e: FormEvent| {
                                    let mut w = state.write();
                                    w.backend = match e.value().as_str() {
                                        "av1-nvenc" => VideoBackend::Av1Nvenc,
                                        "hevc" => VideoBackend::HevcNvenc,
                                        "h264" => VideoBackend::H264Nvenc,
                                        "vt-hevc" => VideoBackend::VtHevc,
                                        "qsv-hevc" => VideoBackend::QsvHevc,
                                        "qsv-av1" => VideoBackend::QsvAv1,
                                        "amf-hevc" => VideoBackend::AmfHevc,
                                        "amf-av1" => VideoBackend::AmfAv1,
                                        "x264" => VideoBackend::CpuX264,
                                        "x265" => VideoBackend::CpuX265,
                                        "av1" => VideoBackend::CpuAv1,
                                        "copy" => VideoBackend::CopyVideo,
                                        _ => VideoBackend::Auto,
                                    };
                                    // P7–P9 are SVT-only effort steps. Leaving
                                    // one selected on a hardware encoder would
                                    // show the wrong option and clamp silently.
                                    if w.backend != VideoBackend::CpuAv1
                                        && matches!(
                                            w.nvenc_preset,
                                            NvencPreset::P7 | NvencPreset::P8 | NvencPreset::P9
                                        )
                                    {
                                        w.nvenc_preset = NvencPreset::P6;
                                    }
                                },
                                // Hardware backends appear only when this
                                // machine can actually run them (detection
                                // includes a trial encode for the AV1
                                // variants) — an incompatible option is
                                // never shown, never mind selectable.
                                option { value: "auto", selected: backend == VideoBackend::Auto, "Auto (best HW → CPU)" }
                                if av1_nvenc {
                                    option { value: "av1-nvenc", selected: backend == VideoBackend::Av1Nvenc, "NVENC AV1 (RTX 40+)" }
                                }
                                if hevc_nvenc {
                                    option { value: "hevc", selected: backend == VideoBackend::HevcNvenc, "NVENC HEVC" }
                                }
                                if h264_nvenc {
                                    option { value: "h264", selected: backend == VideoBackend::H264Nvenc, "NVENC H.264" }
                                }
                                if vt_hevc {
                                    option { value: "vt-hevc", selected: backend == VideoBackend::VtHevc, "VideoToolbox HEVC (Apple)" }
                                }
                                if qsv_hevc {
                                    option { value: "qsv-hevc", selected: backend == VideoBackend::QsvHevc, "Quick Sync HEVC (Intel)" }
                                }
                                if qsv_av1 {
                                    option { value: "qsv-av1", selected: backend == VideoBackend::QsvAv1, "Quick Sync AV1 (Arc)" }
                                }
                                if amf_hevc {
                                    option { value: "amf-hevc", selected: backend == VideoBackend::AmfHevc, "AMF HEVC (AMD)" }
                                }
                                if amf_av1 {
                                    option { value: "amf-av1", selected: backend == VideoBackend::AmfAv1, "AMF AV1 (RX 7000+)" }
                                }
                                option { value: "x264", selected: backend == VideoBackend::CpuX264, "CPU x264 (CRF = slider)" }
                                option { value: "x265", selected: backend == VideoBackend::CpuX265, "CPU x265 slow (CRF = slider)" }
                                if svt_av1 {
                                    option { value: "av1", selected: backend == VideoBackend::CpuAv1, "CPU AV1 SVT (slow)" }
                                }
                                option { value: "copy", selected: backend == VideoBackend::CopyVideo, "Audio only" }
                            }
                        }
                        div { class: "flex flex-col gap-1",
                            Label { "Encoder preset" }
                            select {
                                class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm",
                                onchange: move |e: FormEvent| {
                                    state.write().nvenc_preset = match e.value().as_str() {
                                        "p3" => NvencPreset::P3,
                                        "p4" => NvencPreset::P4,
                                        "p6" => NvencPreset::P6,
                                        "p7" => NvencPreset::P7,
                                        "p8" => NvencPreset::P8,
                                        "p9" => NvencPreset::P9,
                                        _ => NvencPreset::P5,
                                    };
                                },
                                option { value: "p3", selected: preset == NvencPreset::P3, "P3 (fast)" }
                                option { value: "p4", selected: preset == NvencPreset::P4, "P4 (medium)" }
                                option { value: "p5", selected: preset == NvencPreset::P5, "P5 (slow)" }
                                option { value: "p6", selected: preset == NvencPreset::P6, "P6 (slower)" }
                                // SVT-only effort steps: presets 6/5/4 are
                                // minutes-per-minute encodes, so they appear
                                // only on the backend that honours them.
                                if backend == VideoBackend::CpuAv1 {
                                    option { value: "p7", selected: preset == NvencPreset::P7, "P7 (SVT 6)" }
                                    option { value: "p8", selected: preset == NvencPreset::P8, "P8 (SVT 5)" }
                                    option { value: "p9", selected: preset == NvencPreset::P9, "P9 (SVT 4)" }
                                }
                            }
                        }
                        div { class: "flex flex-col gap-1",
                            Label { "Resolution" }
                            ScalePicker {
                                value: scale,
                                on_change: move |v| state.write().scale = v,
                            }
                        }
                    }
                    div { class: "flex items-center gap-3",
                        div { class: "w-28 shrink-0",
                            Label { "Audio" }
                        }
                        select {
                            class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm disabled:opacity-50 disabled:cursor-not-allowed",
                            onchange: move |e: FormEvent| {
                                state.write().opus_bps = match e.value().as_str() {
                                    "off" => None,
                                    v => v.parse::<u32>().ok().map(|k| k * 1000),
                                };
                            },
                            option { value: "off", selected: opus_bps.is_none(), "Off (keep as-is)" }
                            for kbps in shrinkr_core::pipeline::OPUS_TARGETS_KBPS {
                                {
                                    let max = kbps == shrinkr_core::pipeline::MAX_OPUS_BPS / 1000;
                                    let label = if max {
                                        format!("Opus {kbps}k (max)")
                                    } else {
                                        format!("Opus {kbps}k")
                                    };
                                    rsx! {
                                        option {
                                            value: "{kbps}",
                                            selected: opus_bps == Some(kbps * 1000),
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                        // Lives beside the audio settings it drives, not in
                        // the Goal ladder: it's a remux-speed mode where the
                        // speed↔size trade-off doesn't apply. Checked state
                        // derives from the backend, so picking Copy video in
                        // the backend dropdown checks it too.
                        label { class: "flex items-center gap-2 text-sm",
                            Checkbox {
                                checked: backend == VideoBackend::CopyVideo,
                                on_checked_change: move |v| {
                                    let mut w = state.write();
                                    if v {
                                        if w.backend != VideoBackend::CopyVideo {
                                            w.last_video_backend = w.backend;
                                        }
                                        w.backend = VideoBackend::CopyVideo;
                                        // An upscale needs a target; the 64k
                                        // default would just re-copy most sources.
                                        if w.opus_bps.is_none() {
                                            w.opus_bps = Some(192_000);
                                        }
                                    } else {
                                        w.backend =
                                            if w.last_video_backend == VideoBackend::CopyVideo {
                                                VideoBackend::Auto
                                            } else {
                                                w.last_video_backend
                                            };
                                    }
                                },
                            }
                            "Audio only (video copied)"
                        }
                    }
                    div { class: "flex items-center gap-3",
                        div { class: "w-28 shrink-0",
                            Label {
                            if backend == VideoBackend::CopyVideo {
                                "Quality (N/A)"
                            } else if backend.cq_is_av1_crf() {
                                "AV1 CRF"
                            } else if backend == VideoBackend::CpuX264 {
                                "x264 CRF"
                            } else if backend == VideoBackend::CpuX265 {
                                "x265 CRF"
                            } else if matches!(
                                backend,
                                VideoBackend::Av1Nvenc
                                    | VideoBackend::QsvAv1
                                    | VideoBackend::AmfAv1
                            ) {
                                "AV1 CQ"
                            } else if matches!(
                                backend,
                                VideoBackend::VtHevc | VideoBackend::QsvHevc | VideoBackend::AmfHevc
                            ) {
                                "GPU CQ"
                            } else {
                                "NVENC CQ"
                            }
                            }
                        }
                        div { class: "max-w-md flex-1",
                            Slider {
                                min: 18.0,
                                max: 40.0,
                                step: 1.0,
                                value: cq as f64,
                                disabled: preserve_quality,
                                oninput: move |e: FormEvent| {
                                    if let Ok(v) = e.value().parse::<f64>() {
                                        state.write().cq = v.clamp(18.0, 40.0) as u32;
                                    }
                                },
                            }
                        }
                        div { class: "flex w-12 shrink-0 justify-end",
                            Badge { "{cq_label}" }
                        }
                    }
                    div { class: "flex flex-col gap-2",
                        label { class: "flex items-center gap-2 text-sm",
                            Checkbox {
                                checked: keep_subs,
                                disabled: preserve_quality,
                                on_checked_change: move |v| state.write().keep_subs = v,
                            }
                            "Keep subtitles"
                        }
                        label { class: "flex items-center gap-2 text-sm",
                            Checkbox {
                                checked: all_audio,
                                disabled: preserve_quality,
                                on_checked_change: move |v| state.write().all_audio = v,
                            }
                            "Keep all audio tracks (dual-audio)"
                        }
                        // SVT-only tool: hidden entirely when the build
                        // lacks the encoder, like every other capability-
                        // gated option.
                        if svt_av1 {
                            label { class: "flex items-center gap-2 text-sm",
                                Checkbox {
                                    checked: film_grain,
                                    disabled: preserve_quality,
                                    on_checked_change: move |v| state.write().film_grain = v,
                                }
                                "AV1 film-grain synthesis (grainy sources)"
                            }
                        }
                    }
                    div { class: "flex flex-col gap-1",
                        Label { "Extra ffmpeg flags (advanced)" }
                        input {
                            class: "w-full rounded-md border border-input bg-background px-2 py-1.5 font-mono text-xs",
                            r#type: "text",
                            placeholder: "-tune grain -maxrate 8M",
                            disabled: preserve_quality,
                            value: "{extra_args}",
                            oninput: move |e: FormEvent| {
                                state.write().extra_args = e.value();
                            },
                        }
                        p { class: "text-xs text-muted-foreground",
                            if preserve_quality {
                                "Extra flags are inactive while Preserve quality is on."
                            } else {
                                "Appended as output options (can override quality flags). Estimates ignore them. Blocked: -i -map -ss -t -f -y -c:s and other structural flags."
                            }
                        }
                    }
                    if has_image {
                        div { class: "flex flex-col gap-3",
                            p { class: "text-xs font-semibold text-muted-foreground", "Images" }
                            div { class: "flex items-center gap-3",
                                div { class: "w-28 shrink-0",
                                    Label { "Image quality" }
                                }
                                div { class: "max-w-md flex-1",
                                    // Stored value is a CQ (higher = lower
                                    // quality), so the slider renders it
                                    // mirrored: left = low quality, right = high.
                                    Slider {
                                        min: 18.0,
                                        max: 40.0,
                                        step: 1.0,
                                        value: 58.0 - image_cq as f64,
                                        oninput: move |e: FormEvent| {
                                            if let Ok(v) = e.value().parse::<f64>() {
                                                state.write().image_cq = (58.0 - v).clamp(18.0, 40.0) as u32;
                                            }
                                        },
                                    }
                                }
                                div { class: "flex w-12 shrink-0 justify-end",
                                    Badge { "{image_cq_label}" }
                                }
                            }
                            div { class: "flex items-center gap-3",
                                div { class: "w-28 shrink-0",
                                    Label { "Image resolution" }
                                }
                                div { class: "flex-1",
                                    ScalePicker {
                                        value: image_scale,
                                        on_change: move |v| state.write().image_scale = v,
                                    }
                                }
                            }
                            label { class: "flex items-center gap-2 text-sm",
                                Checkbox {
                                    checked: image_preserve_format,
                                    on_checked_change: move |v| {
                                        state.write().image_preserve_format = v;
                                    },
                                }
                                "Preserve image formats (png stays png, tiff stays tiff)"
                            }
                            Collapsible { title: "How image quality works",
                                "{image_quality_note} PNG/BMP/TIFF convert to JPEG when smaller; transparency converts to WebP with the alpha channel kept; animations skip; dead-end JPEGs get one measured WebP fallback. With Preserve image formats checked, PNGs stay PNG (constant opaque alpha planes dropped, near-lossless 256-color palettes, lossless re-encode) and TIFFs stay TIFF via deflate — files that cannot win as their own format skip."
                            }
                        }
                    }
                    div { class: "flex flex-col gap-3",
                        p { class: "text-xs font-semibold text-muted-foreground", "General" }
                        div { class: "flex flex-col gap-2",
                            label { class: "flex items-center gap-2 text-sm",
                                Checkbox {
                                    checked: skip_efficient,
                                    disabled: audio_only,
                                    on_checked_change: move |v| state.write().skip_efficient = v,
                                }
                                "Skip already-efficient files"
                            }
                            label { class: "flex items-center gap-2 text-sm",
                                Checkbox {
                                    checked: replace,
                                    on_checked_change: move |v| state.write().replace = v,
                                }
                                "Replace originals after verified encode"
                            }
                        }
                        div { class: "flex items-center gap-3",
                            div { class: "w-28 shrink-0",
                                Label { "Min. saving %" }
                            }
                            div { class: "max-w-md flex-1",
                                Slider {
                                    min: 0.0,
                                    max: 30.0,
                                    step: 1.0,
                                    value: min_saving,
                                    disabled: audio_only || !skip_efficient,
                                    oninput: move |e: FormEvent| {
                                        if let Ok(v) = e.value().parse::<f64>() {
                                            state.write().min_saving_pct = v.clamp(0.0, 30.0);
                                        }
                                    },
                                }
                            }
                            div { class: "flex w-12 shrink-0 justify-end",
                                Badge { "{saving_label}" }
                            }
                        }
                        div { class: "flex items-center gap-3",
                            div { class: "w-28 shrink-0",
                                Label { "Parallel jobs" }
                            }
                            div { class: "max-w-md flex-1",
                                Slider {
                                    min: 1.0,
                                    max: 4.0,
                                    step: 1.0,
                                    value: parallel as f64,
                                    oninput: move |e: FormEvent| {
                                        if let Ok(v) = e.value().parse::<f64>() {
                                            state.write().parallel = v.clamp(1.0, 4.0) as usize;
                                        }
                                    },
                                }
                            }
                            div { class: "flex w-12 shrink-0 justify-end",
                                Badge { "{parallel_label}" }
                            }
                        }
                    }
                    if !executing {
                        div { class: "flex flex-col gap-2",
                            Button {
                                variant: ButtonVariant::Default,
                                class: "w-full",
                                disabled: scan_busy
                                    || queue_restoring
                                    || pending_waiting
                                    || optimize_busy,
                                title: if scan_busy { Some("Waiting for the scan to finish".to_string()) } else { None },
                                onclick: move |_| crate::process_clicked(state, pump_tx(&state)),
                                "{process_label}"
                            }
                            if n_retry > 0 {
                                Button {
                                    variant: ButtonVariant::Warning,
                                    class: "w-full",
                                    disabled: pending_waiting || optimize_busy,
                                    onclick: move |_| crate::retry_clicked(state, pump_tx(&state)),
                                    "Retry {n_retry} failed {retry_file_word}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Wall-time estimate formatted for a segment line: seconds under 90 s,
/// minutes under 90 min, hours beyond.
fn fmt_secs(secs: f64) -> String {
    if secs < 90.0 {
        format!("{secs:.0}s")
    } else if secs < 5400.0 {
        format!("{:.0} min", secs / 60.0)
    } else {
        format!("{:.1} h", secs / 3600.0)
    }
}

/// Signed percentage for a source → estimated-output pair: `-24%` for a
/// shrink, `+12%` for growth. `<1%` when rounding would hide a real
/// change; exact equality says `0%`.
fn fmt_size_change(src: u64, out: u64) -> String {
    if src == out {
        return "0%".to_string();
    }
    let pct = (out as f64 - src as f64) / src.max(1) as f64 * 100.0;
    if pct.abs() < 0.5 {
        "<1%".to_string()
    } else {
        format!("{pct:+.0}%")
    }
}

/// Batch totals in MB, or KB while under 1 MB.
fn fmt_mb_kb(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let f = bytes as f64;
    if f >= MB {
        format!("{:.1} MB", f / MB)
    } else {
        format!("{:.0} KB", f / 1024.0)
    }
}

/// The Goal control: one speed↔size axis rendered as a segmented control —
/// position on the axis *is* the trade-off, and the caption labels the two
/// ends. With videos loaded, each segment carries the batch's own numbers
/// (shrink % and estimated wall time) instead of adjectives, so the
/// trade-off is read off real files rather than an explainer. Selection is
/// derived by matching the current settings against each goal's resolved
/// triple, so hand-tuning any knob drops the highlight instead of fighting
/// the user.
/// One Goal segment's second line, split so the leading percent can
/// carry its own color (green shrink / orange growth) while the rest of
/// the line keeps the segment's muted tone.
struct GoalSub {
    /// The percent (or the "no gain…" phrase when nothing shrinks).
    main: String,
    /// Tailwind color classes for [`Self::main`]; empty = inherit the
    /// line's tone (used by "no gain", which isn't a size).
    main_cls: &'static str,
    /// " · ~time · n files (n warned)" tail.
    rest: String,
}

#[component]
fn GoalPicker() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let s = state.read();
    // Goals resolve through hardware capabilities; before background
    // detection lands, segments render unhighlighted with their subtitles
    // (and without estimates) instead of blocking on the trial encodes.
    let caps = s.caps;
    let backend = s.backend;
    let preset = s.nvenc_preset;
    let cq = s.cq;
    // Under Preserve quality the quality value is pinned, so segments match on
    // encoder + preset alone — the axis stays a working speed↔size
    // control instead of never highlighting.
    let preserve_quality = s.preserve_quality;
    let segs: Vec<(Goal, bool, Option<GoalSub>)> = Goal::ALL
        .iter()
        .map(|&goal| {
            let active = match caps {
                Some(c) if preserve_quality => goal.matches_preserve_quality(backend, preset, c),
                Some(c) => goal.matches(backend, preset, cq, c),
                None => false,
            };
            let est = s.goal_estimate(goal);
            let counted = est.files + est.warned;
            let files_note = if counted == 1 { "file" } else { "files" };
            // When nothing shrinks, "no gain" already says where every
            // file went — the warned count would only duplicate it.
            let warn_note = if est.files > 0 && est.warned > 0 {
                format!(" ({} warned)", est.warned)
            } else {
                String::new()
            };
            let line = if counted == 0 {
                // Nothing probed yet: the subtitle says what the goal is
                // for; no probed videos would make a fake zero misleading.
                None
            } else {
                Some(match est.saving_frac() {
                    Some(frac) => {
                        let pct = -frac * 100.0;
                        // Warn files stay out of the shrink math (they
                        // can only grow and get discarded), so a plain
                        // sign flip is enough — but never clamp: if the
                        // shrinking subset ever estimates a gain, say +
                        // honestly.
                        let (text, cls) = if pct >= 0.0 {
                            (format!("{pct:.0}%"), "text-green-500")
                        } else {
                            (format!("+{:.0}%", -pct), "text-orange-300")
                        };
                        GoalSub {
                            main: text,
                            main_cls: cls,
                            rest: format!(
                                " · ~{} · {counted} {files_note}{warn_note}",
                                fmt_secs(est.secs),
                            ),
                        }
                    }
                    None => GoalSub {
                        main: "no gain".to_string(),
                        // Not a size value — inherits the line's tone.
                        main_cls: "",
                        rest: format!(
                            " · ~{} · {counted} {files_note}{warn_note}",
                            fmt_secs(est.secs),
                        ),
                    },
                })
            };
            (goal, active, line)
        })
        .collect();
    drop(s);
    rsx! {
        div { class: "flex flex-col gap-1",
            p { class: "text-xs font-semibold text-muted-foreground", "Goal" }
            // Equal grid tracks, not content-sized flex items: the
            // estimate line changes with every setting, and the control
            // must not breathe in and out as it does.
            div { class: "grid w-full grid-cols-3 divide-x divide-border overflow-hidden rounded-md border border-input",
                for (goal, active, line) in segs {
                    {
                        let base = "flex min-w-0 flex-col items-center gap-0.5 px-3 py-2 text-center text-sm transition-colors";
                        let seg_cls = if active {
                            format!("{base} bg-primary text-primary-foreground")
                        } else {
                            format!("{base} text-muted-foreground hover:bg-accent hover:text-accent-foreground")
                        };
                        let sub_cls = if active {
                            "text-xs text-primary-foreground/75"
                        } else {
                            "text-xs text-muted-foreground/75"
                        };
                        rsx! {
                            button {
                                key: "{goal:?}",
                                class: "{seg_cls}",
                                onclick: move |_| {
                                    let caps = shrinkr_core::hw::caps();
                                    let (b, p, c) = goal.resolve(caps);
                                    let mut w = state.write();
                                    w.backend = b;
                                    w.nvenc_preset = p;
                                    w.cq = c;
                                },
                                span { class: "font-medium", "{goal.label()}" }
                                span { class: "{sub_cls}",
                                    {match line {
                                        Some(g) => rsx! {
                                            span { class: "font-medium {g.main_cls}", "{g.main}" }
                                            "{g.rest}"
                                        },
                                        None => rsx! { "{goal.subtitle()}" },
                                    }}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Resolution policy dropdown shared by the video and image sections.
/// `Custom` reveals W/H inputs: the policy becomes `ScalePolicy::Custom`,
/// a fit-inside box (downscale-only, aspect preserved, never upscaled).
/// Width/height live in local text signals so half-typed numbers ("1" on
/// the way to "1600") never clamp the field under the user's cursor —
/// they commit to the policy only once they parse.
#[component]
fn ScalePicker(
    value: ScalePolicy,
    on_change: EventHandler<ScalePolicy>,
    #[props(optional)] disabled: bool,
) -> Element {
    let is_custom = matches!(value, ScalePolicy::Custom(_, _));
    let mut wtext = use_signal(move || match value {
        ScalePolicy::Custom(w, _) => w.to_string(),
        _ => String::new(),
    });
    let mut htext = use_signal(move || match value {
        ScalePolicy::Custom(_, h) => h.to_string(),
        _ => String::new(),
    });
    // One side of the committed custom box: the parsed field text if it
    // parses, else whatever the policy already holds, else the default.
    let side = |text: String, fallback: u32| -> u32 {
        text.parse::<u32>()
            .map(|v| v.clamp(16, 8192))
            .unwrap_or(fallback.clamp(16, 8192))
    };
    let box_now = move || match value {
        ScalePolicy::Custom(w, h) => (side(wtext(), w), side(htext(), h)),
        _ => (side(wtext(), 1920), side(htext(), 1080)),
    };
    rsx! {
        div { class: "flex flex-col gap-1",
            select {
                class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm disabled:opacity-50 disabled:cursor-not-allowed",
                disabled,
                onchange: move |e: FormEvent| {
                    match e.value().as_str() {
                        "1080p" => on_change.call(ScalePolicy::Force1080p),
                        "720p" => on_change.call(ScalePolicy::Force720p),
                        "480p" => on_change.call(ScalePolicy::Force480p),
                        "custom" => {
                            // Field text survives dropdown toggles, so the
                            // user's last size comes back instead of resetting.
                            let (w, h) = box_now();
                            wtext.set(w.to_string());
                            htext.set(h.to_string());
                            on_change.call(ScalePolicy::Custom(w, h));
                        }
                        _ => on_change.call(ScalePolicy::Preserve),
                    }
                },
                option { value: "preserve", selected: value == ScalePolicy::Preserve, "Preserve" }
                option { value: "1080p", selected: value == ScalePolicy::Force1080p, "Force 1080p" }
                option { value: "720p", selected: value == ScalePolicy::Force720p, "Force 720p" }
                option { value: "480p", selected: value == ScalePolicy::Force480p, "Force 480p" }
                option { value: "custom", selected: is_custom, "Custom (max W × H)" }
            }
            if is_custom {
                div { class: "flex items-center gap-2",
                    input {
                        class: "w-20 rounded-md border border-input bg-background px-2 py-1 text-sm disabled:opacity-50 disabled:cursor-not-allowed",
                        r#type: "number",
                        min: "16",
                        max: "8192",
                        disabled,
                        placeholder: "width",
                        value: "{wtext}",
                        oninput: move |e: FormEvent| {
                            let v = e.value();
                            wtext.set(v.clone());
                            if let Ok(w) = v.parse::<u32>() {
                                let (_, h) = box_now();
                                on_change.call(ScalePolicy::Custom(w.clamp(16, 8192), h));
                            }
                        },
                    }
                    span { class: "text-xs text-muted-foreground", "×" }
                    input {
                        class: "w-20 rounded-md border border-input bg-background px-2 py-1 text-sm disabled:opacity-50 disabled:cursor-not-allowed",
                        r#type: "number",
                        min: "16",
                        max: "8192",
                        disabled,
                        placeholder: "height",
                        value: "{htext}",
                        oninput: move |e: FormEvent| {
                            let v = e.value();
                            htext.set(v.clone());
                            if let Ok(h) = v.parse::<u32>() {
                                let (w, _) = box_now();
                                on_change.call(ScalePolicy::Custom(w, h.clamp(16, 8192)));
                            }
                        },
                    }
                    span { class: "text-xs text-muted-foreground", "px max box — aspect kept, never upscaled" }
                }
            }
        }
    }
}

/// Split a preflight reason into (text, highlighted) word segments: size
/// values ("219.2 MB") and percentages ("−51%)" ) stand out inside the
/// long one-line verdict. Unit words ride along with their number;
/// punctuation stays attached to its token. Highlighted segments keep
/// the row's own color — only the weight changes.
fn est_parts(reason: &str) -> Vec<(String, bool)> {
    const UNITS: [&str; 3] = ["GB", "MB", "KB"];
    let toks: Vec<&str> = reason.split(' ').collect();
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        let unit_next = i + 1 < toks.len() && UNITS.contains(&toks[i + 1]);
        if t.contains('%') {
            out.push((t.to_string(), true));
            i += 1;
        } else if unit_next && t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            // Number + unit are one value ("219.2 MB") — kept together.
            out.push((format!("{t} {}", toks[i + 1]), true));
            i += 2;
        } else if !t.is_empty() {
            out.push((t.to_string(), false));
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

#[component]
fn EstimateCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let s = state.read();
    let elig_idx = s.eligible();
    let elig = elig_idx.len();
    let skipped = s.files.len() - elig;
    let (_, warned, _) = s.plan_counts();
    let threshold = s.threshold();
    let (elig_b, new_b, _delta_b, secs) = s.estimate();
    // A set, not a per-file linear `contains` over the eligible indices —
    // that term was O(library × eligible) on every render.
    let elig_set: HashSet<usize> = elig_idx.iter().copied().collect();
    let skipped_b: u64 = s
        .files
        .iter()
        .enumerate()
        .filter(|(i, _)| !elig_set.contains(i))
        .map(|(_, f)| f.bytes)
        .sum();
    // Signed wording: a growing plan says how much it grows (bytes and
    // percent) instead of showing the same total with a 0% difference.
    let change = size_change_label(elig_b, new_b);
    let headline = format!(
        "{} eligible • {} → {} • {}",
        elig,
        human_bytes(elig_b),
        human_bytes(new_b),
        change
    );
    let warn_note = if warned > 0 {
        format!(" • {warned} below {threshold:.0}% (warned, still queued)")
    } else {
        String::new()
    };
    let subline = if skipped > 0 {
        format!(
            "{} skipped file(s) ({} excluded){warn_note} • About {:.1}h",
            skipped,
            human_bytes(skipped_b),
            secs / 3600.0
        )
    } else {
        format!("About {:.1}h{warn_note}", secs / 3600.0)
    };
    let rows: Vec<(String, String, String, String)> = s
        .preflight_rows()
        .iter()
        .map(|(name, res, verdict)| {
            // Warn rows are part of the run (they count as eligible) but
            // stand out from clean shrinks so the threshold is visible.
            let (mark, cls, reason) = match verdict {
                Preflight::Shrink { reason } => ("✓", "text-xs text-green-500", reason),
                Preflight::Warn { reason } => ("⚠", "text-xs text-orange-300", reason),
                Preflight::Skip { reason } => ("✗", "text-xs text-muted-foreground", reason),
            };
            let (mark, cls) = (mark.to_string(), cls.to_string());
            let label = if res == "?" {
                format!("{name}: ")
            } else {
                format!("{name} · {res}: ")
            };
            (mark, label, reason.clone(), cls)
        })
        .collect();
    let target_text = s.target_text.clone();
    let target_solving = s.target_solving;
    let preserve_quality = s.preserve_quality;
    // Target solving is video-only (images don't take a CRF), so the
    // solve row follows the library like the Benchmark card — the
    // estimates and plan rows above stay for image-only batches.
    let has_video = s.has_videos();
    drop(s);
    rsx! {
        Card {
            CardHeader {
                CardTitle { "Estimate & plan" }
            }
            CardContent {
                p { class: "text-sm font-semibold", "{headline}" }
                p { class: "text-xs text-muted-foreground", "{subline}" }
                if preserve_quality && has_video {
                    p { class: "text-xs text-muted-foreground",
                        "Time estimates exclude the final quality check. Any result that fails it is discarded."
                    }
                }
                div { class: "mt-2 max-h-36 overflow-y-auto rounded-md border border-border p-2",
                    for (mark , prefix , reason , cls) in rows {
                        p { class: "{cls}",
                            "{mark} {prefix}"
                            for (seg , hl) in est_parts(&reason) {
                                if hl {
                                    span { class: "font-semibold", "{seg} " }
                                } else {
                                    "{seg} "
                                }
                            }
                        }
                    }
                }
                if has_video {
                    div { class: "mt-2",
                        p { class: "text-xs text-muted-foreground",
                            if preserve_quality {
                                "Preserve quality takes priority over a target size. Turn it off to fit a specific size."
                            } else {
                                "Want a specific size? Enter it (400MB) or how much smaller (2x) — the quality setting is picked automatically to hit it."
                            }
                        }
                        div { class: "mt-1 flex items-center gap-2",
                            input {
                                class: "w-full rounded-md border border-input bg-background px-2 py-1.5 font-mono text-xs",
                                r#type: "text",
                                placeholder: "400MB or 2x",
                                disabled: preserve_quality,
                                value: "{target_text}",
                                oninput: move |e: FormEvent| {
                                    state.write().target_text = e.value();
                                },
                            }
                            if target_solving {
                                // The solver shares a cancel flag with every
                                // probe encode; this stops it after the
                                // in-flight one.
                                Button {
                                    variant: ButtonVariant::Destructive,
                                    onclick: move |_| crate::jobs::cancel_target_solve(state),
                                    "Cancel fit"
                                }
                            } else {
                                Button {
                                    variant: ButtonVariant::Outline,
                                    disabled: preserve_quality,
                                    onclick: move |_| crate::solve_clicked(state, pump_tx(&state)),
                                    "Fit to size"
                                }
                            }
                        }
                    }
                }
                div { class: "mt-2",
                    Collapsible { title: "How estimates work",
                        "Size, time and plan update live as you change settings. Videos: codec × CQ × preset × resolution × audio. Images: their own quality and resolution knobs above (JPEG/WebP re-encode, PNG/BMP/TIFF → JPEG when smaller, transparency → WebP with alpha kept, measured WebP fallback for dead-end JPEGs; with format preservation on, png/tiff re-encode as themselves losslessly and conversions are off; animated files skip). NVENC CQ ≠ x264 CRF — verify in Benchmark. Extra ffmpeg flags apply to video only."
                    }
                }
            }
        }
    }
}

/// Clear removes every file added to the selection: probed library rows,
/// paths still waiting behind a running scan, and mid-run staging. The
/// running batch and its waiting queue are snapshots of their own, so
/// they keep going, and the settings stay exactly as picked.
fn clear_clicked(mut state: Signal<AppState>) {
    let mut w = state.write();
    let dropped = w.clear_selection();
    let mut line = if dropped == 0 {
        "Selection cleared.".to_string()
    } else {
        format!("Cleared {dropped} file(s) from the selection.")
    };
    if w.executing {
        line.push_str(" The running batch and its queue keep going.");
    }
    w.push_log(line);
}

/// Mid-run manual queue: move the still-queueable library files behind
/// the running batch. Everything preflight can run on joins — including
/// below-threshold files, run with a warning; the no-gain guard keeps
/// their originals safe. The files encode with the running batch's
/// frozen settings.
fn queue_clicked(mut state: Signal<AppState>) {
    let s = state.read();
    // Belt and braces: the button only exists mid-run and is disabled
    // when nothing is addable.
    if !s.executing {
        return;
    }
    let idx = s.queueable();
    if idx.is_empty() {
        return;
    }
    let files: Vec<_> = idx.iter().map(|&i| s.files[i].clone()).collect();
    let Some(cfg) = s.exec_cfg.clone() else {
        return;
    };
    drop(s);
    let mut w = state.write();
    let added = files.len();
    w.pending.push(crate::state::PendingSegment {
        files,
        cfg,
        warn: true,
    });
    w.push_log(format!(
        "Queued {added} more file(s) behind the running batch - they start when it finishes, same settings (below-threshold files warn)."
    ));
    crate::persist::save_queue(&w);
}

/// Mid-run "Add to queue": commit the staged files — folders picked
/// while a batch runs and scanned into the library — behind that batch.
/// The click snapshots the settings: these files encode with exactly
/// what the sidebar holds at this moment (warn policy, so
/// below-threshold files the user deliberately staged run after all;
/// the no-gain guard keeps their originals safe).
fn add_staged_clicked(mut state: Signal<AppState>) {
    let s = state.read();
    // Belt and braces: the button only exists mid-run.
    if !s.executing || s.staged.is_empty() {
        return;
    }
    let extra_args = match shrinkr_core::ffmpeg::parse_extra_args(s.effective_extra_args()) {
        Ok(v) => v,
        Err(e) => {
            drop(s);
            state
                .write()
                .push_log(format!("ERROR: extra ffmpeg flags rejected: {e}"));
            return;
        }
    };
    let cfg = crate::state::ExecCfg {
        params: s.est_params(),
        keep_subs: s.effective_keep_subs(),
        parallel: s.parallel.clamp(1, 8),
        replace: s.replace,
        threshold: s.threshold(),
        threshold_policy: s.threshold_policy(),
        extra_args,
    };
    let files: Vec<_> = s.staged.clone();
    drop(s);
    let mut w = state.write();
    let added = files.len();
    w.pending.push(crate::state::PendingSegment {
        files,
        cfg,
        warn: true,
    });
    w.staged.clear();
    w.push_log(format!(
        "Queued {added} staged file(s) behind the running batch - they start when it finishes, with the picked settings (below-threshold files warn)."
    ));
    crate::persist::save_queue(&w);
}

fn process_clicked(mut state: Signal<AppState>, tx: UnboundedSender<JobMsg>) {
    let s = state.read();
    // One core run at a time: an optimize batch owns the workers until it
    // finishes (the Process button is hidden then, this is the guard).
    if s.optimize_executing || s.optimize_queued.is_some() {
        drop(s);
        state
            .write()
            .push_log("An optimize batch is waiting or running - wait for it to finish.".into());
        return;
    }
    // Adding files auto-scans; a batch must not snapshot a half-probed
    // selection (files still arriving would land in staging afterwards).
    if s.scanning || s.probing {
        drop(s);
        state
            .write()
            .push_log("Scan still running - wait for it to finish, then press Process.".into());
        return;
    }
    // A queue restored from the previous session owns the run until the
    // user resumes or discards it; otherwise Process would run the same
    // files out of the library and double-encode them.
    if !s.pending.is_empty() {
        drop(s);
        state
            .write()
            .push_log("A restored queue is waiting - press Resume queue or Discard first.".into());
        return;
    }
    let idx = s.eligible();
    if idx.is_empty() {
        let files = s.files.clone();
        let muted_set = s.muted_codecs.clone();
        let muted_n = s
            .files
            .iter()
            .filter(|f| s.muted_codecs.contains(&f.vcodec))
            .count();
        let p = s.est_params();
        let t = s.threshold();
        let policy = s.threshold_policy();
        let skip_efficient = s.skip_efficient;
        drop(s);
        let mut w = state.write();
        w.push_log("Process pressed but 0 files eligible — reasons:".into());
        if files.is_empty() {
            w.push_log("  (no probed files — add files first; they scan automatically)".into());
        }
        // Muted-codec files are not part of the run at all: their
        // threshold verdicts would just be noise here.
        for f in files
            .iter()
            .filter(|f| !muted_set.contains(&f.vcodec))
            .take(10)
        {
            if let Preflight::Skip { reason } = preflight_auto_with(f, &p, t, policy) {
                let name = f.path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
                w.push_log(format!("  ✗ {}: {}", name, reason));
            }
        }
        if muted_n > 0 {
            w.push_log(format!(
                "  hint: {muted_n} file(s) are unchecked in the Library (codec muted) — tick the circle back on to include them."
            ));
        }
        let audio_only = p.backend == VideoBackend::CopyVideo && p.opus_bps.is_some();
        if audio_only {
            w.push_log(
                "  hint: audio-only passes skip only files without an audio track — everything else runs."
                    .into(),
            );
        } else if skip_efficient {
            w.push_log(
                "  hint: uncheck 'Skip already-efficient files' to queue low-gain files with a warning, or lower 'Min. saving %'."
                    .into(),
            );
        } else {
            w.push_log(
                "  hint: these files can't be shrunk even with the threshold off (see the reasons above)."
                    .into(),
            );
        }
        return;
    }
    let files: Vec<_> = idx.iter().map(|&i| s.files[i].clone()).collect();
    drop(s);
    start_batch(state, tx, files);
}

/// Re-run this run's failed files with the current settings. Failure is
/// technical (decode, disk, flags), not a threshold verdict, so there's
/// no preflight re-gate — the attempt either runs or fails again with a
/// fresh log line. The failed list restarts at what this retry produces.
fn retry_clicked(mut state: Signal<AppState>, tx: UnboundedSender<JobMsg>) {
    let s = state.read();
    // Belt and braces: the button only exists between runs.
    if s.executing {
        return;
    }
    if !s.pending.is_empty() {
        drop(s);
        state
            .write()
            .push_log("A restored queue is waiting - press Resume queue or Discard first.".into());
        return;
    }
    let files = s.retry_files().to_vec();
    drop(s);
    if files.is_empty() {
        return;
    }
    start_batch(state, tx, files);
}

/// Forget the processed-file ledger. Scans stop skipping files that were
/// handled before; the files themselves are untouched.
fn clear_history_clicked(mut state: Signal<AppState>) {
    crate::persist::clear_history();
    let mut w = state.write();
    w.history_count = 0;
    w.push_log(
        "Cleared processed-file history - the next scan treats every file as a fresh input.".into(),
    );
}

/// Shared run entry for Process and Retry: ffmpeg sanity check, extra
/// flags parsed up front (typos fail in the log, not halfway through a
/// batch), a first-command echo, then the worker hand-off.
fn start_batch(
    mut state: Signal<AppState>,
    tx: UnboundedSender<JobMsg>,
    files: Vec<shrinkr_core::media::MediaFile>,
) {
    let s = state.read();
    let mut cfg = crate::state::ExecCfg {
        params: s.est_params(),
        keep_subs: s.effective_keep_subs(),
        parallel: s.parallel.clamp(1, 8),
        replace: s.replace,
        threshold: s.threshold(),
        threshold_policy: s.threshold_policy(),
        extra_args: Vec::new(),
    };
    // Parse the advanced flags now so typos fail fast with a log line,
    // not halfway through a batch.
    let extra_args = match shrinkr_core::ffmpeg::parse_extra_args(s.effective_extra_args()) {
        Ok(v) => v,
        Err(e) => {
            drop(s);
            state
                .write()
                .push_log(format!("ERROR: extra ffmpeg flags rejected: {e}"));
            return;
        }
    };
    drop(s);
    cfg.extra_args = extra_args;
    let _ = run_batch(state, tx, files, cfg);
}

/// Start one batch from an already-resolved settings snapshot. Process,
/// Retry and Resume all land here; the snapshot is stored as `exec_cfg`
/// so chained segments keep their queue-time settings, and the queue is
/// persisted immediately for crash recovery.
fn run_batch(
    mut state: Signal<AppState>,
    tx: UnboundedSender<JobMsg>,
    files: Vec<shrinkr_core::media::MediaFile>,
    cfg: crate::state::ExecCfg,
) -> bool {
    use shrinkr_core::ffmpeg::{build_args, command_line};
    use shrinkr_core::hw::caps;
    use shrinkr_core::pipeline::{plan_streams_with, select_levels_for_media};
    if shrinkr_core::process::cmd("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        state.write().push_log(
            "ERROR: ffmpeg not found — press Download FFmpeg in the header, or grab a release archive (FFmpeg is bundled)."
                .into(),
        );
        return false;
    }
    let params = cfg.params;
    let keep_subs = cfg.keep_subs;
    let parallel = cfg.parallel;
    let replace = cfg.replace;
    let threshold = cfg.threshold;
    let threshold_policy = cfg.threshold_policy;
    let extra_args = cfg.extra_args.clone();
    let first_cmd = files.first().and_then(|f0| {
        let params = params.for_media(f0);
        if shrinkr_core::images::is_shrinkable_image(&f0.path) {
            // Image jobs echo the image command; target ext comes from the plan.
            shrinkr_core::images::plan_image(f0, params.image_preserve_format)
                .ok()
                .map(|plan| {
                    if plan.encoder == "jpegtran" {
                        "jpegtran --optimize --progressive (lossless)".to_string()
                    } else {
                        command_line(&shrinkr_core::images::build_image_args(
                            f0,
                            &plan,
                            params.image_cq,
                            params.image_scale,
                            std::path::Path::new(&format!("<tmp>.{}", plan.target_ext)),
                        ))
                    }
                })
        } else {
            let levels = select_levels_for_media(params.backend, caps(), f0);
            levels.first().map(|l0| {
                let plan = plan_streams_with(
                    f0,
                    keep_subs,
                    params.opus_bps,
                    params.all_audio,
                    shrinkr_core::pipeline::forces_audio_reencode(params.backend, params.opus_bps),
                );
                let args = build_args(
                    f0,
                    *l0,
                    params.preset,
                    params.cq,
                    params.scale,
                    keep_subs,
                    std::path::Path::new("<tmp>.mkv"),
                    &plan,
                    &extra_args,
                    caps(),
                    if params.film_grain { 8 } else { 0 },
                );
                command_line(&args)
            })
        }
    });
    let cancel = Arc::new(AtomicBool::new(false));
    // A fresh run starts with an empty skip set; queue decisions from the
    // previous run don't leak into this one. Shared with the workers so a
    // mid-run Skip/✕ beats the queue.
    let skips = Arc::new(std::sync::Mutex::new(HashSet::new()));
    {
        let mut w = state.write();
        w.executing = true;
        w.exec_skips = Some(skips.clone());
        w.exec_items = files
            .iter()
            .map(|f| {
                crate::state::ExecItem::new(
                    f.path
                        .file_name()
                        .and_then(|x| x.to_str())
                        .unwrap_or("?")
                        .to_string(),
                    f.path.clone(),
                    f.res_label(),
                )
            })
            .collect();
        // Failure bookkeeping runs off this snapshot; a fresh run (Process
        // or Retry) restarts the failed list — retries re-fail into it.
        w.exec_files = files.clone();
        w.failed_files.clear();
        w.exec_total = files.len();
        w.exec_discarded = 0;
        w.cancel = Some(cancel.clone());
        w.exec_cfg = Some(cfg);
        w.push_log(format!(
            "Executing {} files via {:?}+{} cq{} …",
            files.len(),
            params.backend,
            params.preset.as_str(),
            params.cq
        ));
        if !extra_args.is_empty() {
            w.push_log(format!("extra flags: {}", extra_args.join(" ")));
        }
        if let Some(cmd) = first_cmd {
            w.push_log(format!("cmd[0]: {}", cmd));
        }
    }
    crate::jobs::start_exec(
        tx,
        files,
        params,
        keep_subs,
        parallel,
        replace,
        threshold,
        threshold_policy,
        extra_args,
        cancel,
        skips,
    );
    let s = state.read();
    crate::persist::save_queue(&s);
    true
}

/// Resume the queue restored from `queue.json`: run the first segment
/// with its own settings snapshot; the remaining segments chain after it
/// through the normal `ExecDone` path.
fn resume_queue_clicked(mut state: Signal<AppState>, tx: UnboundedSender<JobMsg>) {
    let seg = {
        let mut w = state.write();
        if w.executing
            || w.optimize_executing
            || w.optimize_queued.is_some()
            || w.queue_restoring
            || w.pending.is_empty()
        {
            return;
        }
        w.pending.remove(0)
    };
    // Mid-run explicit queue additions run below-threshold files with a
    // warning; the persisted `warn` flag is what carries that decision.
    let mut cfg = seg.cfg.clone();
    if seg.warn {
        cfg.threshold_policy = shrinkr_core::pipeline::ThresholdPolicy::Warn;
    }
    if !run_batch(state, tx, seg.files.clone(), cfg) {
        // ffmpeg missing / failed hand-off: leave the queue intact.
        state.write().pending.insert(0, seg);
        let s = state.read();
        crate::persist::save_queue(&s);
    }
}

/// Throw away a restored queue (the files themselves stay untouched).
fn discard_restored(mut state: Signal<AppState>) {
    let mut w = state.write();
    let n = w.pending_len();
    if n == 0 {
        return;
    }
    w.pending.clear();
    crate::persist::save_queue(&w);
    w.push_log(format!("Discarded {n} restored queued file(s)."));
}

/// UI-thread part of Fit to size: parse the target + flags up front, then
/// hand the first eligible file to the solver worker. The solved value is
/// written back into the quality slider (batch runs reuse it).
fn solve_clicked(mut state: Signal<AppState>, tx: UnboundedSender<JobMsg>) {
    use shrinkr_core::target::parse_target;
    let s = state.read();
    if s.preserve_quality {
        drop(s);
        state
            .write()
            .push_log("Turn off Preserve quality to fit a target size.".into());
        return;
    }
    let spec = match parse_target(&s.target_text) {
        Ok(v) => v,
        Err(e) => {
            drop(s);
            state.write().push_log(format!("ERROR: bad target: {e}"));
            return;
        }
    };
    let idx = s.eligible();
    // Target solving drives the video encoder ladder; still images have
    // no CRF to solve (their quality is the same CQ slider).
    let Some(vi) = idx
        .iter()
        .copied()
        .find(|&i| !shrinkr_core::images::is_shrinkable_image(&s.files[i].path))
    else {
        drop(s);
        state.write().push_log(
            "Fit to size is video-only — no eligible video in the plan (images don't take a CRF)."
                .into(),
        );
        return;
    };
    let f = s.files[vi].clone();
    let fname = f
        .path
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("?")
        .to_string();
    let params = s.est_params();
    let keep_subs = s.effective_keep_subs();
    let extra_args = match shrinkr_core::ffmpeg::parse_extra_args(s.effective_extra_args()) {
        Ok(v) => v,
        Err(e) => {
            drop(s);
            state
                .write()
                .push_log(format!("ERROR: extra ffmpeg flags rejected: {e}"));
            return;
        }
    };
    drop(s);
    // The solver used to be uncancellable: a multi-hour SVT solve could
    // only be stopped by killing the process (orphaning ffmpeg children
    // and temp files). The cancel handle is shared with every probe
    // encode, so the Cancel button kills the in-flight one and no further
    // rounds start.
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut w = state.write();
        w.target_solving = true;
        w.target_cancel = Some(cancel.clone());
        w.push_log(format!(
            "Target solve on {fname}: aiming {} with {:?} …",
            spec.describe(),
            params.backend
        ));
    }
    crate::jobs::start_target_solve(
        tx,
        f,
        params.backend,
        params.preset,
        params.cq,
        params.scale,
        keep_subs,
        extra_args,
        params.opus_bps,
        params.all_audio,
        params.film_grain,
        spec,
        cancel,
    );
}

#[component]
fn BenchCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let parent = crate::dialog::DialogParent::capture(&dioxus::desktop::use_window());
    let s = state.read();
    let running = s.bench_running;
    let note = s.bench_note.clone();
    let rows: Vec<[String; 9]> = s
        .bench_rows
        .iter()
        .map(|r| {
            let out = r.out_bytes > 0;
            [
                r.tag.clone(),
                r.encoder.clone(),
                r.preset.clone(),
                if out {
                    human_bytes(r.out_bytes as u64)
                } else {
                    "-".into()
                },
                if out {
                    format!("{:.0}%", r.ratio * 100.0)
                } else {
                    "-".into()
                },
                if out {
                    format!("{:.0}s", r.elapsed_s)
                } else {
                    "-".into()
                },
                if out {
                    format!("{:.0}", r.avg_fps)
                } else {
                    "-".into()
                },
                if out {
                    format!("{:.2}x", r.realtime)
                } else {
                    "-".into()
                },
                if r.gpu_max > 0 {
                    format!("{}/{}%", r.gpu_avg, r.gpu_max)
                } else {
                    "-".into()
                },
            ]
        })
        .collect();
    let has_rows = !rows.is_empty();
    drop(s);
    rsx! {
        Card {
            CardHeader {
                CardTitle { "Benchmark — x264 vs NVENC P3–P6" }
            }
            CardContent {
                div { class: "flex flex-col gap-3",
                    if !running {
                        Button {
                            variant: ButtonVariant::Outline,
                            onclick: move |_| {
                                let tx = pump_tx(&state);
                                let s = state.read();
                                if s.files.is_empty() {
                                    drop(s);
                                    state
                                        .write()
                                        .push_log("Benchmark: add files first — they scan automatically.".into());
                                    return;
                                }
                                // The matrix is the video encoder ladder;
                                // still images stay out of it (same
                                // video-only rule as Solve CRF).
                                let vi = s
                                    .eligible()
                                    .iter()
                                    .copied()
                                    .find(|&i| {
                                        !shrinkr_core::images::is_shrinkable_image(
                                            &s.files[i].path,
                                        )
                                    })
                                    .or_else(|| {
                                        s.files.iter().position(|f| {
                                            !shrinkr_core::images::is_shrinkable_image(&f.path)
                                        })
                                    });
                                let Some(vi) = vi else {
                                    drop(s);
                                    state.write().push_log(
                                        "Benchmark is video-only — no eligible video in the plan (images don't run the x264/NVENC matrix)."
                                            .into(),
                                    );
                                    return;
                                };
                                let m = s.files[vi].clone();
                                let (cq, scale, keep_subs) = (s.cq, s.scale, s.keep_subs);
                                drop(s);
                                let cancel = Arc::new(AtomicBool::new(false));
                                {
                                    let mut w = state.write();
                                    w.bench_running = true;
                                    w.bench_cancel = Some(cancel.clone());
                                    w.bench_rows.clear();
                                    w.bench_note.clear();
                                    w.push_log(format!(
                                        "Benchmark: {} through x264 + NVENC P3–P6 (cq{}) …",
                                        m.path.display(),
                                        cq
                                    ));
                                }
                                crate::jobs::start_bench(tx, m, cq, scale, keep_subs, cancel);
                            },
                            "Run on first eligible file"
                        }
                    } else {
                        div { class: "flex items-center justify-between gap-2",
                            div { class: "flex items-center gap-2 text-sm text-muted-foreground",
                                Spinner {}
                                "Benchmarking… (temp outputs deleted)"
                            }
                            Button {
                                variant: ButtonVariant::Destructive,
                                onclick: move |_| crate::jobs::cancel_bench(state),
                                "Cancel"
                            }
                        }
                    }
                    if !note.is_empty() {
                        p { class: "text-sm font-semibold", "{note}" }
                    }
                    if has_rows {
                        div { class: "overflow-x-auto",
                            table { class: "w-full text-xs",
                                thead {
                                    tr { class: "border-b border-border text-left text-muted-foreground",
                                        th { class: "px-2 py-1", "run" }
                                        th { class: "px-2 py-1", "enc" }
                                        th { class: "px-2 py-1", "preset" }
                                        th { class: "px-2 py-1", "out" }
                                        th { class: "px-2 py-1", "ratio" }
                                        th { class: "px-2 py-1", "time" }
                                        th { class: "px-2 py-1", "fps" }
                                        th { class: "px-2 py-1", "RT" }
                                        th { class: "px-2 py-1", "gpu" }
                                    }
                                }
                                tbody {
                                    for cells in &rows {
                                        tr { class: "border-b border-border",
                                            td { class: "px-2 py-1", "{cells[0]}" }
                                            td { class: "px-2 py-1", "{cells[1]}" }
                                            td { class: "px-2 py-1", "{cells[2]}" }
                                            td { class: "px-2 py-1", "{cells[3]}" }
                                            td { class: "px-2 py-1", "{cells[4]}" }
                                            td { class: "px-2 py-1", "{cells[5]}" }
                                            td { class: "px-2 py-1", "{cells[6]}" }
                                            td { class: "px-2 py-1", "{cells[7]}" }
                                            td { class: "px-2 py-1", "{cells[8]}" }
                                        }
                                    }
                                }
                            }
                        }
                        Button {
                            variant: ButtonVariant::Outline,
                            onclick: move |_| {
                                let rows = state.read().bench_rows.clone();
                                let tx = pump_tx(&state);
                                std::thread::spawn(move || {
                                    let mut dlg = rfd::FileDialog::new();
                                    if let Some(p) = &parent {
                                        dlg = dlg.set_parent(p);
                                    }
                                    if let Some(p) = dlg
                                        .set_file_name("shrinkr-bench.csv")
                                        .save_file()
                                    {
                                        let line = match shrinkr_core::bench::write_csv(&p, &rows) {
                                            Ok(()) => format!(
                                                "benchmark CSV saved to {}",
                                                p.display()
                                            ),
                                            Err(e) => format!("warn: CSV save failed: {e}"),
                                        };
                                        let _ = tx.unbounded_send(JobMsg::BenchLog { line });
                                    }
                                });
                            },
                            "Save benchmark CSV…"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ConvertCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let parent = crate::dialog::DialogParent::capture(&dioxus::desktop::use_window());
    let s = state.read();
    let files = s.convert_files.clone();
    let scanning = s.convert_scanning;
    let executing = s.convert_executing;
    let replace = s.convert_replace;
    let done = s.convert_done;
    let total = files.len();
    let ready = files.iter().filter(|f| f.runnable()).count();
    let convert_caps_ready = s.convert_caps_ready;
    drop(s);
    // One caps read, and only after the warmup thread has populated the
    // core OnceLock — this card mounts hidden on the first render, and a
    // direct read here blocked startup on `soffice --version`.
    let (caps_summary, has_office) = if convert_caps_ready {
        let c = shrinkr_core::convert::caps();
        (c.summary(), c.has_office())
    } else {
        (String::new(), false)
    };
    // Drop-zone highlight.
    let mut drag_over = use_signal(|| false);
    let drag_cls = if drag_over() {
        "border-primary"
    } else {
        "border-border"
    };
    let convert_label = format!("Convert {ready} file(s)");
    rsx! {
        Card {
            CardHeader {
                CardTitle { "Convert — change file type" }
            }
            CardContent {
                div { class: "flex flex-col gap-3",
                    div {
                        class: "rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground {drag_cls}",
                        ondragenter: move |_| drag_over.set(true),
                        ondragleave: move |_| drag_over.set(false),
                        ondragover: move |evt| {
                            evt.prevent_default();
                            drag_over.set(true);
                        },
                        ondrop: move |evt| {
                            evt.prevent_default();
                            drag_over.set(false);
                            let paths = dropped_paths(&evt);
                            if !paths.is_empty() {
                                let tx = pump_tx(&state);
                                crate::jobs::start_convert_scan(tx, paths);
                            }
                        },
                        "Drag & drop files or folders here, or use the buttons below."
                    }
                    div { class: "flex gap-2",
                        Button {
                            variant: ButtonVariant::Outline,
                            class: "flex-1",
                            disabled: executing,
                            onclick: move |_| {
                                let tx = pump_tx(&state);
                                std::thread::spawn(move || {
                                    let mut dlg = rfd::FileDialog::new();
                                    if let Some(p) = &parent {
                                        dlg = dlg.set_parent(p);
                                    }
                                    if let Some(paths) = dlg
                                        .add_filter(
                                            "media",
                                            shrinkr_core::convert::CONVERT_EXTS,
                                        )
                                        .pick_files()
                                    {
                                        // Probed by the convert scanner; rows
                                        // appear in convert_files only (never
                                        // in the shrink Sources list).
                                        crate::jobs::start_convert_scan(tx, paths);
                                    }
                                });
                            },
                            "Add files…"
                        }
                        Button {
                            variant: ButtonVariant::Ghost,
                            disabled: files.is_empty() || executing,
                            onclick: move |_| {
                                let mut w = state.write();
                                if !w.convert_executing {
                                    w.convert_files.clear();
                                    w.convert_done = 0;
                                }
                            },
                            "Clear"
                        }
                    }
                    if scanning {
                        div { class: "flex items-center gap-2 text-xs text-muted-foreground",
                            Spinner {}
                            "Collecting & probing…"
                        }
                    }
                    if !files.is_empty() {
                        div { class: "flex max-h-64 flex-col gap-2 overflow-y-auto",
                            for (i , f) in files.iter().enumerate() {
                                {
                                    let name = f.name();
                                    let kind = f.kind.label().to_string();
                                    let src = f.src_ext.clone();
                                    let target = f.target_ext.clone();
                                    // Source dimensions once probed (hidden
                                    // for audio/documents, which have none).
                                    let res = f
                                        .media
                                        .as_ref()
                                        .map(|m| m.res_label())
                                        .unwrap_or_default();
                                    let options = f.feasible_options();
                                    let est = f.estimate_text();
                                    let status = f.status_text();
                                    let frac = f.frac;
                                    let unconvertible = options.is_empty();
                                    let is_doc = f.kind
                                        == shrinkr_core::convert::MediaKind::Document;
                                    let is_extract = shrinkr_core::convert::is_audio_extract(
                                        f.kind,
                                        &target,
                                    );
                                    // Split video rows into video + audio-extraction
                                    // groups so `.mp3` on a video reads as
                                    // extraction, not a container change.
                                    let video_opts: Vec<_> = options
                                        .iter()
                                        .filter(|t| {
                                            !shrinkr_core::convert::is_audio_extract(f.kind, t.ext)
                                        })
                                        .collect();
                                    let audio_opts: Vec<_> = options
                                        .iter()
                                        .filter(|t| {
                                            shrinkr_core::convert::is_audio_extract(f.kind, t.ext)
                                        })
                                        .collect();
                                    rsx! {
                                        div { class: "flex flex-col gap-1 rounded-md border border-border p-2",
                                            div { class: "flex items-center gap-2",
                                                span { class: "min-w-0 flex-1 truncate font-mono text-xs", "{name}" }
                                                Badge { variant: BadgeVariant::Secondary, "{kind}" }
                                                span { class: "shrink-0 text-xs text-muted-foreground", ".{src}" }
                                                if !res.is_empty() && res != "?" {
                                                    span { class: "shrink-0 text-xs text-muted-foreground", "{res}" }
                                                }
                                                if is_extract {
                                                    Badge { variant: BadgeVariant::Secondary, "→audio" }
                                                }
                                                if unconvertible {
                                                    if is_doc {
                                                        span { class: "shrink-0 text-xs text-red-500", "needs LibreOffice" }
                                                    } else {
                                                        span { class: "shrink-0 text-xs text-red-500", "no convertible output" }
                                                    }
                                                } else {
                                                    select {
                                                        class: "max-w-56 rounded-md border border-input bg-background px-1 py-0.5 text-xs",
                                                        disabled: executing,
                                                        onchange: move |e: FormEvent| {
                                                            let v = e.value();
                                                            if let Some(item) = state.write().convert_files.get_mut(i) {
                                                                item.target_ext = v;
                                                            }
                                                        },
                                                        if !video_opts.is_empty() && !audio_opts.is_empty() {
                                                            optgroup { label: "Video",
                                                                for t in &video_opts {
                                                                    {
                                                                        let label = format!("{} (. {})", t.label, t.ext);
                                                                        rsx! {
                                                                            option {
                                                                                value: "{t.ext}",
                                                                                selected: t.ext == target,
                                                                                "{label}"
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            optgroup { label: "Extract audio",
                                                                for t in &audio_opts {
                                                                    {
                                                                        let label = format!("{} (. {})", t.label, t.ext);
                                                                        rsx! {
                                                                            option {
                                                                                value: "{t.ext}",
                                                                                selected: t.ext == target,
                                                                                "{label}"
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        } else {
                                                            for t in &options {
                                                                {
                                                                    let label = format!("{} (. {})", t.label, t.ext);
                                                                    rsx! {
                                                                        option {
                                                                            value: "{t.ext}",
                                                                            selected: t.ext == target,
                                                                            "{label}"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                button {
                                                    class: "shrink-0 rounded px-1 text-xs text-muted-foreground hover:bg-accent hover:text-accent-foreground",
                                                    disabled: executing,
                                                    onclick: move |_| {
                                                        let mut w = state.write();
                                                        if !w.convert_executing && i < w.convert_files.len() {
                                                            w.convert_files.remove(i);
                                                        }
                                                    },
                                                    "✕"
                                                }
                                            }
                                            p { class: "text-xs text-muted-foreground", "{est}" }
                                            div { class: "flex items-center gap-2",
                                                Progress { value: frac * 100.0 }
                                                span { class: "shrink-0 text-xs text-muted-foreground", "{status}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        label { class: "flex items-center gap-2 text-sm",
                            Checkbox {
                                checked: replace,
                                on_checked_change: move |v| state.write().convert_replace = v,
                            }
                            "Replace originals after verified convert"
                        }
                        if !executing {
                            Button {
                                variant: ButtonVariant::Default,
                                class: "w-full",
                                disabled: ready == 0,
                                onclick: move |_| convert_clicked(state, pump_tx(&state)),
                                "{convert_label}"
                            }
                        } else {
                            p { class: "text-xs text-muted-foreground",
                                "Converting {done}/{total}…"
                            }
                            Button {
                                variant: ButtonVariant::Destructive,
                                onclick: move |_| crate::jobs::cancel_convert(state),
                                "Cancel"
                            }
                        }
                    } else {
                        p { class: "text-xs text-muted-foreground",
                            "Nothing here yet — drop files above. Estimates show the expected size change per file."
                        }
                    }
                    if convert_caps_ready && !has_office {
                        Callout { variant: CalloutVariant::Info, title: "Documents need LibreOffice",
                            "Word, Excel, PowerPoint and PDF conversion needs LibreOffice installed (soffice on PATH). Media and subtitle conversion works without it."
                        }
                    }
                    Collapsible { title: "What convert supports",
                        "Video→video, video→audio (extract soundtrack), audio→audio, image→image, subtitle→subtitle, Word→Word, spreadsheet→spreadsheet. Only outputs this machine can write are offered. {caps_summary}"
                    }
                }
            }
        }
    }
}

/// UI-thread part of Convert: eligibility gate, then worker handoff.
fn convert_clicked(mut state: Signal<AppState>, tx: UnboundedSender<JobMsg>) {
    let s = state.read();
    let ready: Vec<usize> = s
        .convert_files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.runnable())
        .map(|(i, _)| i)
        .collect();
    if ready.is_empty() {
        drop(s);
        state.write().push_log(
            "Convert pressed but no convertible files — add files and wait for probing.".into(),
        );
        return;
    }
    if shrinkr_core::process::cmd("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        drop(s);
        state.write().push_log(
            "ERROR: ffmpeg not found — press Download FFmpeg in the header, or grab a release archive (FFmpeg is bundled)."
                .into(),
        );
        return;
    }
    let files = s.convert_files.clone();
    let replace = s.convert_replace;
    drop(s);
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut w = state.write();
        w.convert_executing = true;
        w.convert_done = 0;
        w.convert_cancel = Some(cancel.clone());
        for &i in &ready {
            if let Some(f) = w.convert_files.get_mut(i) {
                f.frac = 0.0;
            }
        }
        w.push_log(format!("Converting {} file(s)…", ready.len()));
    }
    crate::jobs::start_convert_exec(tx, files, ready, replace, cancel);
}

#[component]
fn OptimizeCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let s = state.read();
    let executing = s.optimize_executing;
    let paused = s.optimize_paused;
    let replace = s.optimize_replace;
    let done = s.optimize_done;
    let pdf_preset = s.optimize_pdf_preset;
    let image_level = s.optimize_image_level;
    let total = s.optimize_files.len();
    let ready = s.optimize_files.iter().filter(|f| f.runnable()).count();
    // Only the settings that apply to the picked files: a PDF-only batch
    // hides the image effort, an image-only batch hides the PDF preset.
    let has_pdf = s
        .optimize_files
        .iter()
        .any(|f| f.kind == state::OptimizeKind::Pdf);
    let has_image = s
        .optimize_files
        .iter()
        .any(|f| f.kind == state::OptimizeKind::Image);
    let shrink_running = s.executing;
    let queued = s.optimize_queued.as_ref().map(|b| b.indices.len());
    drop(s);
    let run_label = if let Some(n) = queued {
        format!("Optimize {n} file(s) queued")
    } else if shrink_running {
        format!("Queue {ready} file(s)")
    } else {
        format!("Optimize {ready} file(s)")
    };
    let run_disabled = ready == 0 || queued.is_some();
    // The queued batch carries the settings picked when it was queued, so
    // the controls lock until it starts.
    let settings_locked = executing || queued.is_some();
    let setting_cols = if (has_pdf || !has_image) && (has_image || !has_pdf) {
        "md:grid-cols-3"
    } else {
        "md:grid-cols-2"
    };
    rsx! {
        Card {
            CardHeader {
                CardTitle { "Optimize - PDF and lossless images" }
            }
            CardContent {
                div { class: "flex flex-col gap-3",
                    div { class: "grid grid-cols-1 gap-3 {setting_cols}",
                        if has_pdf || !has_image {
                            div { class: "flex flex-col gap-1",
                                Label { "PDF level" }
                                select {
                                    class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm",
                                    disabled: settings_locked,
                                    onchange: move |e: FormEvent| {
                                        if let Some(p) = shrinkr_core::pdf::PdfPreset::parse(&e.value()) {
                                            state.write().optimize_pdf_preset = p;
                                        }
                                    },
                                    option {
                                        value: "lossless",
                                        selected: pdf_preset == shrinkr_core::pdf::PdfPreset::Lossless,
                                        "Lossless"
                                    }
                                    option {
                                        value: "balanced",
                                        selected: pdf_preset == shrinkr_core::pdf::PdfPreset::Balanced,
                                        "Balanced"
                                    }
                                    option {
                                        value: "small",
                                        selected: pdf_preset == shrinkr_core::pdf::PdfPreset::Small,
                                        "Small"
                                    }
                                }
                            }
                        }
                        if has_image || !has_pdf {
                            div { class: "flex flex-col gap-1",
                                Label { "Image effort" }
                                select {
                                    class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm",
                                    disabled: settings_locked,
                                    onchange: move |e: FormEvent| {
                                        if let Some(p) = shrinkr_core::optimize::OptimizeLevel::parse(&e.value()) {
                                            state.write().optimize_image_level = p;
                                        }
                                    },
                                    option {
                                        value: "balanced",
                                        selected: image_level == shrinkr_core::optimize::OptimizeLevel::Balanced,
                                        "Balanced"
                                    }
                                    option {
                                        value: "max",
                                        selected: image_level == shrinkr_core::optimize::OptimizeLevel::Max,
                                        "Maximum"
                                    }
                                }
                            }
                        }
                        div { class: "flex flex-col justify-end gap-1 pb-1.5",
                            label { class: "flex items-center gap-2 text-sm",
                                Checkbox {
                                    checked: replace,
                                    disabled: settings_locked,
                                    on_checked_change: move |v| state.write().optimize_replace = v,
                                }
                                "Replace originals"
                            }
                        }
                    }
                    if total > 0 {
                        if !executing {
                            Button {
                                variant: ButtonVariant::Default,
                                class: "w-full",
                                disabled: run_disabled,
                                onclick: move |_| optimize_clicked(state, pump_tx(&state)),
                                "{run_label}"
                            }
                        } else {
                            p { class: "text-xs text-muted-foreground",
                                if paused {
                                    "Optimizing {done}/{total} · paused"
                                } else {
                                    "Optimizing {done}/{total}."
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Fixed row height of the optimize queue list, in CSS pixels. The
/// windowing math below and the rendered row height share it.
const OPTIMIZE_ROW_H: usize = 36;
/// Rows visible in the queue panel before it scrolls.
const OPTIMIZE_ROWS_VISIBLE: usize = 10;

/// Rows rendered for one scroll position in the optimize queue, plus the
/// spacer heights that keep the scrollbar proportional to the whole list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OptimizeWindow {
    start: usize,
    end: usize,
    pad_top: usize,
    pad_bottom: usize,
}

fn optimize_window(total: usize, top_row: usize) -> OptimizeWindow {
    let first = top_row.min(total.saturating_sub(OPTIMIZE_ROWS_VISIBLE));
    // Overscan keeps the next rows painted before they scroll in.
    let start = first.saturating_sub(2);
    let end = (first + OPTIMIZE_ROWS_VISIBLE + 2).min(total);
    OptimizeWindow {
        start,
        end,
        pad_top: start * OPTIMIZE_ROW_H,
        pad_bottom: (total - end) * OPTIMIZE_ROW_H,
    }
}

/// Queue of files picked for Optimize, between the settings card and the
/// log. Only the rows around the scroll position are in the tree, so a
/// folder scan with tens of thousands of files stays responsive while
/// ten rows fill the panel.
#[component]
fn OptimizeFilesCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let mut top_row = use_signal(|| 0usize);
    // The list element disappears with an emptied queue; a later refill
    // starts at scrollTop 0, so the window has to start there too.
    use_effect(move || {
        if state.read().optimize_files.is_empty() && *top_row.peek() != 0 {
            top_row.set(0);
        }
    });
    let s = state.read();
    let total = s.optimize_files.len();
    if total == 0 {
        return rsx! {};
    }
    let executing = s.optimize_executing;
    // A batch queued behind the shrink run owns the current rows; removing
    // one would invalidate its indices.
    let locked = executing || s.optimize_queued.is_some();
    let win = optimize_window(total, top_row());
    // Two 1px borders shrink the content box; add them so ten full rows
    // fit without showing a sliver of the eleventh.
    let list_height = total.min(OPTIMIZE_ROWS_VISIBLE) * OPTIMIZE_ROW_H + 2;
    let rows: Vec<(usize, state::OptimizeItem)> = (win.start..win.end)
        .map(|i| (i, s.optimize_files[i].clone()))
        .collect();
    drop(s);
    rsx! {
        Card {
            CardHeader {
                div { class: "flex w-full items-center justify-between gap-3",
                    CardTitle { "Selected files" }
                    span { class: "text-xs text-muted-foreground tabular-nums",
                        "{total} file(s)"
                    }
                }
            }
            CardContent {
                div {
                    id: "optimize-files",
                    class: "overflow-y-auto rounded-md border border-border",
                    style: "height: {list_height}px",
                    onscroll: move |_| {
                        spawn(async move {
                            let script = format!(
                                "const el = document.getElementById('optimize-files'); \
                                 if (!el) {{ dioxus.send(0); }} else {{ \
                                 dioxus.send(Math.floor(el.scrollTop / {OPTIMIZE_ROW_H})); }}"
                            );
                            let mut eval = document::eval(&script);
                            if let Ok(row) = eval.recv::<usize>().await {
                                if *top_row.peek() != row {
                                    top_row.set(row);
                                }
                            }
                        });
                    },
                    div { style: "height: {win.pad_top}px" }
                    for (i , f) in rows {
                        div {
                            class: "flex items-center gap-2 overflow-hidden border-b border-border px-2 last:border-b-0",
                            style: "height: {OPTIMIZE_ROW_H}px",
                            span { class: "min-w-0 flex-1 truncate font-mono text-xs", "{f.name()}" }
                            Badge { class: "shrink-0", variant: BadgeVariant::Secondary, "{f.kind_label()}" }
                            span { class: "shrink-0 text-xs text-muted-foreground tabular-nums",
                                "{human_bytes(f.bytes)}"
                            }
                            if f.done && f.output_bytes > 0 {
                                Badge { class: "shrink-0", variant: BadgeVariant::Secondary,
                                    "{fmt_size_change(f.bytes, f.output_bytes)}"
                                }
                            }
                            span {
                                class: if f.failed { "shrink-0 text-xs text-red-500" } else { "shrink-0 text-xs text-muted-foreground" },
                                "{f.status_text()}"
                            }
                            button {
                                class: "shrink-0 rounded px-1 text-xs text-muted-foreground hover:bg-accent hover:text-accent-foreground",
                                disabled: locked,
                                onclick: move |_| {
                                    let mut w = state.write();
                                    if !w.optimize_executing && i < w.optimize_files.len() {
                                        w.optimize_files.remove(i);
                                    }
                                },
                                "×"
                            }
                        }
                    }
                    div { style: "height: {win.pad_bottom}px" }
                }
            }
        }
    }
}

/// Totals for the last finished Optimize batch. Hidden until a run
/// reports per-file outcomes; a new run or Clear removes it.
#[component]
fn OptimizeSummaryCard() -> Element {
    let state = use_context::<Signal<AppState>>();
    let summary = state.read().optimize_summary.clone();
    let Some(summary) = summary else {
        return rsx! {};
    };
    let before = fmt_mb_kb(summary.start_bytes);
    let after = fmt_mb_kb(summary.output_bytes);
    let saved = summary.saved_bytes();
    let saved_label = format!("{} ({:.1}%)", fmt_mb_kb(saved), summary.saved_pct());
    rsx! {
        Card {
            CardHeader {
                CardTitle { "Summary" }
            }
            CardContent {
                div { class: "grid grid-cols-1 gap-3 md:grid-cols-3",
                    div { class: "flex flex-col gap-0.5",
                        span { class: "text-xs text-muted-foreground", "Files" }
                        span { class: "text-sm font-semibold tabular-nums", "{summary.files}" }
                    }
                    div { class: "flex flex-col gap-0.5",
                        span { class: "text-xs text-muted-foreground", "Skipped" }
                        span { class: "text-sm font-semibold tabular-nums", "{summary.skipped}" }
                    }
                    div { class: "flex flex-col gap-0.5",
                        span { class: "text-xs text-muted-foreground", "Errors" }
                        span {
                            class: if summary.errors > 0 {
                                "text-sm font-semibold tabular-nums text-red-500"
                            } else {
                                "text-sm font-semibold tabular-nums"
                            },
                            "{summary.errors}"
                        }
                    }
                    div { class: "flex flex-col gap-0.5",
                        span { class: "text-xs text-muted-foreground", "Before" }
                        span { class: "text-sm font-semibold tabular-nums", "{before}" }
                    }
                    div { class: "flex flex-col gap-0.5",
                        span { class: "text-xs text-muted-foreground", "After" }
                        span { class: "text-sm font-semibold tabular-nums", "{after}" }
                    }
                    div { class: "flex flex-col gap-0.5",
                        span { class: "text-xs text-muted-foreground", "Saved" }
                        span {
                            class: if saved > 0 {
                                "text-sm font-semibold tabular-nums text-green-300"
                            } else {
                                "text-sm font-semibold tabular-nums"
                            },
                            "{saved_label}"
                        }
                    }
                }
            }
        }
    }
}

/// UI-thread part of Optimize: collect runnable rows, snapshot the preset
/// choices, then start the batch or queue it behind a running shrink run.
fn optimize_clicked(mut state: Signal<AppState>, tx: UnboundedSender<JobMsg>) {
    let s = state.read();
    let indices: Vec<usize> = s
        .optimize_files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.runnable())
        .map(|(i, _)| i)
        .collect();
    if indices.is_empty() {
        drop(s);
        state
            .write()
            .push_log("Optimize pressed but no files are runnable.".into());
        return;
    }
    let batch = state::OptimizeBatch {
        indices,
        replace: s.optimize_replace,
        pdf_preset: s.optimize_pdf_preset,
        level: s.optimize_image_level,
    };
    drop(s);
    let mut w = state.write();
    if w.optimize_executing || w.optimize_queued.is_some() {
        return;
    }
    if w.executing {
        // The shrink run owns the workers; this batch starts when its
        // queue drains (ExecDone), with the settings picked here.
        w.optimize_summary = None;
        w.push_log(format!(
            "Queued {} optimize file(s) behind the shrink run.",
            batch.indices.len()
        ));
        w.optimize_queued = Some(batch);
    } else if !crate::jobs::start_optimize_batch(&mut w, tx, batch) {
        w.push_log("warn: optimize batch could not start.".into());
    }
}

#[component]
fn LogCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    // The rendered window is maintained by push_log / refresh_log_visible:
    // borrow it here instead of re-filtering and cloning 200 entries on
    // every progress tick.
    let s = state.read();
    let entries = s.log_visible.clone();
    let filter = s.log_filter;
    drop(s);
    // Whether the view is pinned to the newest line. Flips to false the
    // moment the user scrolls up (via onscroll below); flips back when
    // they return to the bottom or hit "latest".
    let mut at_bottom = use_signal(|| true);
    let stuck = at_bottom();
    // Follow new lines only while pinned — never yank the scroll out from
    // under someone reading history mid-run.
    use_effect(move || {
        let _ = state.read().log.len();
        if *at_bottom.peek() {
            spawn(async move {
                let _ = document::eval(
                    "const el = document.getElementById('log-scroll'); if (el) el.scrollTop = el.scrollHeight;",
                )
                .await;
            });
        }
    });
    rsx! {
        Card {
            CardHeader {
                div { class: "flex flex-wrap items-center gap-1",
                    CardTitle { "Log" }
                    for level in Level::all() {
                        {
                            let on = filter.visible(level);
                            let color = match level {
                                Level::Error => "text-red-500",
                                Level::Warn => "text-orange-300",
                                Level::Success => "text-green-300",
                                Level::Skip => "text-neutral-300",
                                Level::Command => "text-sky-300",
                                Level::Info => "text-muted-foreground",
                            };
                            let cls = if on {
                                format!("rounded-md border px-2 py-1 text-xs font-semibold {color}")
                            } else {
                                "rounded-md border border-transparent px-2 py-1 text-xs font-semibold text-muted-foreground"
                                    .to_string()
                            };
                            rsx! {
                                button {
                                    class: "{cls}",
                                    onclick: move |_| {
                                        state.write().log_filter.toggle(level);
                                        state.write().refresh_log_visible();
                                    },
                                    "{level.badge()}"
                                }
                            }
                        }
                    }
                }
            }
            CardContent {
                if !stuck {
                    div { class: "mb-1",
                        Button {
                            variant: ButtonVariant::Outline,
                            onclick: move |_| {
                                at_bottom.set(true);
                                spawn(async move {
                                    let _ = document::eval(
                                        "const el = document.getElementById('log-scroll'); if (el) el.scrollTop = el.scrollHeight;",
                                    )
                                    .await;
                                });
                            },
                            "↓ Latest"
                        }
                    }
                }
                div {
                    id: "log-scroll",
                    class: "flex max-h-40 flex-col gap-1 overflow-y-auto font-mono",
                    onscroll: move |_| {
                        // Read the live scroll position out of the DOM and
                        // mirror it into the pin signal (write only on flip
                        // so scroll-storms don't spam renders).
                        spawn(async move {
                            let mut eval = document::eval(
                                "const el = document.getElementById('log-scroll'); \
                                 if (!el) { dioxus.send(true); } else { \
                                 dioxus.send((el.scrollHeight - el.scrollTop - el.clientHeight) < 48); }",
                            );
                            if let Ok(near) = eval.recv::<bool>().await {
                                if *at_bottom.peek() != near {
                                    at_bottom.set(near);
                                }
                            }
                        });
                    },
                    for entry in entries {
                        LogLine { entry }
                    }
                }
            }
        }
    }
}

/// One log row: timestamp + severity dot + tag + message, all colored
/// from the entry's pre-classified level.
#[component]
fn LogLine(entry: std::sync::Arc<LogEntry>) -> Element {
    let (dot, tag, text) = match entry.level {
        Level::Error => ("bg-destructive", "text-red-500", "text-red-500"),
        Level::Warn => ("bg-orange-300", "text-orange-300", "text-orange-300"),
        Level::Success => ("bg-green-300", "text-green-300", "text-green-300"),
        Level::Skip => ("bg-neutral-300", "text-neutral-300", "text-neutral-300"),
        Level::Command => ("bg-sky-300", "text-sky-300", "text-sky-300"),
        Level::Info => ("bg-muted", "text-muted-foreground", "text-muted-foreground"),
    };
    rsx! {
        div { class: "flex items-start gap-2",
            span { class: "shrink-0 text-xs text-muted-foreground", "{entry.stamp()}" }
            span { class: "mt-1 h-2 w-2 shrink-0 rounded-full {dot}" }
            span { class: "shrink-0 text-xs font-semibold {tag}", "{entry.level.badge()}" }
            p { class: "text-xs break-all {text}", "{entry.text}" }
        }
    }
}

#[cfg(test)]
mod optimize_window_tests {
    use super::*;

    #[test]
    fn short_queue_renders_every_row_without_spacers() {
        let win = optimize_window(5, 0);
        assert_eq!((win.start, win.end), (0, 5));
        assert_eq!((win.pad_top, win.pad_bottom), (0, 0));
    }

    #[test]
    fn middle_window_keeps_the_total_scroll_height() {
        let total = 1000;
        let win = optimize_window(total, 500);
        assert_eq!((win.start, win.end), (498, 512));
        assert_eq!(
            win.pad_top + (win.end - win.start) * OPTIMIZE_ROW_H + win.pad_bottom,
            total * OPTIMIZE_ROW_H
        );
    }

    #[test]
    fn bottom_window_stops_at_the_last_row() {
        let total = 100;
        let win = optimize_window(total, usize::MAX);
        assert_eq!(win.end, total);
        assert_eq!(win.pad_bottom, 0);
        assert!(win.start < total);
    }
}
