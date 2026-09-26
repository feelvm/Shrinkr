//! Benchmark mode: same source(s) through x264 CRF28 (baseline A) and
//! HEVC NVENC CQ28 P3–P6 (B–E), recording size/time/fps/realtime/CPU/GPU.
//!
//! Usable two ways:
//!
//! * CLI: `shrinkr-cli --benchmark <file|dir> [--bench-out DIR] [--cq 28] [--dry-run]`
//! * GUI: "Benchmark" section on the main window (runs the first eligible
//!   file, shows the table, offers CSV save).

use crate::ffmpeg::{EncodeBackend, FfmpegBackend};
use crate::media::{human_bytes, MediaFile};
use crate::pipeline::{Level, NvencPreset, ScalePolicy, VideoBackend};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct BenchItem {
    pub tag: String, // "A:x264", "B:P3", ...
    pub backend: VideoBackend,
    pub preset: NvencPreset, // ignored for x264
    pub cq: u32,
}

pub fn matrix(cq: u32) -> Vec<BenchItem> {
    vec![
        BenchItem {
            tag: "A:x264-crf28".into(),
            backend: VideoBackend::CpuX264,
            preset: NvencPreset::P5,
            cq: 28, // baseline stays pinned; NVENC rows sweep `cq`
        },
        BenchItem {
            tag: "B:nvenc-p3".into(),
            backend: VideoBackend::HevcNvenc,
            preset: NvencPreset::P3,
            cq,
        },
        BenchItem {
            tag: "C:nvenc-p4".into(),
            backend: VideoBackend::HevcNvenc,
            preset: NvencPreset::P4,
            cq,
        },
        BenchItem {
            tag: "D:nvenc-p5".into(),
            backend: VideoBackend::HevcNvenc,
            preset: NvencPreset::P5,
            cq,
        },
        BenchItem {
            tag: "E:nvenc-p6".into(),
            backend: VideoBackend::HevcNvenc,
            preset: NvencPreset::P6,
            cq,
        },
    ]
}

#[derive(Clone, Debug)]
pub struct BenchRow {
    pub tag: String,
    pub source: String,
    pub src_bytes: u64,
    pub src_dur: f64,
    pub src_res: String,
    pub src_vcodec: String,
    pub encoder: String,
    pub preset: String,
    pub quality: String,
    pub out_bytes: i64, // -1 on failure/skip
    pub elapsed_s: f64,
    pub avg_fps: f64,
    pub realtime: f64,
    pub ratio: f64,
    pub gpu_avg: u32,
    pub gpu_max: u32,
    pub status: String,
}

impl BenchRow {
    pub fn csv_header() -> &'static str {
        "tag,source,src_bytes,src_dur_s,src_res,src_vcodec,encoder,preset,quality,out_bytes,elapsed_s,avg_fps,realtime_x,ratio_pct,gpu_avg,gpu_max,status"
    }
    pub fn csv_line(&self) -> String {
        format!(
            "{},{},{},{:.1},{},{},{},{},{},{},{:.1},{:.1},{:.2},{:.1},{},{},{}",
            csv_esc(&self.tag),
            csv_esc(&self.source),
            self.src_bytes,
            self.src_dur,
            self.src_res,
            self.src_vcodec,
            self.encoder,
            self.preset,
            self.quality,
            self.out_bytes,
            self.elapsed_s,
            self.avg_fps,
            self.realtime,
            self.ratio * 100.0,
            self.gpu_avg,
            self.gpu_max,
            csv_esc(&self.status),
        )
    }

    pub fn fail(tag: &str, m: &MediaFile, status: String) -> Self {
        Self {
            tag: tag.into(),
            source: m.path.display().to_string(),
            src_bytes: m.bytes,
            src_dur: m.duration_s,
            src_res: m.res_label(),
            src_vcodec: m.vcodec.clone(),
            encoder: "-".into(),
            preset: "-".into(),
            quality: "-".into(),
            out_bytes: -1,
            elapsed_s: 0.0,
            avg_fps: 0.0,
            realtime: 0.0,
            ratio: 0.0,
            gpu_avg: 0,
            gpu_max: 0,
            status,
        }
    }
}

fn csv_esc(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Run the 5-way matrix on one source file. Outputs go to `out_dir`.
/// `dry_run` prints commands without executing.
pub fn run_on_file(
    m: &MediaFile,
    out_dir: &Path,
    cq: u32,
    scale: ScalePolicy,
    keep_subs: bool,
    dry_run: bool,
    on_step: &dyn Fn(&str),
) -> Vec<BenchRow> {
    let _ = std::fs::create_dir_all(out_dir);
    let be = FfmpegBackend;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut rows = vec![];
    let stem = m.path.file_stem().and_then(|s| s.to_str()).unwrap_or("src");
    for item in matrix(cq) {
        let safe_tag: String = item
            .tag
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let out = out_dir.join(format!("{stem}-{safe_tag}.mkv"));
        if dry_run {
            let levels =
                crate::pipeline::select_levels_for_media(item.backend, crate::hw::caps(), m);
            let level = levels.first().copied().unwrap_or(Level::CpuX264);
            let plan = crate::pipeline::plan_streams(
                m,
                keep_subs,
                Some(crate::pipeline::DEFAULT_OPUS_BPS),
                false,
            );
            let args = crate::ffmpeg::build_args(
                m,
                level,
                item.preset,
                item.cq,
                scale,
                keep_subs,
                &out,
                &plan,
                &[],
            );
            on_step(&format!(
                "[dry-run {}] {}",
                item.tag,
                crate::ffmpeg::command_line(&args)
            ));
            rows.push(BenchRow::fail(&item.tag, m, "dry-run".into()));
            continue;
        }
        on_step(&format!("benchmark {} …", item.tag));
        let t0 = Instant::now();
        let r = be.encode(
            m,
            &out,
            item.backend,
            item.preset,
            item.cq,
            scale,
            keep_subs,
            &[],
            Some(crate::pipeline::DEFAULT_OPUS_BPS),
            false,
            &|_| {},
            &cancel,
        );
        let _wall = t0.elapsed().as_secs_f64();
        match r {
            Ok(er) => {
                rows.push(BenchRow {
                    tag: item.tag.clone(),
                    source: m.path.display().to_string(),
                    src_bytes: m.bytes,
                    src_dur: m.duration_s,
                    src_res: m.res_label(),
                    src_vcodec: m.vcodec.clone(),
                    encoder: er.encoder,
                    preset: er.preset,
                    quality: er.quality,
                    out_bytes: er.output_bytes as i64,
                    elapsed_s: er.elapsed_s,
                    avg_fps: er.avg_fps,
                    realtime: er.realtime,
                    ratio: er.ratio,
                    gpu_avg: er.gpu_avg,
                    gpu_max: er.gpu_max,
                    status: format!(
                        "ok {} hwdec={} audio={}",
                        er.level, er.hw_decode, er.audio_mode
                    ),
                });
            }
            Err(e) => rows.push(BenchRow::fail(&item.tag, m, format!("FAIL: {e}"))),
        }
        let _ = std::fs::remove_file(&out);
    }
    rows
}

pub fn write_csv(path: &Path, rows: &[BenchRow]) -> std::io::Result<()> {
    let mut s = String::from(BenchRow::csv_header());
    s.push('\n');
    for r in rows {
        s.push_str(&r.csv_line());
        s.push('\n');
    }
    std::fs::write(path, s)
}

/// Pick a sensible NVENC default from benchmark rows (smallest file that is
/// still ≥80% of the fastest preset's speed — favours quality per FPS).
/// Returns the winning tag, if any NVENC row succeeded.
pub fn recommend(rows: &[BenchRow]) -> Option<String> {
    let nv: Vec<&BenchRow> = rows
        .iter()
        .filter(|r| r.out_bytes > 0 && r.tag.contains("nvenc"))
        .collect();
    if nv.is_empty() {
        return None;
    }
    let fastest = nv.iter().map(|r| r.avg_fps).fold(0.0f64, f64::max);
    let floor = fastest * 0.8;
    let mut cands: Vec<&BenchRow> = nv.iter().filter(|r| r.avg_fps >= floor).copied().collect();
    if cands.is_empty() {
        cands = nv.clone();
    }
    cands.sort_by(|a, b| a.out_bytes.cmp(&b.out_bytes));
    cands.first().map(|r| {
        format!(
            "{} ({} → {}, {:.1}% of source, {:.0} fps, {:.2}x realtime)",
            r.tag,
            human_bytes(r.src_bytes),
            human_bytes(r.out_bytes as u64),
            r.ratio * 100.0,
            r.avg_fps,
            r.realtime
        )
    })
}

/// Collect candidate media files for CLI benchmark (file or recursive dir).
pub fn collect_inputs(arg: &Path) -> Vec<PathBuf> {
    if arg.is_file() {
        return vec![arg.to_path_buf()];
    }
    let mut hits = vec![];
    for e in walkdir::WalkDir::new(arg)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !e.file_type().is_file() {
            continue;
        }
        if let Some(ext) = e.path().extension().and_then(|s| s.to_str()) {
            if crate::media::MEDIA_EXTS.contains(&ext.to_lowercase().as_str()) {
                hits.push(e.path().to_path_buf());
            }
        }
    }
    hits.sort();
    hits
}
