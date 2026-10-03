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

mod components;
mod dialog;
mod jobs;
mod state;

use components::ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Badge, BadgeVariant, Button, ButtonVariant,
    Callout, CalloutVariant, Card, CardContent, CardDescription, CardHeader, CardTitle, Checkbox,
    Collapsible, Empty, EmptyDescription, EmptyTitle, Label, Progress, Slider, Spinner, Tabs,
    TabsContent, TabsList, TabsTrigger,
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
use shrinkr_core::pipeline::{preflight_auto, NvencPreset, Preflight, ScalePolicy, VideoBackend};
use state::AppState;
use std::sync::{atomic::AtomicBool, Arc};

/// 64x64 RGBA dump of assets/icons/icon.png; decoded at compile time to
/// avoid pulling in an image codec just for the window icon.
const APP_ICON_RGBA: &[u8] = include_bytes!("../assets/icons/icon_64.rgba");

fn app_icon() -> dioxus::desktop::tao::window::Icon {
    dioxus::desktop::tao::window::Icon::from_rgba(APP_ICON_RGBA.to_vec(), 64, 64)
        .expect("bundled window icon has valid dimensions")
}

fn main() {
    // NOTE: dioxus-desktop forces always-on-top in DEBUG builds unless told
    // otherwise ("so we can see it while we work"). A shrink tool that pins
    // itself above file dialogs is unusable, so spell out a normal window.
    use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new().with_window(
                WindowBuilder::new()
                    .with_title("Shrinkr")
                    .with_window_icon(Some(app_icon()))
                    .with_inner_size(LogicalSize::new(1280.0, 900.0))
                    .with_min_inner_size(LogicalSize::new(1024.0, 700.0))
                    .with_decorations(true)
                    .with_always_on_top(false),
            ),
        )
        .launch(App);
}

/// Styles are inlined (rather than linked as manganis assets) so the
/// window is styled even without the `dx` asset server. Rebuild
/// `assets/tailwind.css` with the Tailwind CLI after markup changes —
/// the new output is picked up automatically at the next `cargo build`.
const TAILWIND_CSS: &str = include_str!("../assets/tailwind.css");
const SLIDER_CSS: &str = include_str!("../assets/slider.css");

#[component]
fn App() -> Element {
    let mut state = use_signal(AppState::default);
    use_context_provider(|| state);
    let pump = use_coroutine(move |mut rx: UnboundedReceiver<JobMsg>| async move {
        while let Some(m) = rx.next().await {
            jobs::apply_msg(&mut state, m);
        }
    });
    // Stored sender lets job handlers start chained batches on ExecDone.
    state.write().job_tx = Some(pump.tx());
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

fn pump_tx() -> UnboundedSender<JobMsg> {
    use_coroutine_handle::<JobMsg>().tx()
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
    let hw = s.hw_summary.clone();
    let checking = s.update_checking;
    let installing = s.update_installing;
    let available = s.update_available.clone();
    let ready = s.update_ready.clone();
    let busy = s.executing || s.convert_executing;
    drop(s);
    rsx! {
        header { class: "relative flex shrink-0 items-center justify-between border-b border-border bg-card px-5 py-3",
            div {
                h1 { class: "text-lg font-bold tracking-tight", "Shrinkr" }
            }
            div { class: "pointer-events-none absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2",
                if tools_ok {
                    Badge { variant: BadgeVariant::Success, "ffmpeg ready" }
                } else {
                    Badge { variant: BadgeVariant::Destructive, "ffmpeg missing" }
                }
            }
            div { class: "flex items-center gap-2",
                Badge { variant: BadgeVariant::Secondary, "{hw}" }
                if let Some(v) = ready {
                    Badge { variant: BadgeVariant::Success, "v{v} installed" }
                    Button {
                        variant: ButtonVariant::Default,
                        onclick: move |_| {
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

/// UI-thread part of "Check for updates": flag + worker handoff.
fn check_clicked(mut state: Signal<AppState>) {
    state.write().update_checking = true;
    state.write().update_error = None;
    crate::jobs::start_update_check(pump_tx(), env!("CARGO_PKG_VERSION").to_string());
}

/// UI-thread part of "Update": refuse while a batch runs (the running
/// exe is being replaced), otherwise flag + worker handoff.
fn update_clicked(mut state: Signal<AppState>) {
    {
        let s = state.read();
        if s.executing || s.convert_executing {
            drop(s);
            state.write().push_log(
                "Update deferred — a batch is running. Wait for it to finish, then press Update again.".into(),
            );
            return;
        }
    }
    state.write().update_installing = true;
    state.write().update_error = None;
    state.write().push_log("Downloading + installing update…".into());
    crate::jobs::start_update_install(pump_tx(), env!("CARGO_PKG_VERSION").to_string());
}

#[component]
fn Sidebar() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let parent = crate::dialog::DialogParent::capture(&dioxus::desktop::use_window());
    let s = state.read();
    let can_scan = !s.targets.is_empty() && !s.scanning && !s.probing;
    let has_targets = !s.targets.is_empty();
    let targets: Vec<std::path::PathBuf> = s.targets.clone();
    let scanning = s.scanning;
    let probing = s.probing;
    let probe_done = s.probe_done;
    let probe_total = s.probe_total;
    let files_summary = crate::sidebar_summary(&s);
    // Run state — the Shrink button and its batch progress live at the
    // bottom of this sidebar, next to the import controls.
    let executing = s.executing;
    let exec_done = s.exec_done();
    let exec_total = s.exec_total;
    let exec_overall = s.exec_overall();
    let exec_items = s.exec_items.clone();
    let n_eligible = s.eligible().len();
    drop(s);
    let probe_pct = if probe_total > 0 {
        probe_done as f64 / probe_total as f64 * 100.0
    } else {
        0.0
    };
    let probe_label = format!("Probing {probe_done}/{probe_total} …");
    let progress_label = format!(
        "Working {exec_done}/{exec_total} — {:.0}%",
        exec_overall * 100.0
    );
    let shrink_label = format!("Shrink {n_eligible} file(s)");
    // OS drag & drop onto the sidebar queues shrink sources.
    let drop_tx = pump_tx();
    let mut drag_over = use_signal(|| false);
    let drag_cls = if drag_over() { "ring-2 ring-inset ring-primary" } else { "" };
    rsx! {
        aside {
            class: "flex w-80 shrink-0 flex-col gap-4 overflow-y-auto border-r border-border bg-card p-4 {drag_cls}",
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
                    let _ = drop_tx.unbounded_send(JobMsg::TargetsAdded(paths));
                }
            },
            Card {
                CardHeader {
                    CardTitle { "Sources" }
                    CardDescription { "Folders or files, then Scan — or drag & drop them here" }
                }
                CardContent {
                    div { class: "flex flex-col gap-2",
                        Button {
                            variant: ButtonVariant::Outline,
                            class: "w-full",
                            onclick: move |_| {
                                let tx = pump_tx();
                                std::thread::spawn(move || {
                                    let mut dlg = rfd::FileDialog::new();
                                    if let Some(p) = &parent {
                                        dlg = dlg.set_parent(p);
                                    }
                                    if let Some(paths) = dlg.pick_folders() {
                                        let _ = tx.unbounded_send(JobMsg::TargetsAdded(paths));
                                    }
                                });
                            },
                            "Add folders…"
                        }
                        Button {
                            variant: ButtonVariant::Outline,
                            class: "w-full",
                            onclick: move |_| {
                                let tx = pump_tx();
                                std::thread::spawn(move || {
                                    let mut dlg = rfd::FileDialog::new();
                                    if let Some(p) = &parent {
                                        dlg = dlg.set_parent(p);
                                    }
                                    if let Some(paths) = dlg
                                        .add_filter("media", shrinkr_core::media::MEDIA_EXTS)
                                        .add_filter("images", shrinkr_core::images::IMAGE_SHRINK_EXTS)
                                        .pick_files()
                                    {
                                        let _ = tx.unbounded_send(JobMsg::TargetsAdded(paths));
                                    }
                                });
                            },
                            "Add files…"
                        }
                        div { class: "flex gap-2",
                            Button {
                                variant: ButtonVariant::Default,
                                class: "flex-1",
                                disabled: !can_scan,
                                onclick: move |_| {
                                    let tx = pump_tx();
                                    let targets = state.read().targets.clone();
                                    {
                                        let mut w = state.write();
                                        // Mid-run scans append (queued behind
                                        // the batch); only a fresh scan
                                        // replaces the library.
                                        if !w.executing {
                                            w.files.clear();
                                            w.bench_rows.clear();
                                            w.bench_note.clear();
                                        }
                                        // Scan consumes the targets: scanned
                                        // paths leave the Sources list so
                                        // they never need manual removal.
                                        w.targets.clear();
                                        w.scanning = true;
                                        w.push_log(format!(
                                            "Scanning {} selected item(s) …",
                                            targets.len()
                                        ));
                                    }
                                    crate::jobs::start_scan(tx, targets);
                                },
                                "Scan"
                            }
                            Button {
                                variant: ButtonVariant::Ghost,
                                disabled: !has_targets,
                                onclick: move |_| {
                                    let mut w = state.write();
                                    w.targets.clear();
                                    // The running batch owns its snapshot;
                                    // the library view stays intact mid-run.
                                    if !w.executing {
                                        w.files.clear();
                                    }
                                },
                                "Clear"
                            }
                        }
                    }
                    if has_targets {
                        div { class: "mt-3 max-h-32 overflow-y-auto rounded-md border border-border p-2",
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
                                                w.files.clear();
                                            }
                                        },
                                        "✕"
                                    }
                                }
                            }
                        }
                    }
                    if scanning {
                        div { class: "mt-2 flex items-center gap-2 text-xs text-muted-foreground",
                            Spinner {}
                            "Walking subdirectories…"
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
            Card {
                CardHeader {
                    CardTitle { "Library" }
                }
                CardContent {
                    for (codec , n , bytes) in files_summary {
                        div { class: "flex justify-between text-xs",
                            span { "{codec}: {n} files" }
                            span { class: "text-muted-foreground", "{human_bytes(bytes)}" }
                        }
                    }
                }
            }
            // Run control, pinned to the bottom of the sidebar: the
            // Shrink button when idle, batch progress + cancel mid-run.
            div { class: "mt-auto flex flex-col gap-2",
                if !executing {
                    Button {
                        variant: ButtonVariant::Default,
                        class: "w-full",
                        onclick: move |_| crate::shrink_clicked(state, pump_tx()),
                        "{shrink_label}"
                    }
                } else {
                    Progress { value: exec_overall * 100.0 }
                    p { class: "text-xs text-muted-foreground", "{progress_label}" }
                    p { class: "text-xs font-semibold", "Per-file progress ({exec_done}/{exec_total})" }
                    div { class: "flex max-h-40 flex-col gap-2 overflow-y-auto",
                        for item in exec_items {
                            div { class: "flex flex-col gap-1",
                                div { class: "flex items-center gap-2",
                                    span { class: "min-w-0 flex-1 truncate font-mono text-xs", "{item.name}" }
                                    span { class: "shrink-0 text-xs text-muted-foreground", "{item.status_text()}" }
                                }
                                Progress { value: item.frac * 100.0 }
                            }
                        }
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
                    }
                    TabsContent { value: "shrink",
                        div { class: "flex flex-col gap-4",
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
                }
                LogCard {}
            }
        }
    }
}

/// Shrink-tab empty state, doubling as a drop zone for files/folders.
#[component]
fn ShrinkEmpty() -> Element {
    let drop_tx = pump_tx();
    let mut drag_over = use_signal(|| false);
    let drag_cls = if drag_over() { "ring-2 ring-inset ring-primary" } else { "" };
    rsx! {
        Card {
            CardContent {
                div {
                    class: "rounded-md border border-dashed border-border p-6 text-center {drag_cls}",
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
                            let _ = drop_tx.unbounded_send(JobMsg::TargetsAdded(paths));
                        }
                    },
                    Empty {
                        EmptyTitle { "No files yet" }
                        EmptyDescription {
                            "Add folders or files (videos and images) on the left, press Scan — or drag & drop them here."
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
    let backend = s.backend;
    let preset = s.nvenc_preset;
    let scale = s.scale;
    let cq = s.cq;
    let image_cq = s.image_cq;
    let image_scale = s.image_scale;
    let image_preserve_format = s.image_preserve_format;
    let keep_subs = s.keep_subs;
    let all_audio = s.all_audio;
    let skip_efficient = s.skip_efficient;
    let replace = s.replace;
    let min_saving = s.min_saving_pct;
    let parallel = s.parallel;
    let extra_args = s.extra_args.clone();
    let opus_bps = s.opus_bps;
    // Sections follow the library: video knobs only when a video is
    // loaded, image knobs only when an image is — no NVENC/audio rows
    // for a folder of photos.
    let has_video = s.has_videos();
    let has_image = s.has_images();
    drop(s);
    let cq_label = format!("{cq}");
    let image_cq_label = format!("{image_cq}");
    let image_quality_note = format!(
        "Maps to JPEG q{} / WebP q{} — lower = smaller files.",
        jpeg_q_for(image_cq),
        webp_quality_for(image_cq)
    );
    let saving_label = format!("{min_saving:.0}");
    let parallel_label = format!("{parallel}");
    rsx! {
        Card {
            CardHeader {
                CardTitle { "Pipeline" }
                CardDescription { "Quality, resolution and I/O — video and images have separate knobs" }
            }
            CardContent {
                div { class: "flex flex-col gap-4",
                    if has_video {
                        div { class: "flex flex-col gap-3",
                            p { class: "text-xs font-semibold text-muted-foreground", "Video" }
                            div { class: "flex flex-wrap gap-2",
                        Button {
                            variant: ButtonVariant::Outline,
                            onclick: move |_| {
                                let mut w = state.write();
                                w.backend = VideoBackend::Auto;
                                w.nvenc_preset = NvencPreset::P5;
                                w.cq = 28;
                            },
                            "Fast GPU"
                        }
                        Button {
                            variant: ButtonVariant::Outline,
                            onclick: move |_| {
                                let mut w = state.write();
                                w.backend = VideoBackend::HevcNvenc;
                                w.nvenc_preset = NvencPreset::P6;
                                w.cq = 32;
                            },
                            "Smaller GPU"
                        }
                        Button {
                            variant: ButtonVariant::Outline,
                            onclick: move |_| {
                                let mut w = state.write();
                                w.backend = VideoBackend::CpuX265;
                                w.parallel = 1;
                            },
                            "Max CPU shrink"
                        }
                    }
                    div { class: "grid grid-cols-1 gap-3 md:grid-cols-3",
                        div { class: "flex flex-col gap-1",
                            Label { "Backend" }
                            select {
                                class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm",
                                onchange: move |e: FormEvent| {
                                    state.write().backend = match e.value().as_str() {
                                        "hevc" => VideoBackend::HevcNvenc,
                                        "h264" => VideoBackend::H264Nvenc,
                                        "x264" => VideoBackend::CpuX264,
                                        "x265" => VideoBackend::CpuX265,
                                        "av1" => VideoBackend::CpuAv1,
                                        "copy" => VideoBackend::CopyVideo,
                                        _ => VideoBackend::Auto,
                                    };
                                },
                                option { value: "auto", selected: backend == VideoBackend::Auto, "Auto (NVDEC→NVENC→CPU)" }
                                option { value: "hevc", selected: backend == VideoBackend::HevcNvenc, "NVENC HEVC" }
                                option { value: "h264", selected: backend == VideoBackend::H264Nvenc, "NVENC H.264" }
                                option { value: "x264", selected: backend == VideoBackend::CpuX264, "CPU x264 (CRF = slider)" }
                                option { value: "x265", selected: backend == VideoBackend::CpuX265, "CPU x265 slow (CRF = slider)" }
                                option { value: "av1", selected: backend == VideoBackend::CpuAv1, "CPU AV1 SVT (slow)" }
                                option { value: "copy", selected: backend == VideoBackend::CopyVideo, "Copy video (audio/subs only)" }
                            }
                        }
                        div { class: "flex flex-col gap-1",
                            Label { "NVENC preset" }
                            select {
                                class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm",
                                onchange: move |e: FormEvent| {
                                    state.write().nvenc_preset = match e.value().as_str() {
                                        "p3" => NvencPreset::P3,
                                        "p4" => NvencPreset::P4,
                                        "p6" => NvencPreset::P6,
                                        _ => NvencPreset::P5,
                                    };
                                },
                                option { value: "p3", selected: preset == NvencPreset::P3, "P3 (fast)" }
                                option { value: "p4", selected: preset == NvencPreset::P4, "P4 (medium)" }
                                option { value: "p5", selected: preset == NvencPreset::P5, "P5 (slow)" }
                                option { value: "p6", selected: preset == NvencPreset::P6, "P6 (slower)" }
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
                            class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm",
                            onchange: move |e: FormEvent| {
                                state.write().opus_bps = match e.value().as_str() {
                                    "off" => None,
                                    v => v.parse::<u32>().ok().map(|k| k * 1000),
                                };
                            },
                            option { value: "off", selected: opus_bps.is_none(), "Off (keep as-is)" }
                            option { value: "32", selected: opus_bps == Some(32_000), "Opus 32k" }
                            option { value: "48", selected: opus_bps == Some(48_000), "Opus 48k" }
                            option { value: "64", selected: opus_bps == Some(64_000), "Opus 64k" }
                            option { value: "96", selected: opus_bps == Some(96_000), "Opus 96k" }
                            option { value: "128", selected: opus_bps == Some(128_000), "Opus 128k" }
                            option { value: "192", selected: opus_bps == Some(192_000), "Opus 192k" }
                            option { value: "256", selected: opus_bps == Some(256_000), "Opus 256k" }
                        }
                        p { class: "text-xs text-muted-foreground",
                            "Tracks at/below target are copied. Exotic layouts → AAC."
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
                                on_checked_change: move |v| state.write().keep_subs = v,
                            }
                            "Keep subtitles (copied, tiny)"
                        }
                        label { class: "flex items-center gap-2 text-sm",
                            Checkbox {
                                checked: all_audio,
                                on_checked_change: move |v| state.write().all_audio = v,
                            }
                            "Keep all audio tracks (dual-audio)"
                        }
                    }
                    div { class: "flex flex-col gap-1",
                        Label { "Extra ffmpeg flags (advanced)" }
                        input {
                            class: "w-full rounded-md border border-input bg-background px-2 py-1.5 font-mono text-xs",
                            r#type: "text",
                            placeholder: "-tune grain -maxrate 8M",
                            value: "{extra_args}",
                            oninput: move |e: FormEvent| {
                                state.write().extra_args = e.value();
                            },
                        }
                        p { class: "text-xs text-muted-foreground",
                            "Appended as output options (can override quality flags). Estimates ignore them. Blocked: -i -map -ss -t -f -y -c:s and other structural flags."
                        }
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
                    Collapsible { title: "How estimates work",
                        "Size, time and plan update live as you change settings. Videos: codec × CQ × preset × resolution × audio. Images: their own quality and resolution knobs above (JPEG/WebP re-encode, PNG/BMP/TIFF → JPEG when smaller, transparency → WebP with alpha kept, measured WebP fallback for dead-end JPEGs; with format preservation on, png/tiff re-encode as themselves losslessly and conversions are off; animated files skip). NVENC CQ ≠ x264 CRF — verify in Benchmark. Extra ffmpeg flags apply to video only."
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
fn ScalePicker(value: ScalePolicy, on_change: EventHandler<ScalePolicy>) -> Element {
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
                class: "rounded-md border border-input bg-background px-2 py-1.5 text-sm",
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
                        class: "w-20 rounded-md border border-input bg-background px-2 py-1 text-sm",
                        r#type: "number",
                        min: "16",
                        max: "8192",
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
                        class: "w-20 rounded-md border border-input bg-background px-2 py-1 text-sm",
                        r#type: "number",
                        min: "16",
                        max: "8192",
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

#[component]
fn EstimateCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let s = state.read();
    let elig_idx = s.eligible();
    let elig = elig_idx.len();
    let skipped = s.files.len() - elig;
    let (elig_b, new_b, saved_b, secs) = s.estimate();
    let skipped_b: u64 = s
        .files
        .iter()
        .enumerate()
        .filter(|(i, _)| !elig_idx.contains(i))
        .map(|(_, f)| f.bytes)
        .sum();
    let pct = if elig_b > 0 {
        saved_b as f64 / elig_b as f64 * 100.0
    } else {
        0.0
    };
    let headline = format!(
        "{} eligible • {} → {} (free ~{} • {:.0}%)",
        elig,
        human_bytes(elig_b),
        human_bytes(new_b),
        human_bytes(saved_b),
        pct
    );
    let subline = if skipped > 0 {
        format!(
            "{} skipped file(s) ({} excluded) • About {:.1}h — change any Pipeline setting and this updates",
            skipped,
            human_bytes(skipped_b),
            secs / 3600.0
        )
    } else {
        format!(
            "About {:.1}h — change any Pipeline setting and this updates",
            secs / 3600.0
        )
    };
    let rows: Vec<(String, String, String)> = s
        .preflight_rows()
        .into_iter()
        .map(|(name, go, reason)| {
            let mark = if go {
                "✓".to_string()
            } else {
                "✗".to_string()
            };
            let cls = if go {
                "text-xs text-green-500".to_string()
            } else {
                "text-xs text-muted-foreground".to_string()
            };
            (mark, format!("{name}: {reason}"), cls)
        })
        .collect();
    let target_text = s.target_text.clone();
    let target_solving = s.target_solving;
    // Target solving is video-only (images don't take a CRF), so the
    // solve row follows the library like the Benchmark card — the
    // estimates and plan rows above stay for image-only batches.
    let has_video = s.has_videos();
    drop(s);
    rsx! {
        Card {
            CardHeader {
                CardTitle { "Estimate & plan" }
                CardDescription { "Which files Shrink will touch, and why the rest skip" }
            }
            CardContent {
                p { class: "text-sm font-semibold", "{headline}" }
                p { class: "text-xs text-muted-foreground", "{subline}" }
                div { class: "mt-2 max-h-36 overflow-y-auto rounded-md border border-border p-2",
                    for (mark , text , cls) in rows {
                        p { class: "{cls}", "{mark} {text}" }
                    }
                }
                if has_video {
                    div { class: "mt-2 flex items-center gap-2",
                        input {
                            class: "w-full rounded-md border border-input bg-background px-2 py-1.5 font-mono text-xs",
                            r#type: "text",
                            placeholder: "400MB or 2x",
                            value: "{target_text}",
                            oninput: move |e: FormEvent| {
                                state.write().target_text = e.value();
                            },
                        }
                        Button {
                            variant: ButtonVariant::Outline,
                            disabled: target_solving,
                            onclick: move |_| crate::solve_clicked(state, pump_tx()),
                            if target_solving { "Solving…" } else { "Solve CRF" }
                        }
                    }
                    p { class: "text-xs text-muted-foreground",
                        "Solves the quality value for the first eligible file and sets the slider. Batch runs reuse it."
                    }
                }
            }
        }
    }
}

/// UI-thread part of Shrink: eligibility gate with loud skip reasons,
/// then state setup + worker handoff.
fn shrink_clicked(mut state: Signal<AppState>, tx: UnboundedSender<JobMsg>) {
    use shrinkr_core::ffmpeg::{build_args, command_line};
    use shrinkr_core::hw::caps;
    use shrinkr_core::pipeline::{plan_streams, select_levels_for_media};
    let s = state.read();
    let idx = s.eligible();
    if idx.is_empty() {
        let files = s.files.clone();
        let p = s.est_params();
        let t = s.threshold();
        drop(s);
        let mut w = state.write();
        w.push_log("Shrink pressed but 0 files eligible — reasons:".into());
        if files.is_empty() {
            w.push_log("  (no probed files — Scan first)".into());
        }
        for f in files.iter().take(10) {
            if let Preflight::Skip { reason } = preflight_auto(f, &p, t) {
                let name = f.path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
                w.push_log(format!("  ✗ {}: {}", name, reason));
            }
        }
        w.push_log(
            "  hint: uncheck 'Skip already-efficient files' or lower 'Min. saving %'.".into(),
        );
        return;
    }
    if shrinkr_core::process::cmd("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        drop(s);
        state
            .write()
            .push_log("ERROR: ffmpeg not found on PATH.".into());
        return;
    }
    let files: Vec<_> = idx.iter().map(|&i| s.files[i].clone()).collect();
    let params = s.est_params();
    let keep_subs = s.keep_subs;
    let parallel = s.parallel.clamp(1, 8);
    let replace = s.replace;
    let threshold = s.threshold();
    // Parse the advanced flags now so typos fail fast with a log line,
    // not halfway through a batch.
    let extra_args = match shrinkr_core::ffmpeg::parse_extra_args(&s.extra_args) {
        Ok(v) => v,
        Err(e) => {
            drop(s);
            state
                .write()
                .push_log(format!("ERROR: extra ffmpeg flags rejected: {e}"));
            return;
        }
    };
    let first_cmd = files.first().and_then(|f0| {
        if shrinkr_core::images::is_shrinkable_image(&f0.path) {
            // Image jobs echo the image command; target ext comes from the plan.
            shrinkr_core::images::plan_image(f0, params.image_preserve_format)
                .ok()
                .map(|plan| {
                command_line(&shrinkr_core::images::build_image_args(
                    f0,
                    &plan,
                    params.image_cq,
                    params.image_scale,
                    std::path::Path::new(&format!("<tmp>.{}", plan.target_ext)),
                ))
            })
        } else {
            let levels = select_levels_for_media(params.backend, caps(), f0);
            levels.first().map(|l0| {
                let plan = plan_streams(f0, keep_subs, params.opus_bps, params.all_audio);
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
                );
                command_line(&args)
            })
        }
    });
    drop(s);
    let cancel = Arc::new(AtomicBool::new(false));
    let cfg = crate::state::ExecCfg {
        params,
        keep_subs,
        parallel,
        replace,
        threshold,
        extra_args: extra_args.clone(),
    };
    {
        let mut w = state.write();
        w.executing = true;
        w.exec_items = files
            .iter()
            .map(|f| {
                crate::state::ExecItem::new(
                    f.path
                        .file_name()
                        .and_then(|x| x.to_str())
                        .unwrap_or("?")
                        .to_string(),
                )
            })
            .collect();
        w.exec_total = files.len();
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
        tx, files, params, keep_subs, parallel, replace, threshold, extra_args, cancel,
    );
}

/// UI-thread part of Solve CRF: parse the target + flags up front, then
/// hand the first eligible file to the solver worker. The solved value is
/// written back into the quality slider (batch runs reuse it).
fn solve_clicked(mut state: Signal<AppState>, tx: UnboundedSender<JobMsg>) {
    use shrinkr_core::target::parse_target;
    let s = state.read();
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
            "Solve CRF is video-only — no eligible video in the plan (images don't take a CRF)."
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
    let keep_subs = s.keep_subs;
    let extra_args = match shrinkr_core::ffmpeg::parse_extra_args(&s.extra_args) {
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
    {
        let mut w = state.write();
        w.target_solving = true;
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
        spec,
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
                CardDescription { "Same file through 5 configs, then pick the default from data" }
            }
            CardContent {
                div { class: "flex flex-col gap-3",
                    if !running {
                        Button {
                            variant: ButtonVariant::Outline,
                            onclick: move |_| {
                                let tx = pump_tx();
                                let s = state.read();
                                if s.files.is_empty() {
                                    drop(s);
                                    state
                                        .write()
                                        .push_log("Benchmark: scan files first.".into());
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
                                {
                                    let mut w = state.write();
                                    w.bench_running = true;
                                    w.bench_rows.clear();
                                    w.bench_note.clear();
                                    w.push_log(format!(
                                        "Benchmark: {} through x264 + NVENC P3–P6 (cq{}) …",
                                        m.path.display(),
                                        cq
                                    ));
                                }
                                crate::jobs::start_bench(tx, m, cq, scale, keep_subs);
                            },
                            "Run on first eligible file"
                        }
                    } else {
                        div { class: "flex items-center gap-2 text-sm text-muted-foreground",
                            Spinner {}
                            "Benchmarking… (temp outputs deleted)"
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
                                let tx = pump_tx();
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
    drop(s);
    let caps_summary = shrinkr_core::convert::caps().summary();
    let has_office = shrinkr_core::convert::caps().has_office();
    // Drop-zone highlight.
    let mut drag_over = use_signal(|| false);
    let drag_cls = if drag_over() { "border-primary" } else { "border-border" };
    let convert_label = format!("Convert {ready} file(s)");
    rsx! {
        Card {
            CardHeader {
                CardTitle { "Convert — change file type" }
                CardDescription { "Change a file's type — video, audio, image, subtitle or document" }
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
                                let tx = pump_tx();
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
                                let tx = pump_tx();
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
                                onclick: move |_| convert_clicked(state, pump_tx()),
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
                    if !has_office {
                        Callout { variant: CalloutVariant::Info, title: "Documents need LibreOffice",
                            "Word, Excel, PowerPoint and PDF conversion needs LibreOffice installed (soffice on PATH). Media and subtitle conversion works without it."
                        }
                    }
                    Collapsible { title: "What convert supports",
                        "Video→video, video→audio (extract soundtrack), audio→audio, image→image, subtitle→subtitle, Word→Word, spreadsheet→spreadsheet. Only outputs this machine can write are offered. {caps_summary}"
                    }
                    Collapsible { title: "How convert works",
                        "Remuxes (-c copy) when the codecs fit the new container, otherwise re-encodes once with high-quality defaults. Video→audio extracts the soundtrack only (MP3 192k, M4A 128k, Opus 96k, Vorbis q5, FLAC lossless, WAV PCM). Documents go through LibreOffice (one at a time). Size estimates are exact for remuxes, approximate (~) otherwise."
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
        state
            .write()
            .push_log("ERROR: ffmpeg not found on PATH.".into());
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
fn LogCard() -> Element {
    let mut state = use_context::<Signal<AppState>>();
    let entries = state.read().visible_log();
    let filter = state.read().log_filter;
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
fn LogLine(entry: LogEntry) -> Element {
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
