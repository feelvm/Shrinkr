//! Encoding backend abstraction + FFmpeg implementation.
//!
//! ```text
//! Rust
//! ├── media analysis   (media.rs)
//! ├── pipeline select  (pipeline.rs)
//! ├── FFmpeg backend   (this file: FfmpegBackend)
//! └── future direct NVENC/NVDEC backend (implements EncodeBackend)
//! ```
//!
//! Today only [`FfmpegBackend`] exists. A future Video Codec SDK backend
//! just implements [`EncodeBackend`] and plugs into `run_with_fallback`.

use crate::hw::HwCaps;
use crate::media::MediaFile;
use crate::pipeline::{
    effective_scale_target, Level, NvencPreset, ScalePolicy, StreamPlan, VideoBackend,
};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Progress events forwarded to the UI thread.
#[derive(Clone, Debug)]
pub struct Progress {
    pub frac: f64,
    pub fps: f64,
}

/// Final per-file metrics (also the benchmark row payload).
#[derive(Clone, Debug, Default)]
pub struct EncodeResult {
    pub encoder: String,
    pub preset: String,
    pub quality: String,
    pub level: String,
    pub hw_decode: bool,
    pub scaled_to: Option<(u32, u32)>,
    pub audio_mode: String,
    pub output_bytes: u64,
    pub elapsed_s: f64,
    pub avg_fps: f64,
    pub realtime: f64,
    pub gpu_avg: u32,
    pub gpu_max: u32,
    pub ratio: f64,
    /// extra stages note (ffmpeg handles demux→mux internally; wall time
    /// is reported and `-benchmark` stderr tail is kept for drill-down).
    pub ffmpeg_bench_tail: String,
}

/// Minimal backend trait so a future direct-NVENC implementation can slot in.
pub trait EncodeBackend {
    #[allow(clippy::too_many_arguments)]
    fn encode(
        &self,
        media: &MediaFile,
        out_path: &Path,
        backend: VideoBackend,
        preset: NvencPreset,
        cq: u32,
        scale: ScalePolicy,
        keep_subs: bool,
        extra_args: &[String],
        opus_bps: Option<u32>,
        all_audio: bool,
        progress: &dyn Fn(Progress),
        cancel: &Arc<AtomicBool>,
    ) -> Result<EncodeResult, String>;
}

pub struct FfmpegBackend;

impl EncodeBackend for FfmpegBackend {
    fn encode(
        &self,
        media: &MediaFile,
        out_path: &Path,
        backend: VideoBackend,
        preset: NvencPreset,
        cq: u32,
        scale: ScalePolicy,
        keep_subs: bool,
        extra_args: &[String],
        opus_bps: Option<u32>,
        all_audio: bool,
        progress: &dyn Fn(Progress),
        cancel: &Arc<AtomicBool>,
    ) -> Result<EncodeResult, String> {
        run_with_fallback(
            media,
            out_path,
            backend,
            preset,
            cq,
            scale,
            keep_subs,
            extra_args,
            opus_bps,
            all_audio,
            progress,
            cancel,
            crate::hw::caps(),
        )
    }
}

// ── Extra user flags ─────────────────────────────────────────
///
/// The advanced free-text field is appended as output options (after all
/// built-ins, before the output path), so it can tune (`-tune grain`,
/// `-x265-params …`) and even override (`-crf 32`) the pipeline's own
/// quality flags — ffmpeg lets the last occurrence win.
///
/// Structural flags that would break the app's bookkeeping (inputs,
/// stream maps, seeking, progress reporting, the output itself) are
/// rejected by [`parse_extra_args`]. Size/time estimates intentionally
/// ignore extra flags: they describe a different encode than modeled.
///
/// Flags the advanced field may never carry (exact match, `-flag=value`
/// form included). Everything else passes through to ffmpeg verbatim.
pub const BLOCKED_EXTRA_FLAGS: &[&str] = &[
    "-i",
    "-map",
    "-map_metadata",
    "-map_chapters",
    "-ss",
    "-sseof",
    "-t",
    "-to",
    "-frames",
    "-f",
    "-y",
    "-n",
    "-progress",
    "-nostats",
    "-stats",
    "-hide_banner",
    "-benchmark",
    "-hwaccel",
    "-hwaccel_output_format",
    "-extra_hw_frames",
    "-c:s",
    "-dn",
    "-attach",
    "-dump_attachment",
];

/// Split free text the way a shell would: whitespace-separated, with
/// `"double"`, `'single'` quotes and `\` escapes. Returns the arg list.
pub fn split_extra_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut in_token = false;
    for c in s.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            in_token = true;
            continue;
        }
        match (quote, c) {
            (_, '\\') if quote != Some('\'') => escaped = true,
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                in_token = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            _ => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if escaped {
        // Trailing backslash: keep it literally rather than dropping input.
        cur.push('\\');
        in_token = true;
    }
    if in_token {
        out.push(cur);
    }
    out
}

/// Split + validate the advanced free-text field. `Ok(vec![])` for blank
/// input (no-op). Unterminated quotes are tolerated (rest of line is one
/// token) — strictness belongs on dangerous flags, not on typing.
pub fn parse_extra_args(s: &str) -> Result<Vec<String>, String> {
    let args = split_extra_args(s);
    for a in &args {
        let base = a.split('=').next().unwrap_or(a);
        if BLOCKED_EXTRA_FLAGS.contains(&base) {
            return Err(format!(
                "{a} is managed by the app — pick another flag (see hint under the field)"
            ));
        }
    }
    Ok(args)
}

// ── Command builder ──────────────────────────────────────────

/// Exact FFmpeg arguments for one attempt level.
///
/// Zero-copy GPU path (Level::HwHevc / HwH264):
/// `-hwaccel cuda -hwaccel_output_format cuda` keeps NVDEC output resident;
/// `scale_cuda` (when resizing) and `hevc_nvenc` then consume GPU frames
/// with no GPU→CPU→GPU round-trip. CPU fallback levels omit the hwaccel
/// flags so decode happens in system RAM and NVENC uploads once.
pub fn build_args(
    media: &MediaFile,
    level: Level,
    preset: NvencPreset,
    cq: u32,
    scale: ScalePolicy,
    _keep_subs: bool,
    out_path: &Path,
    plan: &StreamPlan,
    extra_args: &[String],
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-progress".into(),
        "pipe:1".into(),
    ];

    // 1) HW decode flags (input options, before -i)
    if level.uses_hw_decode() {
        a.push("-hwaccel".into());
        a.push("cuda".into());
        a.push("-hwaccel_output_format".into());
        a.push("cuda".into());
        a.push("-extra_hw_frames".into());
        a.push("3".into());
    }

    a.push("-i".into());
    a.push(media.path.to_string_lossy().into_owned());

    // 2) GPU-side vs CPU-side scaling (effective target includes the
    //    mandatory even-dimensions fix for odd sources). Copy-video skips
    //    scaling entirely: any filter would force a re-encode.
    let target = if level == Level::CopyVideo {
        None
    } else {
        effective_scale_target(media, scale)
    };
    match (target, level.uses_hw_decode()) {
        (Some((w, h)), true) => {
            a.push("-vf".into());
            a.push(format!("scale_cuda={w}:{h}"));
        }
        (Some((w, h)), false) => {
            a.push("-vf".into());
            a.push(format!("scale={w}:{h}"));
        }
        (None, _) => {}
    }

    // 3) Stream selection: primary video + primary audio (+ all audio
    //    with `all_audio`) + subs?. Secondary video, attachments and
    //    metadata are dropped.
    a.push("-map".into());
    a.push("0:v:0".into());
    if plan.has_audio {
        a.push("-map".into());
        a.push(if plan.all_audio {
            "0:a".into()
        } else {
            "0:a:0".into()
        });
    }
    if plan.copy_subs {
        a.push("-map".into());
        a.push("0:s?".into());
        a.push("-c:s".into());
        a.push("copy".into());
    }
    a.push("-map_metadata".into());
    a.push("-1".into());
    a.push("-dn".into()); // drop data/attachments

    // 4) Video encoder
    match level {
        Level::HwHevc | Level::CpuDecodeHevcNvenc => {
            a.push("-c:v".into());
            a.push("hevc_nvenc".into());
            a.push("-preset".into());
            a.push(preset.as_str().into());
            a.push("-tune".into());
            a.push("hq".into());
            a.push("-rc".into());
            a.push("vbr".into());
            a.push("-cq".into());
            a.push(format!("{cq}"));
            a.push("-b:v".into());
            a.push("0".into());
            // Quality-per-bit: lookahead + spatial/temporal AQ buy ~5-15%
            // smaller files at the same CQ for ~5% speed. Flags exist since
            // ~2017 drivers; the fallback chain catches any failure anyway.
            a.push("-rc-lookahead".into());
            a.push("32".into());
            a.push("-spatial-aq".into());
            a.push("1".into());
            a.push("-temporal-aq".into());
            a.push("1".into());
            a.push("-profile:v".into());
            a.push(
                if media.bit_depth >= 10 {
                    "main10"
                } else {
                    "main"
                }
                .into(),
            );
        }
        Level::HwH264 | Level::CpuDecodeH264Nvenc => {
            a.push("-c:v".into());
            a.push("h264_nvenc".into());
            a.push("-preset".into());
            a.push(preset.as_str().into());
            a.push("-tune".into());
            a.push("hq".into());
            a.push("-rc".into());
            a.push("vbr".into());
            a.push("-cq".into());
            a.push(format!("{cq}"));
            a.push("-b:v".into());
            a.push("0".into());
            a.push("-rc-lookahead".into());
            a.push("32".into());
            a.push("-spatial-aq".into());
            a.push("1".into());
            a.push("-temporal-aq".into());
            a.push("1".into());
            a.push("-profile:v".into());
            a.push("high".into());
        }
        Level::CpuX264 => {
            a.push("-c:v".into());
            a.push("libx264".into());
            a.push("-crf".into());
            a.push(format!("{cq}"));
            a.push("-preset".into());
            a.push("medium".into());
        }
        Level::CpuX265 => {
            // Max-compression path: slow preset squeezes ~15-25% smaller
            // than medium at the same CRF. Slow on purpose — pair with
            // overnight/parallel-1 runs.
            a.push("-c:v".into());
            a.push("libx265".into());
            a.push("-crf".into());
            a.push(format!("{cq}"));
            a.push("-preset".into());
            a.push("slow".into());
        }
        Level::CpuAv1 => {
            // SVT-AV1 software encode (the Ryzen path on RTX 30 boxes —
            // Ampere has no AV1 NVENC). Preset 8 is the sweet spot for
            // overnight jobs (6–7 = slower/better, 10 = ~2x faster); CRF
            // comes from the shared CQ slider (28 ≈ high quality,
            // 32–38 = shrink territory). 10-bit sources stay 10-bit.
            a.push("-c:v".into());
            a.push("libsvtav1".into());
            a.push("-crf".into());
            a.push(format!("{cq}"));
            a.push("-preset".into());
            a.push("8".into());
            if media.bit_depth >= 10 {
                a.push("-pix_fmt".into());
                a.push("yuv420p10le".into());
            }
        }
        Level::CopyVideo => {
            // Audio-only pass: video bytes untouched. No encoder flags at
            // all — any of them would force (or break) an encode.
            a.push("-c:v".into());
            a.push("copy".into());
        }
    }

    // 4b) HDR color passthrough. `-map_metadata -1` below strips container
    //     metadata and NVENC/x264 default to BT.709 tags, so HDR10/HLG
    //     sources would come out washed out. Re-assert the probed tags
    //     (ffprobe tokens match encoder option values 1:1); SDR tags that
    //     already equal the encoder defaults are harmless to repeat.
    //     Skipped for stream-copy (no encode reads them).
    if level != Level::CopyVideo && media.is_hdr() {
        push_color_tag(&mut a, "-colorspace:v", &media.color_space);
        push_color_tag(&mut a, "-color_primaries:v", &media.color_primaries);
        push_color_tag(&mut a, "-color_trc:v", &media.color_transfer);
        push_color_tag(&mut a, "-color_range:v", &media.color_range);
    }

    // 5) Audio: stream-copy when Off or the source is already at/below
    //    target (re-encoding could only grow it); otherwise Opus at the
    //    chosen bitrate, or AAC for layouts Opus rejects. Channels kept.
    //    Decisions recomputed per track (force_aac-aware) so the AAC retry
    //    below can't go stale; with `all_audio` every track gets its own
    //    `-c:a:N`/`-b:a:N` pair.
    if plan.has_audio {
        if plan.all_audio {
            for (i, t) in media.audio.iter().enumerate() {
                let codec = crate::pipeline::audio_track_codec(t, plan.opus_bps, plan.force_aac);
                push_audio_codec(&mut a, Some(i), &codec);
            }
            // NOTE: no `-ac 2` — channel count is preserved.
        } else if plan.audio_copy_eligible && !plan.force_aac {
            a.push("-c:a".into());
            a.push("copy".into());
        } else if let Some(t) = media.primary_audio() {
            let codec = crate::pipeline::audio_track_codec(t, plan.opus_bps, plan.force_aac);
            push_audio_codec(&mut a, None, &codec);
            // NOTE: no `-ac 2` — channel count is preserved.
        }
    }

    // 6) Advanced free-text flags, verbatim, after all built-ins (so they
    //    can tune and override) and before the output path. Already
    //    split + blocklist-checked by `parse_extra_args` at the UI/CLI.
    //    Skipped for stream-copy: video flags are meaningless there and
    //    several (`-crf`, `-preset`, `-tune`) hard-fail the remux.
    if level != Level::CopyVideo {
        a.extend(extra_args.iter().cloned());
    } else if !extra_args.is_empty() {
        log::debug_log("copy-video: extra flags ignored (nothing to encode)");
    }

    a.push(out_path.to_string_lossy().into_owned());
    a
}

/// Push one `-colorspace:v`-style color tag unless the probed value is
/// missing/unknown (an empty push would override the encoder default
/// with garbage).
fn push_color_tag(args: &mut Vec<String>, flag: &str, value: &str) {
    let v = value.trim().to_lowercase();
    if v.is_empty() || v == "unknown" || v == "unspecified" {
        return;
    }
    args.push(flag.into());
    args.push(v);
}

/// Push one audio output's codec options: `-c:a copy`, or a re-encode
/// pair. `idx=None` addresses the single mapped track (`-c:a`/`-b:a`);
/// `Some(i)` addresses track i (`-c:a:i`/`-b:a:i`) for multi-track maps.
fn push_audio_codec(
    args: &mut Vec<String>,
    idx: Option<usize>,
    codec: &crate::pipeline::AudioCodec,
) {
    use crate::pipeline::AudioCodec;
    let (cflag, bflag) = match idx {
        Some(i) => (format!("-c:a:{i}"), format!("-b:a:{i}")),
        None => ("-c:a".into(), "-b:a".into()),
    };
    match codec {
        AudioCodec::Copy => {
            args.push(cflag);
            args.push("copy".into());
        }
        AudioCodec::Opus(bps) => {
            args.push(cflag);
            args.push("libopus".into());
            args.push(bflag);
            args.push(format!("{}k", bps / 1000));
        }
        AudioCodec::Aac(bps) => {
            args.push(cflag);
            args.push("aac".into());
            args.push(bflag);
            args.push(format!("{}k", bps / 1000));
        }
    }
}

/// Human-readable command for logs / `--dry-run`.
pub fn command_line(args: &[String]) -> String {
    let mut s = String::from("ffmpeg");
    for a in args {
        if a.contains(' ') || a.contains('(') {
            s.push_str(&format!(" \"{a}\""));
        } else {
            s.push_str(&format!(" {a}"));
        }
    }
    s
}

// ── Runner with fallback ─────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn run_with_fallback(
    media: &MediaFile,
    out_path: &Path,
    backend: VideoBackend,
    preset: NvencPreset,
    cq: u32,
    scale: ScalePolicy,
    keep_subs: bool,
    extra_args: &[String],
    opus_bps: Option<u32>,
    all_audio: bool,
    progress: &dyn Fn(Progress),
    cancel: &Arc<AtomicBool>,
    caps: &HwCaps,
) -> Result<EncodeResult, String> {
    let levels = crate::pipeline::select_levels_for_media(backend, caps, media);
    let mut plan = crate::pipeline::plan_streams(media, keep_subs, opus_bps, all_audio);
    // Every level's error is kept: the final message shows the whole chain
    // (each attempt truncated), so "the last fallback failed" never hides
    // the earlier, usually more informative, failures.
    let mut chain: Vec<String> = vec![];
    // Subtitle mapping failures (e.g. mov_text→MKV edge cases) would poison
    // every level identically — retry once with subs dropped instead of
    // burning N full encodes on the same fatal mapping error.
    let mut subs_retried = false;
    let mut audio_retried = false;
    for level in levels {
        let args = build_args(
            media, level, preset, cq, scale, keep_subs, out_path, &plan, extra_args,
        );
        log::debug_log(&format!(
            "[{}] {} ({})",
            media
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?"),
            command_line(&args),
            level.encoder_name()
        ));
        match run_once(
            media, &args, level, preset, cq, scale, &plan, progress, cancel,
        ) {
            Ok(r) => return Ok(r),
            Err(e) => {
                let _ = std::fs::remove_file(out_path);
                if cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                if !subs_retried && plan.copy_subs && is_subs_error(&e) {
                    subs_retried = true;
                    plan.copy_subs = false;
                    log::debug_log(&format!(
                        "level {:?} failed on subtitles ({e}); retrying without subs",
                        level
                    ));
                    let args = build_args(
                        media, level, preset, cq, scale, keep_subs, out_path, &plan, extra_args,
                    );
                    match run_once(
                        media, &args, level, preset, cq, scale, &plan, progress, cancel,
                    ) {
                        Ok(r) => return Ok(r),
                        Err(e2) => {
                            let _ = std::fs::remove_file(out_path);
                            if cancel.load(Ordering::Relaxed) {
                                return Err("cancelled".into());
                            }
                            chain.push(format!("{level:?} (no-subs retry): {e} // then: {e2}"));
                            continue;
                        }
                    }
                }
                // An Opus-hostile layout the probe didn't flag (unknown
                // layout assumed standard): same level once more with AAC
                // instead of failing every level on identical audio. The
                // force flag re-decides every track (primary and any
                // secondaries under all_audio).
                if !audio_retried
                    && plan.has_audio
                    && !plan.audio_copy_eligible
                    && is_opus_layout_error(&e)
                {
                    audio_retried = true;
                    plan.force_aac = true;
                    log::debug_log(&format!(
                        "level {:?} failed on audio layout ({e}); retrying with aac",
                        level
                    ));
                    let args = build_args(
                        media, level, preset, cq, scale, keep_subs, out_path, &plan, extra_args,
                    );
                    match run_once(
                        media, &args, level, preset, cq, scale, &plan, progress, cancel,
                    ) {
                        Ok(r) => return Ok(r),
                        Err(e2) => {
                            let _ = std::fs::remove_file(out_path);
                            if cancel.load(Ordering::Relaxed) {
                                return Err("cancelled".into());
                            }
                            chain.push(format!("{level:?} (aac retry): {e} // then: {e2}"));
                            continue;
                        }
                    }
                }
                // Only retry on plausibly-HW-related failures; a corrupt
                // input will fail every level and surfaces after the loop.
                if is_hw_error(&e) {
                    log::debug_log(&format!(
                        "level {:?} failed with HW-ish error, trying next: {e}",
                        level
                    ));
                    chain.push(format!("{level:?}: {e}"));
                    continue;
                }
                // CPU-level or non-HW errors (e.g. bad subs map on a file
                // with no subs — shouldn't happen thanks to `?`, mux
                // failure, disk full): still try the next level once, then
                // give up. The final fallback is CPU x264; if that fails
                // the file is genuinely broken.
                if level != Level::CpuX264 && level != Level::CpuX265 && level != Level::CpuAv1 {
                    log::debug_log(&format!(
                        "level {:?} failed, trying next backend: {e}",
                        level
                    ));
                    chain.push(format!("{level:?}: {e}"));
                    continue;
                }
                chain.push(format!("{level:?}: {e}"));
                break;
            }
        }
    }
    if chain.is_empty() {
        return Err("no backend levels available".into());
    }
    // One entry per attempt, each capped so a verbose ffmpeg can't flood
    // the log panel; newest (usually the most relevant) last.
    let chain = chain
        .iter()
        .map(|e| {
            const CAP: usize = 300;
            if e.len() > CAP {
                format!("{}…", &e[..CAP])
            } else {
                e.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("  ⟶  ");
    Err(format!("all backends failed: {chain}"))
}

/// True when libopus choked on the channel layout (exotic surround the
/// probe didn't flag). Triggers one same-level retry with AAC.
fn is_opus_layout_error(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("mapping family")
        || l.contains("invalid channel layout")
        || (l.contains("libopus") && l.contains("error"))
        || (l.contains("aost#0") && l.contains("error"))
}

fn is_hw_error(e: &str) -> bool {
    let l = e.to_lowercase();
    [
        "cuda",
        "cuvid",
        "nvenc",
        "nvdec",
        "hwaccel",
        "hardware",
        "device",
        "out of memory",
        "insufficient",
        "not supported",
        "no capable",
        "no device",
        "cannot be used with",
        "impossible to convert",
        "scale_cuda",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// True when the failure plausibly comes from subtitle mapping/conversion
/// (worth one subs-dropped retry, not a full fallback chain).
fn is_subs_error(e: &str) -> bool {
    let l = e.to_lowercase();
    [
        "subtitle",
        "mov_text",
        "srt",
        "ass",
        "ssa",
        "0:s",
        "c:s",
        "invalid subtitle",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// Pick the message lines that explain a failure out of a stderr tail.
/// Encoder stat dumps (`i16 v,h,dc…`, `kb/s:…`) always land last and say
/// nothing, so the old "last 6 lines" window showed exactly the wrong
/// lines. Instead: up to 3 lines matching error patterns (in order),
/// then the final line for the exit summary. Capped for the log panel.
fn error_snippet(tail: &str, status: std::process::ExitStatus) -> String {
    const PATTERNS: &[&str] = &[
        "error",
        "failed",
        "invalid",
        "not supported",
        "could not",
        "unable to",
        "no such",
        "denied",
        "no space",
        "disk full",
        "broken",
        "corrupt",
        "truncated",
        "not divisible",
        "incompatible",
    ];
    const STATS: &[&str] = &[
        "kb/s:",
        "i16 ",
        "i8c ",
        "i8 ",
        "p16 ",
        "p8 ",
        "cqp ",
        "coded y,",
        "weighted p-frames",
        "direct mvs",
        "slice decisions",
        "mb i ",
        "mb p ",
        "mb b ",
        "8x8 transform",
        "coded y,uvdc,uvac",
        "ref p frame",
        "ref b frame",
        "ssim",
        "psnr",
    ];
    let is_stat = |l: &str| {
        let t = l.trim_start().to_lowercase();
        STATS.iter().any(|s| t.starts_with(s))
    };
    let mut hits: Vec<&str> = vec![];
    for line in tail.lines() {
        let l = line.to_lowercase();
        if PATTERNS.iter().any(|p| l.contains(p)) && !is_stat(line) {
            if !hits.iter().any(|h| *h == line) {
                hits.push(line);
            }
            if hits.len() >= 3 {
                break;
            }
        }
    }
    let mut parts = hits;
    match tail.lines().last() {
        Some(last) if !last.trim().is_empty() && !parts.iter().any(|h| *h == last) => {
            parts.push(last);
        }
        _ => {}
    }
    if parts.is_empty() {
        return format!("exited with {status} (no stderr captured)");
    }
    const CAP: usize = 600;
    let s = parts.join(" | ");
    if s.len() > CAP {
        format!("{}…", &s[..CAP])
    } else {
        s
    }
}

fn run_once(
    media: &MediaFile,
    args: &[String],
    level: Level,
    preset: NvencPreset,
    cq: u32,
    scale: ScalePolicy,
    plan: &StreamPlan,
    progress: &dyn Fn(Progress),
    cancel: &Arc<AtomicBool>,
) -> Result<EncodeResult, String> {
    let t0 = Instant::now();
    let total_us = (media.duration_s.max(1.0) * 1_000_000.0) as i64;

    // GPU utilisation sampler (best-effort; zeros when no nvidia-smi).
    let sampling = Arc::new(AtomicBool::new(true));
    let sampling2 = sampling.clone();
    let gpu_handle = std::thread::spawn(move || crate::hw::sample_gpu_util(sampling2));

    let mut child = crate::process::cmd("ffmpeg")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn ffmpeg ({}): {}", level.encoder_name(), e))?;

    // Parse `-progress pipe:1` from stdout; keep stderr tail for error +
    // `-benchmark`-style drill-down.
    let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
    let stderr_tail2 = stderr_tail.clone();
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            let r = BufReader::new(err);
            let mut tail: Vec<String> = vec![];
            for line in r.lines().filter_map(|l| l.ok()) {
                tail.push(line);
                if tail.len() > 40 {
                    tail.remove(0);
                }
            }
            if let Ok(mut g) = stderr_tail2.lock() {
                *g = tail.join("\n");
            }
        });
    }

    if let Some(out) = child.stdout.take() {
        let reader = BufReader::new(out);
        for line in reader.lines().filter_map(|l| l.ok()) {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                sampling.store(false, Ordering::Relaxed);
                return Err("cancelled".into());
            }
            let line = line.trim().to_string();
            if let Some(v) = line.strip_prefix("out_time_ms=") {
                let fps_now = 0.0; // instantaneous fps arrives on fps= lines; frac is primary
                if let Ok(us) = v.parse::<i64>() {
                    let frac = if us < 0 {
                        0.0
                    } else {
                        (us as f64 / total_us as f64).clamp(0.0, 1.0)
                    };
                    progress(Progress { frac, fps: fps_now });
                }
            } else if let Some(v) = line.strip_prefix("fps=") {
                if let Ok(f) = v.trim().parse::<f64>() {
                    progress(Progress { frac: -1.0, fps: f });
                }
            }
            if line == "progress=end" {
                break;
            }
        }
    }
    let st = child.wait().map_err(|e| format!("wait ffmpeg: {e}"))?;
    sampling.store(false, Ordering::Relaxed);
    let (gpu_avg, gpu_max, _) = gpu_handle.join().unwrap_or((0, 0, 0));

    if !st.success() {
        let tail = stderr_tail.lock().map(|g| g.clone()).unwrap_or_default();
        return Err(format!(
            "ffmpeg ({}) failed: {}",
            level.encoder_name(),
            error_snippet(&tail, st)
        ));
    }

    let elapsed = t0.elapsed().as_secs_f64().max(0.01);
    let new_len = std::fs::metadata(plan_out_path(args))
        .map(|m| m.len())
        .unwrap_or(0);
    // Guard against failed encodes that exit 0 but leave a stub file.
    // 32 KiB floor: synthetic tiny clips can legitimately encode small,
    // and the codec verify below is the real validity check.
    if new_len < 32 * 1024 {
        return Err(format!(
            "output too small ({} bytes) — likely failed encode",
            new_len
        ));
    }
    // Verify codec via probe. Encodes must come out hevc/h264/av1;
    // stream-copy must come out byte-identical in codec to the source.
    let vf = crate::media::probe_file(&plan_out_path(args))
        .map_err(|e| format!("verify probe: {e:#}"))?;
    if !output_codec_ok(level, &media.vcodec, &vf.vcodec) {
        return Err(format!(
            "unexpected output codec {} (source {})",
            vf.vcodec, media.vcodec
        ));
    }
    // Verify duration: exit-0 truncations (killed mux, full disk with
    // buffered writes) otherwise pass as good files and eat originals.
    // No trim is ever applied, so ≥5% shrinkage means a broken output.
    if media.duration_s > 5.0 && vf.duration_s > 0.0 && vf.duration_s < media.duration_s * 0.95 {
        return Err(format!(
            "output truncated ({:.1}s vs {:.1}s source) — likely failed encode",
            vf.duration_s, media.duration_s
        ));
    }

    let avg_fps = media.total_frames() / elapsed;
    let realtime = media.duration_s.max(0.01) / elapsed;
    let ratio = new_len as f64 / media.bytes.max(1) as f64;
    let bench_tail = stderr_tail.lock().map(|g| g.clone()).unwrap_or_default();
    // FFmpeg may silently fall back to software decode inside a HW-level
    // attempt (exit 0 + "Failed setup for format cuda" on stderr, e.g. for
    // yuv444p inputs whose CUDA path the driver rejects). The output is
    // still correct — NVENC uploads from RAM — but report it honestly so
    // benchmark hwdec flags and logs don't claim a GPU path we didn't get.
    let hw_decode_actual = level.uses_hw_decode() && !stderr_shows_sw_fallback(&bench_tail);
    let quality = match level {
        Level::HwHevc | Level::CpuDecodeHevcNvenc => format!("cq{cq}"),
        Level::HwH264 | Level::CpuDecodeH264Nvenc => format!("cq{cq}"),
        Level::CpuX264 => format!("crf{cq}"),
        Level::CpuX265 => format!("crf{cq}"),
        Level::CpuAv1 => format!("crf{cq}"),
        Level::CopyVideo => "copy".into(),
    };
    let preset_label = match level {
        Level::HwHevc | Level::CpuDecodeHevcNvenc | Level::HwH264 | Level::CpuDecodeH264Nvenc => {
            preset.as_str().into()
        }
        Level::CpuX264 => "medium".into(),
        Level::CpuX265 => "slow".into(),
        Level::CpuAv1 => "svt-p8".into(),
        Level::CopyVideo => "copy".into(),
    };
    Ok(EncodeResult {
        encoder: level.encoder_name().into(),
        preset: preset_label,
        quality,
        level: format!("{:?}", level),
        hw_decode: hw_decode_actual,
        scaled_to: effective_scale_target(media, scale),
        audio_mode: crate::pipeline::audio_mode_label(media, plan),
        output_bytes: new_len,
        elapsed_s: elapsed,
        avg_fps,
        realtime,
        gpu_avg,
        gpu_max,
        ratio,
        ffmpeg_bench_tail: bench_tail,
    })
}

/// Recover the output path (last arg) for size/verify.
fn plan_out_path(args: &[String]) -> PathBuf {
    PathBuf::from(args.last().cloned().unwrap_or_default())
}

/// Move a file robustly: fast `rename` when source and destination share
/// a volume, copy+delete fallback when they don't (e.g. system TEMP on C:
/// vs media library on D: or a NAS — plain `rename` fails there).
fn move_file_robust(src: &Path, dst: &Path) -> Result<(), String> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // Cross-volume move (or rename quirks): copy the bytes, flush,
            // then remove the source. Only remove the source after a
            // verified copy so a full disk can't eat the only good output.
            let copied = std::fs::copy(src, dst).map_err(|e| {
                format!(
                    "move {} → {} failed (rename: {rename_err}; copy: {e})",
                    src.display(),
                    dst.display()
                )
            })?;
            let dst_len = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
            if dst_len != copied {
                let _ = std::fs::remove_file(dst);
                return Err(format!(
                    "move {} → {} failed: copied {copied} bytes but dest has {dst_len}",
                    src.display(),
                    dst.display()
                ));
            }
            std::fs::remove_file(src).map_err(|e| {
                format!(
                    "move {} → {}: copied ok but cleanup of tmp failed: {e}",
                    src.display(),
                    dst.display()
                )
            })?;
            Ok(())
        }
    }
}

/// Pick a destination that doesn't clobber an unrelated existing file:
/// `<stem>.<ext>`, then `<stem>.shrunk.<ext>`, `<stem>.shrunk-1.<ext>`, …
/// The extension comes from `base` itself, so images and videos share
/// the same collision ladder.
fn unique_dest(base: &Path) -> PathBuf {
    if !base.exists() {
        return base.to_path_buf();
    }
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let ext = base
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("out");
    let parent = base.parent();
    let candidate = |name: String| match parent {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    };
    let first = candidate(format!("{stem}.shrunk.{ext}"));
    if !first.exists() {
        return first;
    }
    for i in 1..100u32 {
        let p = candidate(format!("{stem}.shrunk-{i}.{ext}"));
        if !p.exists() {
            return p;
        }
    }
    // Degenerate (100 collisions): fall back to timestamped name.
    candidate(format!(
        "{stem}.shrunk-{}.{ext}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ))
}

/// Move a verified temp output to its final home.
/// * `replace=true`: delete-then-move next to the original (safe on full
///   disks). Same-name (ext→ext) goes through a `.orig.bak` swap.
/// * `replace=false`: keep both; never overwrites — collides resolve to
///   `<stem>.shrunk.<ext>`, `<stem>.shrunk-1.<ext>`, …
///
/// `dest_ext` is the output extension without dot (`"mkv"` for video,
/// `"jpg"` for images) — conversions (avi→mkv, png→jpg) land on the new
/// extension next to the original.
///
/// Cross-volume moves (TEMP vs media drive) fall back to copy+delete.
/// Returns `(bytes saved, final dest path)` — saved is 0 when keeping both.
pub fn place_output(
    orig: &Path,
    tmp_out: &Path,
    replace: bool,
    dest_ext: &str,
) -> Result<(i64, PathBuf), String> {
    let orig_len = std::fs::metadata(orig).map(|m| m.len()).unwrap_or(0) as i64;
    let new_len = std::fs::metadata(tmp_out).map(|m| m.len()).unwrap_or(0) as i64;
    if replace {
        let dest = orig.with_extension(dest_ext);
        if dest == orig {
            let bak = orig.with_extension("orig.bak");
            let _ = std::fs::remove_file(&bak); // stale bak from a crashed run
            std::fs::rename(orig, &bak).map_err(|e| format!("backup original: {e}"))?;
            if let Err(e) = move_file_robust(tmp_out, orig) {
                let _ = std::fs::rename(&bak, orig); // restore on failure
                return Err(format!("move back into place: {e}"));
            }
            let _ = std::fs::remove_file(&bak);
            Ok(((orig_len - new_len).max(0), orig.to_path_buf()))
        } else {
            // Never clobber an unrelated file that already owns `dest`
            // (e.g. movie.mp4 next to a different movie.mkv): pick a fresh
            // name first. Move BEFORE deleting: the original is only
            // removed once the new file is safely in place, so a failed
            // move can never eat it (the old order deleted first).
            let dest = unique_dest(&dest);
            if let Err(e) = move_file_robust(tmp_out, &dest) {
                return Err(format!(
                    "move {} into place: {e} (original kept at {})",
                    dest.display(),
                    orig.display()
                ));
            }
            std::fs::remove_file(orig).map_err(|e| {
                format!(
                    "encoded to {} but could not delete original {}: {e} (delete it by hand)",
                    dest.display(),
                    orig.display()
                )
            })?;
            Ok(((orig_len - new_len).max(0), dest))
        }
    } else {
        let dest = unique_dest(&orig.with_extension(dest_ext));
        move_file_robust(tmp_out, &dest)
            .map_err(|e| format!("move {} into place: {e}", dest.display()))?;
        Ok((0, dest))
    }
}

/// Output-codec check for the verify step: encodes must land on a real
/// video codec, stream-copy must preserve the source codec exactly.
pub fn output_codec_ok(level: Level, src_vcodec: &str, out_vcodec: &str) -> bool {
    if level == Level::CopyVideo {
        return out_vcodec == src_vcodec;
    }
    matches!(out_vcodec, "hevc" | "h264" | "av1")
}

/// True when ffmpeg's stderr shows it fell back to software decode inside
/// an attempt that requested CUDA (exit code stays 0, output stays valid).
fn stderr_shows_sw_fallback(tail: &str) -> bool {
    let l = tail.to_lowercase();
    [
        "failed setup for format cuda",
        "hwaccel initialisation returned error",
        "hwaccel initialization returned error",
        "hardware is lacking required capabilities",
        "falling back to software decode",
        "using software decoding",
    ]
    .iter()
    .any(|k| l.contains(k))
}

// ── Tiny internal logger (file + memory ring not needed) ─────

pub mod log {
    use std::sync::{Mutex, OnceLock};
    static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    pub fn debug_log(s: &str) {
        let m = LINES.get_or_init(|| Mutex::new(vec![]));
        if let Ok(mut g) = m.lock() {
            g.push(s.to_string());
            if g.len() > 200 {
                g.remove(0);
            }
        }
    }
    #[allow(dead_code)]
    pub fn take() -> Vec<String> {
        LINES
            .get()
            .and_then(|m| m.lock().ok().map(|g| g.clone()))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaFile;
    use crate::pipeline::{NvencPreset, StreamPlan};
    use std::fs;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "shrinkr-test-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn robust_move_works_same_volume() {
        let d = tmp_dir("move");
        let src = d.join("a.tmp");
        let dst = d.join("b.mkv");
        fs::write(&src, b"hello").unwrap();
        move_file_robust(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"hello");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn unique_dest_never_clobbers() {
        let d = tmp_dir("unique");
        let base = d.join("movie.mkv");
        assert_eq!(unique_dest(&base), base); // missing → as-is
        fs::write(&base, b"orig").unwrap();
        assert_eq!(unique_dest(&base), d.join("movie.shrunk.mkv"));
        fs::write(d.join("movie.shrunk.mkv"), b"s1").unwrap();
        assert_eq!(unique_dest(&base), d.join("movie.shrunk-1.mkv"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn keep_both_does_not_overwrite_unrelated_mkv() {
        let d = tmp_dir("keepboth");
        // movie.mp4 to shrink + an unrelated movie.mkv that must survive.
        let orig = d.join("movie.mp4");
        let other = d.join("movie.mkv");
        let tmp = d.join("tmp-out.mkv");
        fs::write(&orig, b"orig-mp4").unwrap();
        fs::write(&other, b"unrelated").unwrap();
        fs::write(&tmp, b"shrunk").unwrap();
        let (saved, dest) = place_output(&orig, &tmp, false, "mkv").unwrap();
        assert_eq!(saved, 0);
        assert_eq!(dest, d.join("movie.shrunk.mkv"));
        assert_eq!(fs::read(&other).unwrap(), b"unrelated");
        assert_eq!(fs::read(&dest).unwrap(), b"shrunk");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn replace_same_name_swaps_atomically() {
        let d = tmp_dir("replace");
        let orig = d.join("movie.mkv");
        let tmp = d.join("tmp-out.mkv");
        fs::write(&orig, b"old").unwrap();
        fs::write(&tmp, b"new").unwrap();
        let (saved, dest) = place_output(&orig, &tmp, true, "mkv").unwrap();
        assert_eq!(saved, 0); // new (3B) is not smaller than old (3B)
        assert_eq!(dest, orig);
        assert_eq!(fs::read(&orig).unwrap(), b"new");
        assert!(!d.join("movie.orig.bak").exists());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn replace_new_extension_deletes_original_after_move() {
        // The AVI case: movie.avi → movie.mkv, original must be gone and
        // the content must be the new encode.
        let d = tmp_dir("replace-ext");
        let orig = d.join("movie.avi");
        let tmp = d.join("tmp-out.mkv");
        fs::write(&orig, b"old-avi").unwrap();
        fs::write(&tmp, b"new-mkv-content").unwrap();
        let (saved, dest) = place_output(&orig, &tmp, true, "mkv").unwrap();
        assert_eq!(dest, d.join("movie.mkv"));
        assert!(!orig.exists(), "original must be deleted on success");
        assert!(!tmp.exists(), "tmp must be moved away");
        assert_eq!(fs::read(&dest).unwrap(), b"new-mkv-content");
        assert_eq!(saved, 0); // new content is larger here; saved clamps at 0
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn replace_new_extension_never_clobbers_unrelated_mkv() {
        // movie.avi next to a DIFFERENT movie.mkv: the mkv survives, the
        // encode lands on a fresh name, the avi is still replaced.
        let d = tmp_dir("replace-collide");
        let orig = d.join("movie.avi");
        let other = d.join("movie.mkv");
        let tmp = d.join("tmp-out.mkv");
        fs::write(&orig, b"old-avi").unwrap();
        fs::write(&other, b"unrelated").unwrap();
        fs::write(&tmp, b"new").unwrap();
        let (_, dest) = place_output(&orig, &tmp, true, "mkv").unwrap();
        assert_eq!(dest, d.join("movie.shrunk.mkv"));
        assert_eq!(fs::read(&other).unwrap(), b"unrelated");
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(!orig.exists());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn image_places_land_on_the_image_extension() {
        // png → jpeg flows through the same placer: keep-both writes
        // photo.jpg next to photo.png; replace deletes the png only after
        // the jpg is in place.
        let d = tmp_dir("image-place");
        let orig = d.join("photo.png");
        let tmp = d.join("tmp-out.jpg");
        fs::write(&orig, b"png-bytes").unwrap();
        fs::write(&tmp, b"jpg-bytes").unwrap();
        let (saved, dest) = place_output(&orig, &tmp, false, "jpg").unwrap();
        assert_eq!(saved, 0);
        assert_eq!(dest, d.join("photo.jpg"));
        assert_eq!(fs::read(&dest).unwrap(), b"jpg-bytes");
        assert_eq!(fs::read(&orig).unwrap(), b"png-bytes");
        // Replace: png gone, jpg owns the (new) name.
        fs::remove_file(&dest).unwrap();
        fs::write(&tmp, b"jpg-bytes").unwrap();
        let (_, dest2) = place_output(&orig, &tmp, true, "jpg").unwrap();
        assert_eq!(dest2, d.join("photo.jpg"));
        assert!(!orig.exists(), "png must be deleted on replace");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn image_replace_same_ext_swaps_in_place() {
        // jpg → jpg replace keeps the same name via the bak swap.
        let d = tmp_dir("image-replace");
        let orig = d.join("photo.jpg");
        let tmp = d.join("tmp-out.jpg");
        fs::write(&orig, b"old").unwrap();
        fs::write(&tmp, b"new").unwrap();
        let (_, dest) = place_output(&orig, &tmp, true, "jpg").unwrap();
        assert_eq!(dest, orig);
        assert_eq!(fs::read(&orig).unwrap(), b"new");
        assert!(!d.join("photo.orig.bak").exists());
        fs::remove_dir_all(&d).ok();
    }

    fn test_media() -> MediaFile {
        MediaFile {
            path: PathBuf::from("t.mp4"),
            bytes: 1_000_000_000,
            vcodec: "hevc".into(),
            width: 3840,
            height: 2160,
            fps: 24.0,
            pix_fmt: "yuv420p10le".into(),
            bit_depth: 10,
            color_space: "bt2020nc".into(),
            color_primaries: "bt2020".into(),
            color_transfer: "smpte2084".into(),
            color_range: "tv".into(),
            vbitrate: Some(30_000_000),
            format_bitrate: None,
            duration_s: 600.0,
            audio: vec![],
            acodec: "none".into(),
            video_stream_count: 1,
            audio_stream_count: 0,
            sub_count: 0,
            attach_count: 0,
            has_chapters: false,
        }
    }

    fn nvenc_args(m: &MediaFile) -> Vec<String> {
        let plan = StreamPlan {
            has_audio: false,
            copy_subs: false,
            audio_copy_eligible: false,
            audio_codec: crate::pipeline::AudioCodec::Opus(crate::pipeline::DEFAULT_OPUS_BPS),
            opus_bps: Some(crate::pipeline::DEFAULT_OPUS_BPS),
            all_audio: false,
            force_aac: false,
            audio_layout_note: "",
        };
        build_args(
            m,
            Level::HwHevc,
            NvencPreset::P5,
            28,
            crate::pipeline::ScalePolicy::Preserve,
            true,
            Path::new("out.mkv"),
            &plan,
            &[],
        )
    }

    #[test]
    fn nvenc_args_carry_quality_flags() {
        let args = nvenc_args(&test_media());
        for flag in [
            "-rc-lookahead",
            "32",
            "-spatial-aq",
            "1",
            "-temporal-aq",
            "1",
        ] {
            assert!(args.contains(&flag.to_string()), "missing {flag}: {args:?}");
        }
    }

    #[test]
    fn hdr_sources_keep_color_tags() {
        let m = test_media();
        assert!(m.is_hdr());
        let args = nvenc_args(&m);
        let joined = args.join(" ");
        assert!(joined.contains("-colorspace:v bt2020nc"), "{joined}");
        assert!(joined.contains("-color_trc:v smpte2084"), "{joined}");
        // SDR must not gain color flags (encoder defaults already apply).
        let mut sdr = m.clone();
        sdr.color_transfer = "bt709".into();
        sdr.color_primaries = "bt709".into();
        sdr.bit_depth = 8;
        assert!(!sdr.is_hdr());
        let sdr_args = nvenc_args(&sdr).join(" ");
        assert!(!sdr_args.contains("-colorspace:v"), "{sdr_args}");
    }

    #[test]
    fn extra_args_split_like_a_shell() {
        assert!(split_extra_args("").is_empty());
        assert!(split_extra_args("   ").is_empty());
        assert_eq!(
            split_extra_args("-tune grain -maxrate 8M"),
            ["-tune", "grain", "-maxrate", "8M"]
        );
        assert_eq!(
            split_extra_args("-vf \"scale=1280:720\" -tune 'hq grain'"),
            ["-vf", "scale=1280:720", "-tune", "hq grain"]
        );
        assert_eq!(
            split_extra_args("-x265-params\\ keyint=250"),
            ["-x265-params keyint=250"]
        );
        // Unterminated quote: rest of line becomes one token, nothing lost.
        assert_eq!(split_extra_args("-tune \"grain"), ["-tune", "grain"]);
    }

    #[test]
    fn extra_args_reject_structural_flags() {
        assert!(parse_extra_args("").unwrap().is_empty());
        assert!(parse_extra_args("-tune grain -maxrate 8M -x265-params keyint=250").is_ok());
        // Overrides of quality knobs are the point — allowed.
        assert!(parse_extra_args("-crf 32 -preset slow").is_ok());
        for bad in [
            "-i in.mkv",
            "-map 0:v",
            "-map_metadata -1",
            "-ss 10",
            "-t 60",
            "-f null",
            "-y",
            "-progress pipe:1",
            "-hwaccel cuda",
            "-c:s copy",
            "-dn",
            "-map_metadata=0:s:0",
        ] {
            assert!(parse_extra_args(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn extra_args_land_before_the_output() {
        let m = test_media();
        let plan = StreamPlan {
            has_audio: false,
            copy_subs: false,
            audio_copy_eligible: false,
            audio_codec: crate::pipeline::AudioCodec::Opus(crate::pipeline::DEFAULT_OPUS_BPS),
            opus_bps: Some(crate::pipeline::DEFAULT_OPUS_BPS),
            all_audio: false,
            force_aac: false,
            audio_layout_note: "",
        };
        let extra = parse_extra_args("-tune grain -maxrate 8M").unwrap();
        let args = build_args(
            &m,
            Level::CpuX264,
            NvencPreset::P5,
            28,
            crate::pipeline::ScalePolicy::Preserve,
            true,
            Path::new("out.mkv"),
            &plan,
            &extra,
        );
        let tail = args[args.len() - 5..].join(" ");
        assert_eq!(tail, "-tune grain -maxrate 8M out.mkv", "{args:?}");
    }

    #[test]
    fn error_snippet_skips_stat_dumps() {
        let tail = "\
[libx264 @ 0000028869018e80] kb/s:4.90\n\
[libx264 @ 0000028869018e80] i16 v,h,dc,p: 95% 0% 5% 0%\n\
[libx264 @ 0000028869018e80] coded y,uvDC,uvAC intra: 0.0%\n\
[libx264 @ 0000028869018e80] Weighted P-Frames: Y:0.0% UV:0.0%\n\
Conversion failed!\n";
        let status = std::process::Command::new("cmd")
            .args(["/c", "exit", "1"])
            .status()
            .expect("cmd for a failing ExitStatus");
        let s = error_snippet(tail, status);
        assert!(s.contains("Conversion failed"), "{s}");
        assert!(!s.contains("i16 v,h"), "{s}");
    }

    #[test]
    fn aac_plan_emits_aac_not_opus() {
        let mut m = test_media();
        m.audio = vec![crate::media::AudioTrack {
            codec: "ac3".into(),
            bitrate: Some(384_000),
            channels: 6,
            layout: "5.1(side)".into(),
            sample_rate: Some(48000),
        }];
        m.acodec = "ac3".into();
        let plan = crate::pipeline::plan_streams(&m, true, Some(64_000), false);
        assert_eq!(plan.audio_codec, crate::pipeline::AudioCodec::Aac(128_000));
        let args = build_args(
            &m,
            Level::CpuX264,
            NvencPreset::P5,
            28,
            crate::pipeline::ScalePolicy::Preserve,
            true,
            Path::new("out.mkv"),
            &plan,
            &[],
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:a aac -b:a 128k"), "{joined}");
        assert!(!joined.contains("libopus"), "{joined}");
    }

    #[test]
    fn all_audio_maps_every_track_with_own_codec() {
        let mut m = test_media();
        m.audio = vec![
            crate::media::AudioTrack {
                codec: "aac".into(),
                bitrate: Some(32_000),
                channels: 2,
                layout: "stereo".into(),
                sample_rate: Some(48000),
            },
            crate::media::AudioTrack {
                codec: "dts".into(),
                bitrate: Some(768_000),
                channels: 6,
                layout: "5.1".into(),
                sample_rate: Some(48000),
            },
        ];
        m.acodec = "aac".into();
        let plan = crate::pipeline::plan_streams(&m, true, Some(64_000), true);
        let args = build_args(
            &m,
            Level::CpuX264,
            NvencPreset::P5,
            28,
            crate::pipeline::ScalePolicy::Preserve,
            true,
            Path::new("out.mkv"),
            &plan,
            &[],
        );
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:a "), "{joined}");
        assert!(!joined.contains("0:a:0"), "{joined}");
        assert!(joined.contains("-c:a:0 copy"), "{joined}");
        assert!(joined.contains("-c:a:1 libopus -b:a:1 64k"), "{joined}");
        assert_eq!(crate::pipeline::audio_mode_label(&m, &plan), "copy+opus64k");
    }

    #[test]
    fn opus_layout_error_detector() {
        assert!(is_opus_layout_error(
            "ffmpeg (hevc_nvenc) failed: [libopus @ 000002084f9de580] Invalid channel layout 5.1(side) for specified mapping family -1."
        ));
        assert!(!is_opus_layout_error(
            "ffmpeg (hevc_nvenc) failed: [hevc_nvenc @ 0] Cannot load libnvidia-encode"
        ));
        assert!(!is_opus_layout_error("output too small (12 bytes)"));
    }

    #[test]
    fn copy_video_args_have_no_encoder_flags() {
        let m = test_media();
        let plan = StreamPlan {
            has_audio: false,
            copy_subs: false,
            audio_copy_eligible: false,
            audio_codec: crate::pipeline::AudioCodec::Opus(crate::pipeline::DEFAULT_OPUS_BPS),
            opus_bps: Some(crate::pipeline::DEFAULT_OPUS_BPS),
            all_audio: false,
            force_aac: false,
            audio_layout_note: "",
        };
        let args = build_args(
            &m,
            Level::CopyVideo,
            NvencPreset::P5,
            28,
            crate::pipeline::ScalePolicy::Force720p,
            true,
            Path::new("out.mkv"),
            &plan,
            &["-crf".into(), "32".into()],
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"), "{joined}");
        assert!(!joined.contains("-crf"), "{joined}");
        assert!(!joined.contains("scale"), "{joined}");
        assert!(!joined.contains("-preset"), "{joined}");
        assert!(!joined.contains("-colorspace"), "{joined}");
    }

    #[test]
    fn output_codec_ok_matches_mode() {
        assert!(output_codec_ok(Level::CopyVideo, "hevc", "hevc"));
        assert!(output_codec_ok(Level::CopyVideo, "mpeg4", "mpeg4"));
        assert!(!output_codec_ok(Level::CopyVideo, "hevc", "h264"));
        assert!(output_codec_ok(Level::HwHevc, "hevc", "hevc"));
        assert!(output_codec_ok(Level::CpuX264, "h264", "av1"));
        assert!(!output_codec_ok(Level::CpuX264, "h264", "mpeg4"));
    }

    #[test]
    fn error_snippet_without_stderr_names_the_exit() {
        let status = std::process::Command::new("cmd")
            .args(["/c", "exit", "1"])
            .status()
            .expect("cmd for a failing ExitStatus");
        let s = error_snippet("", status);
        assert!(s.contains("exited with"), "{s}");
    }

    #[test]
    fn odd_dimensions_get_an_even_shave_not_a_failure() {
        let mut m = test_media();
        m.width = 321;
        m.height = 240;
        let plan = StreamPlan {
            has_audio: false,
            copy_subs: false,
            audio_copy_eligible: false,
            audio_codec: crate::pipeline::AudioCodec::Opus(crate::pipeline::DEFAULT_OPUS_BPS),
            opus_bps: Some(crate::pipeline::DEFAULT_OPUS_BPS),
            all_audio: false,
            force_aac: false,
            audio_layout_note: "",
        };
        let args = build_args(
            &m,
            Level::CpuX264,
            NvencPreset::P5,
            28,
            crate::pipeline::ScalePolicy::Preserve,
            true,
            Path::new("out.mkv"),
            &plan,
            &[],
        );
        let joined = args.join(" ");
        assert!(joined.contains("scale=320:240"), "{joined}");
    }
}
