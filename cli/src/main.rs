//! shrinkr-cli: headless shrink / benchmark / probe / caps.
//!
//! Same `shrinkr-core` pipeline the Dioxus GUI drives, without windows:
//! `--shrink` runs real encodes, `--benchmark` runs the NVENC matrix,
//! `--probe`/`--caps`/`--dry-run` inspect without encoding.

use shrinkr_core::ffmpeg::EncodeBackend;
use shrinkr_core::images::build_image_args;
use shrinkr_core::media::{human_bytes, probe_file};
use shrinkr_core::pipeline::{NvencPreset, ScalePolicy, VideoBackend};
use shrinkr_core::{bench, ffmpeg, hw, pipeline};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn tool_ok(prog: &str) -> bool {
    std::process::Command::new(prog)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ok_str(b: bool) -> &'static str {
    if b {
        "found"
    } else {
        "MISSING"
    }
}

fn cli_help() -> String {
    "shrinkr-cli — headless media shrinker\n\
     \n\
     --shrink <file|dir>       run the real shrink pipeline headlessly\n\
     --replace                 delete originals after verified encode\n\
     \u{20}                      (default: keep both, <stem>.mkv alongside)\n\
     --images / --no-images   shrink still images too — jpg/webp re-encode,\n\
     \u{20}                      png/bmp/tiff → jpeg (default: on)\n\
     --backend <auto|hevc|h264|x264|x265|av1|copy>  (default auto)\n\
     --min-saving <pct>        preflight skip threshold (default 10)\n\
     --target <400MB|2x|50%>  solve per-file quality for a size target\n\
     --benchmark <file|dir>   run A:x264 + B-E:NVENC P3-P6 matrix on each input\n\
     --bench-out <dir>        CSV + notes dir (default ./bench-out)\n\
     --cq <18-40>             quality: NVENC CQ / x264/x265 CRF / AV1 CRF (default 28)\n\
     --scale <preserve|1080p|720p|480p|WxH>  video resolution; WxH caps\n\
     \u{20}                      the box, e.g. 1920x1080 (default preserve)\n\
     --image-cq <18-40>       image quality (default: --cq value)\n\
     --image-scale <policy>   image resolution, same values as --scale\n\
     \u{20}                      (default: --scale value)\n\
     --image-preserve-format  
     \u{20}                      drop, near-lossless 256-color palettes), tiff→tiff\n\
     \u{20}                      (deflate); conversions off (default: off)\n\
     --keep-subs / --no-subs  subtitle copy (default keep)\n\
     --all-audio              keep every audio track (default: primary only)\n\
     --audio <off|32|48|64|96|128|192|256>  Opus kbps, off keeps as-is (default 64)\n\
     --extra-args \"...\"      extra ffmpeg output flags, e.g. \"-tune grain\"\n\
     --dry-run                print ffmpeg commands without encoding\n\
     --probe <file>           dump media inspection\n\
     --caps                   print HW capability detection\n\
     --help                   this text"
        .to_string()
}

fn parse_cli() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        println!("{}", cli_help());
        return 0;
    }
    let mut bench: Option<String> = None;
    let mut shrink: Option<String> = None;
    let mut replace_cli = false;
    let mut images_cli = true;
    let mut backend_cli = VideoBackend::Auto;
    let mut min_saving_cli = 10.0f64;
    let mut bench_out = String::from("bench-out");
    let mut cq: u32 = 28;
    let mut scale = ScalePolicy::Preserve;
    let mut image_cq: Option<u32> = None;
    let mut image_scale: Option<ScalePolicy> = None;
    let mut image_preserve = false;
    let mut keep_subs = true;
    let mut all_audio = false;
    let mut opus_bps: Option<u32> = Some(pipeline::DEFAULT_OPUS_BPS);
    let mut extra_args_raw = String::new();
    let mut target_raw: Option<String> = None;
    let mut dry_run = false;
    let mut probe: Option<String> = None;
    let mut caps = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--benchmark" => {
                i += 1;
                if i < args.len() {
                    bench = Some(args[i].clone());
                }
            }
            "--shrink" => {
                i += 1;
                if i < args.len() {
                    shrink = Some(args[i].clone());
                }
            }
            "--replace" => replace_cli = true,
            "--images" => images_cli = true,
            "--no-images" => images_cli = false,
            "--backend" => {
                i += 1;
                if i < args.len() {
                    backend_cli = match args[i].as_str() {
                        "hevc" => VideoBackend::HevcNvenc,
                        "h264" => VideoBackend::H264Nvenc,
                        "x264" => VideoBackend::CpuX264,
                        "x265" => VideoBackend::CpuX265,
                        "av1" => VideoBackend::CpuAv1,
                        "copy" => VideoBackend::CopyVideo,
                        _ => VideoBackend::Auto,
                    };
                }
            }
            "--min-saving" => {
                i += 1;
                if i < args.len() {
                    min_saving_cli = args[i].parse::<f64>().unwrap_or(10.0).clamp(0.0, 100.0);
                }
            }
            "--bench-out" => {
                i += 1;
                if i < args.len() {
                    bench_out = args[i].clone();
                }
            }
            "--cq" => {
                i += 1;
                if i < args.len() {
                    cq = args[i].parse().unwrap_or(28).clamp(18, 40);
                }
            }
            "--scale" => {
                i += 1;
                if i < args.len() {
                    match ScalePolicy::parse(&args[i]) {
                        Some(p) => scale = p,
                        None => {
                            eprintln!(
                                "--scale rejected: {:?} — use preserve, 1080p/720p/480p or WxH (e.g. 1920x1080)",
                                args[i]
                            );
                            return 2;
                        }
                    }
                }
            }
            "--image-cq" => {
                i += 1;
                if i < args.len() {
                    image_cq = Some(args[i].parse().unwrap_or(28).clamp(18, 40));
                }
            }
            "--image-scale" => {
                i += 1;
                if i < args.len() {
                    match ScalePolicy::parse(&args[i]) {
                        Some(p) => image_scale = Some(p),
                        None => {
                            eprintln!(
                                "--image-scale rejected: {:?} — use preserve, 1080p/720p/480p or WxH (e.g. 1920x1080)",
                                args[i]
                            );
                            return 2;
                        }
                    }
                }
            }
            "--image-preserve-format" => image_preserve = true,
            "--keep-subs" => keep_subs = true,
            "--no-subs" => keep_subs = false,
            "--all-audio" => all_audio = true,
            "--audio" => {
                i += 1;
                if i < args.len() {
                    match args[i].as_str() {
                        "off" => opus_bps = None,
                        v => match v.parse::<u32>() {
                            Ok(k) if (8..=512).contains(&k) => opus_bps = Some(k * 1000),
                            _ => {
                                eprintln!("--audio rejected: {v:?} — use off or kbps 8–512");
                                return 2;
                            }
                        },
                    }
                }
            }
            "--extra-args" => {
                i += 1;
                if i < args.len() {
                    extra_args_raw = args[i].clone();
                }
            }
            "--target" => {
                i += 1;
                if i < args.len() {
                    target_raw = Some(args[i].clone());
                }
            }
            "--dry-run" => dry_run = true,
            "--probe" => {
                i += 1;
                if i < args.len() {
                    probe = Some(args[i].clone());
                }
            }
            "--caps" => caps = true,
            "--help" | "-h" => {
                println!("{}", cli_help());
                return 0;
            }
            other => {
                eprintln!("unknown arg: {other}\n{}", cli_help());
                return 2;
            }
        }
        i += 1;
    }
    if caps {
        println!("{}", hw::caps().summary());
        println!("ffmpeg NVENC path keeps frames GPU-resident (NVDEC→CUDA→NVENC).");
        return 0;
    }
    if let Some(p) = probe {
        match probe_file(Path::new(&p)) {
            Ok(m) => println!("{:#?}", m),
            Err(e) => {
                eprintln!("probe failed: {:#}", e);
                return 1;
            }
        }
        return 0;
    }
    if let Some(b) = bench {
        return cli_benchmark(
            Path::new(&b),
            Path::new(&bench_out),
            cq,
            scale,
            keep_subs,
            dry_run,
        );
    }
    if let Some(s) = shrink {
        let extra_args = match ffmpeg::parse_extra_args(&extra_args_raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("--extra-args rejected: {e}");
                return 2;
            }
        };
        let target = match target_raw {
            None => None,
            Some(t) => match shrinkr_core::target::parse_target(&t) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("--target rejected: {e}");
                    return 2;
                }
            },
        };
        return cli_shrink(
            Path::new(&s),
            replace_cli,
            images_cli,
            backend_cli,
            cq,
            scale,
            image_cq.unwrap_or(cq),
            image_scale.unwrap_or(scale),
            image_preserve,
            keep_subs,
            all_audio,
            min_saving_cli,
            extra_args,
            opus_bps,
            target,
            dry_run,
        );
    }
    println!("{}", cli_help());
    0
}

/// Headless shrink: the exact GUI worker path (preflight → FfmpegBackend
/// with fallback → place_output → verify), run sequentially with loud
/// per-file logging. Use it to test "Shrink does nothing" reports.
#[allow(clippy::too_many_arguments)]
fn cli_shrink(
    input: &Path,
    replace: bool,
    images: bool,
    backend: VideoBackend,
    cq: u32,
    scale: ScalePolicy,
    image_cq: u32,
    image_scale: ScalePolicy,
    image_preserve: bool,
    keep_subs: bool,
    all_audio: bool,
    min_saving: f64,
    extra_args: Vec<String>,
    opus_bps: Option<u32>,
    target: Option<shrinkr_core::target::TargetSpec>,
    dry_run: bool,
) -> i32 {
    println!("HW: {}", hw::caps().summary());
    println!(
        "tools: ffmpeg={} ffprobe={}",
        ok_str(tool_ok("ffmpeg")),
        ok_str(tool_ok("ffprobe"))
    );
    if !tool_ok("ffmpeg") || !tool_ok("ffprobe") {
        eprintln!("ffmpeg/ffprobe missing on PATH — install them first.");
        return 1;
    }
    // Videos + still images; --no-images drops the image half.
    let mut inputs: Vec<PathBuf> = shrinkr_core::images::collect_shrink_inputs(input);
    let found = inputs.len();
    if !images {
        inputs.retain(|p| !shrinkr_core::images::is_shrinkable_image(p));
    }
    if inputs.is_empty() {
        eprintln!(
            "no shrinkable files under {}{}",
            input.display(),
            if found > 0 && !images {
                " (--no-images excluded them)"
            } else {
                ""
            }
        );
        return 1;
    }
    println!(
        "shrink {} input(s): backend={:?} cq={} image-cq={} preserve={} replace={} min-saving={:.0}%{}",
        inputs.len(),
        backend,
        cq,
        image_cq,
        image_preserve,
        replace,
        min_saving,
        if extra_args.is_empty() {
            String::new()
        } else {
            format!(" extra=[{}]", extra_args.join(" "))
        }
    );
    let be = ffmpeg::FfmpegBackend;
    let cancel = Arc::new(AtomicBool::new(false));
    let tmp_root = std::env::temp_dir().join("shrinkr-cli");
    let _ = std::fs::create_dir_all(&tmp_root);
    let mut failed = 0;
    // CLI shrink uses the P5 default like the GUI default.
    let wp = pipeline::EstParams {
        backend,
        preset: NvencPreset::P5,
        cq,
        scale,
        opus_bps,
        all_audio,
        image_cq,
        image_scale,
        image_preserve_format: image_preserve,
    };
    for (n, p) in inputs.iter().enumerate() {
        let m = match probe_file(p) {
            Ok(m) => m,
            Err(e) => {
                println!("[{n}] SKIP {}: probe failed: {:#}", p.display(), e);
                failed += 1;
                continue;
            }
        };
        match pipeline::preflight_auto(&m, &wp, min_saving) {
            pipeline::Preflight::Skip { reason } => {
                println!("[{n}] SKIP {}: {}", p.display(), reason);
                continue;
            }
            pipeline::Preflight::Shrink { reason } => {
                println!("[{n}] {} ({} → {})", p.display(), m.res_label(), reason);
            }
        }
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
        // Still-image branch: same preflight/verify/place contract, but a
        // dedicated encoder, temp extension and no target solve.
        if shrinkr_core::images::is_shrinkable_image(p) {
            let mut plan = match shrinkr_core::images::plan_image(&m, image_preserve) {
                Ok(pl) => pl,
                Err(reason) => {
                    println!("[{n}] SKIP {}: {reason}", p.display());
                    continue;
                }
            };
            let tmp_out = tmp_root.join(format!("{stem}-cli-{n}.{}", plan.target_ext));
            let _ = std::fs::remove_file(&tmp_out);
            let args = build_image_args(&m, &plan, image_cq, image_scale, &tmp_out);
            println!("  cmd: {}", ffmpeg::command_line(&args));
            if dry_run {
                continue;
            }
            match shrinkr_core::images::encode_image_with_fallback(
                &m,
                &mut plan,
                image_cq,
                image_scale,
                &tmp_out,
                &cancel,
                image_preserve,
            ) {
                Ok((tmp_final, shrinkr_core::images::ImageOutcome::Done(res))) => {
                    match ffmpeg::place_output(&m.path, &tmp_final, replace, plan.target_ext) {
                        Ok((saved, dest)) => println!(
                            "  DONE: {} → {} ({:.0}%) in {:.1}s [{} {} {}] saved={} orig={} | {}",
                            human_bytes(m.bytes),
                            human_bytes(res.output_bytes),
                            res.ratio * 100.0,
                            res.elapsed_s,
                            res.encoder,
                            res.quality,
                            plan.note,
                            human_bytes(saved.max(0) as u64),
                            if replace { "replaced" } else { "kept" },
                            dest.display()
                        ),
                        Err(e) => {
                            println!("  ENCODED ok but placing output failed: {e}");
                            failed += 1;
                        }
                    }
                }
                Ok((_, shrinkr_core::images::ImageOutcome::NoGain { output_bytes })) => {
                    let _ = std::fs::remove_file(&tmp_out);
                    println!(
                        "  SKIP: no gain ({} → {}) — kept original",
                        human_bytes(m.bytes),
                        human_bytes(output_bytes)
                    );
                }
                Err(e) => {
                    println!("  FAIL: {e}");
                    failed += 1;
                }
            }
            continue;
        }
        let tmp_out = tmp_root.join(format!("{stem}-cli-{n}.mkv"));
        let _ = std::fs::remove_file(&tmp_out);
        // Per-file target solve (after preflight, before the real encode).
        // Skipped under --dry-run: the solver runs trial encodes.
        let mut file_cq = cq;
        if let Some(spec) = target.filter(|_| !dry_run) {
            let work = tmp_root.join("target");
            match shrinkr_core::target::solve_crf_for_file(
                &m,
                backend,
                NvencPreset::P5,
                cq,
                scale,
                keep_subs,
                &extra_args,
                opus_bps,
                all_audio,
                &spec,
                &work,
                &|s| println!("  {s}"),
            ) {
                Ok(o) => {
                    println!(
                        "  target {} → quality {} (predicted {}){}",
                        spec.describe(),
                        o.crf,
                        human_bytes(o.predicted_bytes),
                        if o.reachable {
                            ""
                        } else {
                            " — closest achievable"
                        }
                    );
                    file_cq = o.crf;
                }
                Err(e) => {
                    println!("  target solve failed, using cq{cq}: {e}");
                }
            }
        }
        // Echo exact command (same builder the GUI uses).
        {
            let caps = hw::caps();
            let levels = pipeline::select_levels_for_media(backend, caps, &m);
            if let Some(l0) = levels.first() {
                let plan = pipeline::plan_streams(&m, keep_subs, opus_bps, all_audio);
                let args = ffmpeg::build_args(
                    &m,
                    *l0,
                    NvencPreset::P5,
                    file_cq,
                    scale,
                    keep_subs,
                    &tmp_out,
                    &plan,
                    &extra_args,
                );
                println!("  cmd: {}", ffmpeg::command_line(&args));
            }
        }
        if dry_run {
            continue;
        }
        // NOTE: benchmark matrix varies the preset; shrink uses P5.
        let r = be.encode(
            &m,
            &tmp_out,
            backend,
            NvencPreset::P5,
            file_cq,
            scale,
            keep_subs,
            &extra_args,
            opus_bps,
            all_audio,
            &|_| {},
            &cancel,
        );
        let er = match r {
            Ok(er) => er,
            Err(e) => {
                println!("  FAIL: {e}");
                failed += 1;
                continue;
            }
        };
        match ffmpeg::place_output(&m.path, &tmp_out, replace, "mkv") {
            Ok((saved, dest)) => match probe_file(&dest) {
                Ok(vf) => println!(
                    "  DONE: {} → {} ({:.0}%) in {:.0}s, {:.0} fps, {:.2}x [{} {} hwdec={} audio={}] saved={} orig={} | verify: {}/{} {}, VLC-playable container mkv",
                    human_bytes(m.bytes),
                    human_bytes(er.output_bytes),
                    er.ratio * 100.0,
                    er.elapsed_s,
                    er.avg_fps,
                    er.realtime,
                    er.encoder,
                    er.preset,
                    er.hw_decode,
                    er.audio_mode,
                    human_bytes(saved.max(0) as u64),
                    if replace { "replaced" } else { "kept" },
                    vf.vcodec,
                    vf.acodec,
                    vf.res_label()
                ),
                Err(e) => {
                    println!(
                        "  DONE but verify probe failed for {}: {:#}",
                        dest.display(),
                        e
                    );
                    failed += 1;
                }
            },
            Err(e) => {
                println!("  ENCODED ok but placing output failed: {e}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        eprintln!("{failed} file(s) failed/skipped-by-error.");
        1
    } else {
        0
    }
}

fn cli_benchmark(
    input: &Path,
    out_dir: &Path,
    cq: u32,
    scale: ScalePolicy,
    keep_subs: bool,
    dry_run: bool,
) -> i32 {
    println!("HW: {}", hw::caps().summary());
    let inputs = bench::collect_inputs(input);
    if inputs.is_empty() {
        if shrinkr_core::images::is_shrinkable_image(input) {
            eprintln!(
                "benchmark is video-only — {} is a still image (images don't run the x264/NVENC matrix)",
                input.display()
            );
        } else {
            eprintln!("no media files under {}", input.display());
        }
        return 1;
    }
    println!("{} input(s), cq={}, dry_run={}", inputs.len(), cq, dry_run);
    let _ = std::fs::create_dir_all(out_dir);
    let mut all: Vec<bench::BenchRow> = vec![];
    for p in &inputs {
        let m = match probe_file(p) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("skip {}: {:#}", p.display(), e);
                continue;
            }
        };
        println!(
            "\n== {} ({} {}, {:.1} Mbps?) ==",
            p.display(),
            m.vcodec,
            m.res_label(),
            pipeline::effective_vbitrate(&m)
                .map(|b| b as f64 / 1e6)
                .unwrap_or(-1.0)
        );
        let rows = bench::run_on_file(&m, out_dir, cq, scale, keep_subs, dry_run, &|s| {
            println!("  {}", s);
        });
        for r in &rows {
            println!(
                "  {:12} {:10}/{:5} out={:>10} {:5.0}% {:6.0}s {:6.0}fps {:5.2}x gpu={}/{}% {}",
                r.tag,
                r.encoder,
                r.preset,
                if r.out_bytes > 0 {
                    human_bytes(r.out_bytes as u64)
                } else {
                    "-".into()
                },
                r.ratio * 100.0,
                r.elapsed_s,
                r.avg_fps,
                r.realtime,
                r.gpu_avg,
                r.gpu_max,
                r.status
            );
        }
        if !dry_run {
            if let Some(rec) = bench::recommend(&rows) {
                println!("  recommended: {}", rec);
            }
        }
        all.extend(rows);
    }
    if !dry_run {
        let csv = out_dir.join("benchmark.csv");
        if bench::write_csv(&csv, &all).is_ok() {
            println!("\nCSV: {}", csv.display());
        }
    }
    0
}

fn main() {
    std::process::exit(parse_cli());
}
