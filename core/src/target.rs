//! Target-size mode: "shrink this file to 400 MB / 2x".
//!
//! Instead of guessing a CRF, the solver measures this file's own
//! size curve: cut a few short samples across the duration, encode them
//! at two CRFs with the exact pipeline settings, fit a log-linear
//! `size ≈ A·r^crf` curve, and solve for the CRF that hits the target.
//! Absolute targets (`400MB`, `1.2GB`) and ratios (`2x` = half the bytes,
//! `50%`) both reduce to a target output/input ratio, so one solver
//! covers both forms.

use crate::ffmpeg::{EncodeBackend, FfmpegBackend};
use crate::media::{human_bytes, MediaFile};
use crate::pipeline::{ScalePolicy, VideoBackend};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// A parsed size target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TargetSpec {
    /// Exact output size in bytes.
    Bytes(u64),
    /// Shrink factor: 2.0 = half the bytes, 1.5 = two-thirds, …
    Ratio(f64),
}

impl TargetSpec {
    /// Target output/input byte ratio for a source of `source_bytes`.
    pub fn ratio(&self, source_bytes: u64) -> f64 {
        match self {
            TargetSpec::Bytes(b) => *b as f64 / source_bytes.max(1) as f64,
            TargetSpec::Ratio(r) => 1.0 / r,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            TargetSpec::Bytes(b) => human_bytes(*b),
            TargetSpec::Ratio(r) => format!("{r}x smaller"),
        }
    }
}

/// Parse `400`, `400MB`, `400M`, `1.2GB`, `1.2G`, `800KB`, `2x`, `50%`.
/// Bare numbers mean MB. Errors explain the accepted forms.
pub fn parse_target(s: &str) -> Result<TargetSpec, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty target — try 400MB or 2x".into());
    }
    let lower = t.to_lowercase();
    if let Some(n) = lower.strip_suffix('x') {
        let v: f64 = n
            .trim()
            .parse()
            .map_err(|_| format!("bad ratio {t:?} — try 2x or 1.5x"))?;
        if !(v > 1.0) {
            return Err(format!("ratio must be over 1x (got {t:?})"));
        }
        if v > 100.0 {
            return Err(format!("ratio {t:?} is out of reach — max 100x"));
        }
        return Ok(TargetSpec::Ratio(v));
    }
    if let Some(n) = lower.strip_suffix('%') {
        let v: f64 = n
            .trim()
            .parse()
            .map_err(|_| format!("bad percent {t:?} — try 50%"))?;
        if !(v > 0.0) || v > 100.0 {
            return Err(format!("percent must be 0–100 (got {t:?})"));
        }
        return Ok(TargetSpec::Ratio(100.0 / v));
    }
    // Absolute: leading number + trailing unit.
    let split = t.find(|c: char| c.is_alphabetic()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad size {t:?} — try 400MB or 1.2GB"))?;
    if !(v > 0.0) {
        return Err(format!("size must be positive (got {t:?})"));
    }
    let mult: f64 = match unit.trim().to_lowercase().as_str() {
        "" | "mb" | "m" => 1024.0 * 1024.0,
        "b" => 1.0,
        "kb" | "k" => 1024.0,
        "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        u => return Err(format!("unknown unit {u:?} — use KB, MB, GB or TB")),
    };
    Ok(TargetSpec::Bytes((v * mult) as u64))
}

/// Fit `ln(ratio) = a + b·crf` (least squares over all points) and solve
/// for the CRF hitting `target`. Returns `(crf, predicted_ratio,
/// reachable)` with `crf` clamped to `[lo, hi]`; `reachable=false` means
/// even `hi` can't get there (closest reported).
pub fn solve_crf(measurements: &[(u32, f64)], target: f64, lo: u32, hi: u32) -> (u32, f64, bool) {
    let pick_closest = |target: f64| {
        let &(c, r) = measurements
            .iter()
            .min_by(|(c1, r1), (c2, r2)| {
                (r1 - target)
                    .abs()
                    .partial_cmp(&(r2 - target).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(c1.cmp(c2))
            })
            .expect("at least one measurement");
        (c.clamp(lo, hi), r, r <= target)
    };
    if !(target > 0.0) {
        return pick_closest(target);
    }
    let pts: Vec<(f64, f64)> = measurements
        .iter()
        .filter(|(_, r)| *r > 0.0)
        .map(|&(c, r)| (c as f64, r.ln()))
        .collect();
    if pts.len() < 2 {
        return pick_closest(target);
    }
    let n = pts.len() as f64;
    let (sx, sy) = pts
        .iter()
        .fold((0.0, 0.0), |(x, y), &(c, l)| (x + c, y + l));
    let (sxx, sxy) = pts
        .iter()
        .fold((0.0, 0.0), |(xx, xy), &(c, l)| (xx + c * c, xy + c * l));
    let denom = n * sxx - sx * sx;
    if denom.abs() < 1e-9 {
        return pick_closest(target);
    }
    let b = (n * sxy - sx * sy) / denom;
    if b >= -1e-9 {
        // Flat or inverted (noisy samples): closest endpoint wins.
        return pick_closest(target);
    }
    let a = (sy - b * sx) / n;
    let star = (target.ln() - a) / b;
    let reachable = star <= hi as f64 + 1e-9;
    let c = (star.round() as i64).clamp(lo as i64, hi as i64) as u32;
    (c, (a + b * c as f64).exp(), reachable)
}

/// Sample windows covering the duration: up to `max_n` slices of `len_s`
/// centered at evenly spread fractions (20/50/80% for 3). Short files get
/// fewer, non-overlapping-or-not windows — representativeness degrades
/// gracefully instead of failing.
pub fn sample_plan(duration_s: f64, max_n: usize, len_s: f64) -> Vec<(f64, f64)> {
    if !(duration_s > 0.0) || !(len_s > 0.0) || max_n == 0 {
        return vec![];
    }
    let n = ((duration_s / len_s).floor() as usize).max(1).min(max_n);
    (0..n)
        .map(|i| {
            let frac = (i as f64 + 1.0) / (n as f64 + 1.0);
            let start = (duration_s * frac - len_s / 2.0).clamp(0.0, (duration_s - len_s).max(0.0));
            (start, len_s.min(duration_s))
        })
        .collect()
}

/// Stream-copy one sample window (primary video + primary audio, no
/// re-encode — seconds even on huge files).
pub fn extract_sample(src: &Path, start_s: f64, len_s: f64, out: &Path) -> Result<(), String> {
    let st = crate::process::cmd("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
            &format!("{start_s:.1}"),
            "-i",
            &src.to_string_lossy(),
            "-t",
            &format!("{len_s:.1}"),
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-c",
            "copy",
            &out.to_string_lossy(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("sample ffmpeg spawn: {e}"))?;
    if !st.success() {
        return Err(format!("sample cut failed ({st})"));
    }
    Ok(())
}

fn file_bytes(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Outcome of a target solve.
#[derive(Clone, Debug)]
pub struct SolveOutcome {
    pub crf: u32,
    /// Predicted full-file output bytes at `crf` (fitted curve).
    pub predicted_bytes: u64,
    /// False when even the max CRF can't reach the target (closest given).
    pub reachable: bool,
    /// (crf, sample_src_bytes, sample_out_bytes) per probe point.
    pub measured: Vec<(u32, u64, u64)>,
}

/// Solve the CRF hitting `spec` for one file with the exact pipeline
/// settings (backend/preset/scale/audio/subs/extras). Encodes small
/// samples, so NVENC solves take seconds; CPU backends take minutes.
/// `cq0` is the current slider value (one probe point); the other is
/// `cq0+4` (or below when the slider is already maxed).
#[allow(clippy::too_many_arguments)]
pub fn solve_crf_for_file(
    m: &MediaFile,
    backend: VideoBackend,
    preset: crate::pipeline::NvencPreset,
    cq0: u32,
    scale: ScalePolicy,
    keep_subs: bool,
    extra_args: &[String],
    opus_bps: Option<u32>,
    all_audio: bool,
    spec: &TargetSpec,
    work_dir: &Path,
    on_step: &dyn Fn(&str),
) -> Result<SolveOutcome, String> {
    const LO: u32 = 18;
    const HI: u32 = 40; // shared quality-slider range on every backend
    if backend == VideoBackend::CopyVideo {
        return Err(
            "target mode needs an encoder backend — copy-video has no quality to solve".into(),
        );
    }
    let target = spec.ratio(m.bytes);
    if !(target > 0.0) {
        return Err("could not derive a target ratio".into());
    }
    if target >= 1.0 {
        return Err(format!(
            "target {} is larger than the source {} — nothing to solve",
            spec.describe(),
            human_bytes(m.bytes)
        ));
    }
    let _ = std::fs::create_dir_all(work_dir);
    let stem = m
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("target");
    // 1) Cut samples.
    let plan = sample_plan(m.duration_s.max(60.0), 3, 45.0);
    let mut sample_paths: Vec<PathBuf> = vec![];
    let mut sample_src_bytes = 0u64;
    for (k, (start, len)) in plan.iter().enumerate() {
        let p = work_dir.join(format!("{stem}-sample{k}.mkv"));
        let _ = std::fs::remove_file(&p);
        on_step(&format!(
            "target probe: cutting sample {}/{} ({}s @ {}s)…",
            k + 1,
            plan.len(),
            len,
            start.round() as u64
        ));
        extract_sample(&m.path, *start, *len, &p)?;
        sample_src_bytes += file_bytes(&p);
        sample_paths.push(p);
    }
    if sample_src_bytes == 0 {
        return Err("samples came out empty — source may be unreadable".into());
    }
    // 2) Encode samples at two CRFs bracketing the slider value.
    let c1 = (cq0 + 4).min(HI);
    let c0 = c1.saturating_sub(4).max(LO);
    let be = FfmpegBackend;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut measured: Vec<(u32, u64, u64)> = vec![];
    // Probe encodes, factored so the refinement round reuses them.
    let probe_at = |c: u32,
                    measured: &mut Vec<(u32, u64, u64)>,
                    on_step: &dyn Fn(&str)|
     -> Result<(), String> {
        let mut out_bytes = 0u64;
        for (k, sp) in sample_paths.iter().enumerate() {
            let sm = crate::media::probe_file(sp).map_err(|e| format!("sample probe: {e:#}"))?;
            let tmp = work_dir.join(format!("{stem}-probe{c}-{k}.mkv"));
            let _ = std::fs::remove_file(&tmp);
            on_step(&format!(
                "target probe: sample {}/{} at {} {}…",
                k + 1,
                sample_paths.len(),
                quality_word(backend),
                c
            ));
            let er = be
                .encode(
                    &sm,
                    &tmp,
                    backend,
                    preset,
                    c,
                    scale,
                    keep_subs,
                    extra_args,
                    opus_bps,
                    all_audio,
                    &|_| {},
                    &cancel,
                )
                .map_err(|e| format!("probe encode at {c} failed: {e}"))?;
            out_bytes += er.output_bytes;
            let _ = std::fs::remove_file(&tmp);
        }
        measured.push((c, sample_src_bytes, out_bytes));
        on_step(&format!(
            "target probe: {} {} → {} ({:.0}% of sample)",
            quality_word(backend),
            c,
            human_bytes(out_bytes),
            out_bytes as f64 / sample_src_bytes as f64 * 100.0
        ));
        Ok(())
    };
    for &c in &[c0, c1] {
        probe_at(c, &mut measured, on_step)?;
    }
    // 3) Fit + solve, with one refinement round when the solution lies
    // well outside the probed interval (extrapolation is where the error
    // lives — measuring at the solution fixes most of it).
    let ratios_of = |measured: &[(u32, u64, u64)]| {
        measured
            .iter()
            .map(|&(c, src, out)| (c, out as f64 / src.max(1) as f64))
            .collect::<Vec<(u32, f64)>>()
    };
    let (mut crf, mut predicted_ratio, mut reachable) =
        solve_crf(&ratios_of(&measured), target, LO, HI);
    let probed = |c: u32| measured.iter().any(|&(mc, _, _)| mc == c);
    if reachable && !probed(crf) && (crf > c1 + 2 || crf + 2 < c0) {
        on_step(&format!(
            "target probe: refining at {} {} (outside probed range)…",
            quality_word(backend),
            crf
        ));
        probe_at(crf, &mut measured, on_step)?;
        (crf, predicted_ratio, reachable) = solve_crf(&ratios_of(&measured), target, LO, HI);
    }
    for p in &sample_paths {
        let _ = std::fs::remove_file(p);
    }
    Ok(SolveOutcome {
        crf,
        predicted_bytes: (predicted_ratio * m.bytes as f64) as u64,
        reachable,
        measured,
    })
}

fn quality_word(backend: VideoBackend) -> &'static str {
    match backend {
        VideoBackend::CpuX264 | VideoBackend::CpuX265 | VideoBackend::CpuAv1 => "CRF",
        _ => "CQ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_absolute_forms() {
        assert_eq!(
            parse_target("400MB").unwrap(),
            TargetSpec::Bytes(400 * 1024 * 1024)
        );
        assert_eq!(
            parse_target("1.2gb").unwrap(),
            TargetSpec::Bytes((1.2 * 1024.0 * 1024.0 * 1024.0) as u64)
        );
        assert_eq!(
            parse_target("500").unwrap(),
            TargetSpec::Bytes(500 * 1024 * 1024)
        );
        assert_eq!(parse_target("800k").unwrap(), TargetSpec::Bytes(800 * 1024));
        assert_eq!(
            parse_target("2G").unwrap(),
            TargetSpec::Bytes(2 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_ratio_forms() {
        assert_eq!(parse_target("2x").unwrap(), TargetSpec::Ratio(2.0));
        assert_eq!(parse_target("1.5X").unwrap(), TargetSpec::Ratio(1.5));
        assert_eq!(parse_target("50%").unwrap(), TargetSpec::Ratio(2.0));
        assert_eq!(parse_target("  25% ").unwrap(), TargetSpec::Ratio(4.0));
    }

    #[test]
    fn parse_rejects_garbage() {
        for bad in ["", "abc", "0x", "1x", "-3MB", "0", "400XB", "101%", "1000x"] {
            assert!(parse_target(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn ratio_math() {
        let gb = 1024 * 1024 * 1024u64;
        assert!((TargetSpec::Bytes(gb / 2).ratio(gb) - 0.5).abs() < 1e-9);
        assert!((TargetSpec::Ratio(2.0).ratio(gb) - 0.5).abs() < 1e-9);
        assert!((TargetSpec::Ratio(1.5).ratio(gb) - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn solve_halving_curve() {
        // Halves every 4 points: 0.8@28 → 0.4@32 → expect 36 for 0.2.
        let (c, pred, reach) = solve_crf(&[(28, 0.8), (32, 0.4)], 0.2, 18, 40);
        assert_eq!(c, 36);
        assert!((pred - 0.2).abs() < 1e-9);
        assert!(reach);
    }

    #[test]
    fn solve_clamps_and_flags_unreachable() {
        let (c, _, reach) = solve_crf(&[(28, 0.8), (32, 0.4)], 0.01, 18, 40);
        assert_eq!(c, 40);
        assert!(!reach);
        // Easy target above the measured range: extrapolate to better
        // quality (27), still reachable.
        let (c, _, reach) = solve_crf(&[(28, 0.8), (32, 0.4)], 0.9, 18, 40);
        assert_eq!(c, 27);
        assert!(reach);
    }

    #[test]
    fn solve_survives_degenerate_input() {
        // Flat measurements: closest endpoint, no NaN, no panic.
        let (c, _, _) = solve_crf(&[(28, 0.5), (32, 0.5)], 0.2, 18, 40);
        assert!((18..=40).contains(&c));
        let (c, _, _) = solve_crf(&[(28, 0.5)], 0.2, 18, 40);
        assert_eq!(c, 28);
    }

    #[test]
    fn sample_plan_covers_duration() {
        let p = sample_plan(8918.0, 3, 45.0);
        assert_eq!(p.len(), 3);
        for (s, l) in &p {
            assert!(*s >= 0.0 && s + l <= 8918.0 + 1e-6);
        }
        // Short file: fewer windows, still inside.
        let p = sample_plan(100.0, 3, 45.0);
        assert_eq!(p.len(), 2);
        // Degenerate input: empty, not a crash.
        assert!(sample_plan(0.0, 3, 45.0).is_empty());
        assert!(sample_plan(100.0, 0, 45.0).is_empty());
    }
}
