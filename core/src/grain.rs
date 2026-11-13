//! Adaptive SVT-AV1 film-grain synthesis.
//!
//! Bitrate density is a weak grain proxy: a high-bitrate clean source and a
//! grainy source can look identical in the probe numbers. This module reads
//! actual frames, estimates grain in flat picture areas (where there is no
//! real detail to mistake for noise), and — for a real encode — tests a
//! couple of SVT strengths against a plain encode. The measured choice keeps
//! the smallest result whose synthesized grain energy still resembles the
//! source; if nothing passes, grain stays off rather than smoothing the
//! picture.

use crate::hw::HwCaps;
use crate::media::MediaFile;
use crate::pipeline::{NvencPreset, ScalePolicy};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Analysis is intentionally downscaled. Nearest-neighbour scaling keeps
/// grain amplitude intact (area/bilinear scaling averages it away), so the
/// estimate is representative without decoding full-resolution frames.
const ANALYSIS_WIDTH: u32 = 480;
const ANALYSIS_FPS: u32 = 6;
const BLOCK: usize = 8;
/// A block is "flat" when its own contrast is below this 8-bit level.
const FLAT_STD: f32 = 7.0;
/// Too little flat area means the measurement cannot separate grain from
/// real texture; callers fall back to a plain encode.
const MIN_FLAT_FRACTION: f32 = 0.04;
const MIN_FRAMES: usize = 6;
/// SVT guidance: 4–6 for animation, 8 for normal live action, 10–15 for
/// heavy grain. We stop at 16 rather than pushing the encoder's maximum 50.
const MIN_STRENGTH: u8 = 4;
const MAX_STRENGTH: u8 = 16;
/// A candidate must save at least this fraction vs. grain-off before the
/// texture change is worth it.
const MIN_SIZE_GAIN: f64 = 0.95;
/// Texture tolerance. The decoder's regenerated grain is statistically
/// similar, not identical, so the output's grain energy may sit well below
/// the source's. Keep at least half of it, allow some excess, and require
/// most of the textured-area energy to survive; otherwise the encode is
/// just smoothing the picture.
const MIN_GRAIN_RATIO: f32 = 0.50;
const MAX_GRAIN_RATIO: f32 = 1.60;
const MIN_DETAIL_RATIO: f32 = 0.55;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrainProfile {
    /// Estimated grain/noise sigma in 8-bit levels (flat areas).
    pub sigma: f32,
    /// Mean high-frequency energy in textured areas — real detail.
    pub detail: f32,
    /// Fraction of blocks flat enough to measure grain in.
    pub flat_fraction: f32,
    /// Suggested SVT strength, 0 when the source is clean.
    pub strength: u8,
    pub windows: usize,
    pub frames: usize,
    /// Best window for a measured strength test (start, duration).
    pub hottest: (f64, f64),
}

#[derive(Clone, Copy, Debug)]
pub struct GrainChoice {
    /// 0 = leave film-grain synthesis off.
    pub strength: u8,
    pub profile: Option<GrainProfile>,
    /// False when frame analysis could not run; strength is then 0.
    pub measured: bool,
}

#[derive(Default, Clone, Copy)]
struct Measure {
    sigma: f32,
    detail: f32,
    flat_fraction: f32,
    frames: usize,
}

static GRAIN_SEQ: AtomicU64 = AtomicU64::new(0);

fn work_dir(root: &Path) -> PathBuf {
    let seq = GRAIN_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    root.join(format!("grain-{}-{seq}-{nanos}", std::process::id()))
}

fn analysis_windows(duration: f64) -> Vec<(f64, f64)> {
    if !duration.is_finite() || duration < 1.5 {
        return vec![];
    }
    if duration < 6.0 {
        let len = (duration * 0.8).clamp(1.0, 2.0);
        return vec![(0.0, len)];
    }
    crate::quality::sample_windows(duration)
}

fn analysis_size(media: &MediaFile) -> (u32, u32) {
    let src_w = media.width.max(2);
    let src_h = media.height.max(2);
    let mut w = ANALYSIS_WIDTH.min(src_w) & !1;
    w = w.max(16);
    let mut h = ((src_h as f64 * w as f64 / src_w as f64).round() as u32) & !1;
    h = h.clamp(16, 2160);
    (w, h)
}

/// Decode a short window to raw 8-bit grey frames. Writing to a file keeps
/// cancellation/process handling identical to the encode path (line-based
/// readers cannot safely consume raw video).
#[allow(clippy::too_many_arguments)]
fn decode_frames(
    media: &MediaFile,
    start: f64,
    duration: f64,
    width: u32,
    height: u32,
    out: &Path,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create grain work folder: {e}"))?;
    }
    let filter = format!("fps={ANALYSIS_FPS},scale={width}:{height}:flags=neighbor,format=gray");
    let args = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-ss".into(),
        format!("{start:.3}"),
        "-t".into(),
        format!("{duration:.3}"),
        "-noautorotate".into(),
        "-i".into(),
        media.path.to_string_lossy().into_owned(),
        "-an".into(),
        "-sn".into(),
        "-dn".into(),
        "-vf".into(),
        filter,
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "gray".into(),
        out.to_string_lossy().into_owned(),
    ];
    let outcome = crate::process::run_with_progress(
        crate::process::cmd("ffmpeg").args(&args),
        cancel,
        true,
        |_| {},
    )?;
    if !outcome.status.success() {
        let _ = std::fs::remove_file(out);
        return Err(format!(
            "grain analysis decode failed: {}",
            outcome.stderr_tail.lines().last().unwrap_or("ffmpeg error")
        ));
    }
    Ok(())
}

/// Measure one window's frames. Temporal noise in flat blocks is the
/// grain signal; a slowly-changing clean picture (or moving detail) cannot
/// fake it, because flat blocks have no detail to move.
fn measure_window(frames: &[&[u8]], width: usize, height: usize) -> Option<Measure> {
    let bw = width / BLOCK;
    let bh = height / BLOCK;
    if bw == 0 || bh == 0 || frames.len() < 2 {
        return None;
    }
    let total_blocks = bw * bh;
    let mut flat_blocks = 0usize;
    let mut flat_lap_sum = 0f64;
    let mut flat_lap_n = 0usize;
    let mut detail_lap_sum = 0f64;
    let mut detail_lap_n = 0usize;
    let mut pair_sum = 0f64;
    let mut pair_n = 0usize;

    for (fi, frame) in frames.iter().enumerate() {
        let prev = fi.checked_sub(1).and_then(|p| frames.get(p));
        for by in 0..bh {
            for bx in 0..bw {
                let x0 = bx * BLOCK;
                let y0 = by * BLOCK;
                let mut sum = 0f64;
                let mut sum_sq = 0f64;
                for y in y0..y0 + BLOCK {
                    let row = &frame[y * width..(y + 1) * width];
                    for &px in &row[x0..x0 + BLOCK] {
                        let v = f64::from(px);
                        sum += v;
                        sum_sq += v * v;
                    }
                }
                let n = (BLOCK * BLOCK) as f64;
                let mean = sum / n;
                let var = (sum_sq / n - mean * mean).max(0.0);
                let std = var.sqrt() as f32;
                let flat = std < FLAT_STD && mean > 2.0 && mean < 253.0;
                // Laplacian over the block interior (skip the outer ring so
                // neighbours always exist inside the block).
                let mut lap_sum = 0f64;
                let mut lap_n = 0usize;
                for y in y0 + 1..y0 + BLOCK - 1 {
                    let row = &frame[y * width..(y + 1) * width];
                    let up = &frame[(y - 1) * width..y * width];
                    let down = &frame[(y + 1) * width..(y + 2) * width];
                    for x in x0 + 1..x0 + BLOCK - 1 {
                        let lap = (4.0 * f64::from(row[x])
                            - f64::from(row[x - 1])
                            - f64::from(row[x + 1])
                            - f64::from(up[x])
                            - f64::from(down[x]))
                        .abs();
                        lap_sum += lap;
                        lap_n += 1;
                    }
                }
                if lap_n == 0 {
                    continue;
                }
                if flat {
                    flat_blocks += 1;
                    flat_lap_sum += lap_sum;
                    flat_lap_n += lap_n;
                    // Frame-to-frame difference in the same flat block,
                    // minus the block's mean drift (exposure/fade).
                    if let Some(prev) = prev {
                        let mut diff_sum = 0f64;
                        let mut pix_n = 0usize;
                        for y in y0..y0 + BLOCK {
                            for x in x0..x0 + BLOCK {
                                let i = y * width + x;
                                diff_sum += f64::from(frame[i]) - f64::from(prev[i]);
                                pix_n += 1;
                            }
                        }
                        let drift = diff_sum / pix_n.max(1) as f64;
                        for y in y0..y0 + BLOCK {
                            for x in x0..x0 + BLOCK {
                                let i = y * width + x;
                                let d = f64::from(frame[i]) - f64::from(prev[i]) - drift;
                                pair_sum += d.abs();
                                pair_n += 1;
                            }
                        }
                    }
                } else {
                    detail_lap_sum += lap_sum;
                    detail_lap_n += lap_n;
                }
            }
        }
    }

    if flat_lap_n == 0 || total_blocks == 0 {
        return None;
    }
    // For white noise: Laplacian mean-abs ≈ 3.57σ; two-frame difference
    // mean-abs ≈ 1.128σ. Take the quieter estimate so moving detail or
    // flicker cannot inflate the grain decision.
    let spatial = (flat_lap_sum / flat_lap_n as f64 / 3.568) as f32;
    let temporal = if pair_n > 0 {
        (pair_sum / pair_n as f64 / 1.128) as f32
    } else {
        spatial
    };
    Some(Measure {
        sigma: spatial.min(temporal),
        detail: if detail_lap_n > 0 {
            (detail_lap_sum / detail_lap_n as f64 / 3.568) as f32
        } else {
            0.0
        },
        flat_fraction: flat_blocks as f32 / (total_blocks * frames.len()) as f32,
        frames: frames.len(),
    })
}

/// Measure the source. `None` when the source is too short, decoding fails,
/// or there is not enough flat area to separate grain from texture.
pub fn analyze(media: &MediaFile, root: &Path, cancel: &Arc<AtomicBool>) -> Option<GrainProfile> {
    let windows = analysis_windows(media.duration_s);
    if windows.is_empty() {
        return None;
    }
    let dir = work_dir(root);
    std::fs::create_dir_all(&dir).ok()?;
    let (width, height) = analysis_size(media);
    let frame_len = width as usize * height as usize;
    let mut measures = Vec::new();
    let mut hottest: Option<(f64, f64, f32)> = None;
    for (i, (start, duration)) in windows.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        let raw = dir.join(format!("window-{i}.raw"));
        if decode_frames(media, *start, *duration, width, height, &raw, cancel).is_err() {
            continue;
        }
        let bytes = match std::fs::read(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let frames: Vec<&[u8]> = bytes.chunks_exact(frame_len).collect();
        if frames.len() < 2 {
            continue;
        }
        if let Some(m) = measure_window(&frames, width as usize, height as usize) {
            measures.push(m);
            if hottest.is_none_or(|(_, _, score)| m.sigma > score) {
                hottest = Some((*start, *duration, m.sigma));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if measures.is_empty() {
        return None;
    }
    let frames: usize = measures.iter().map(|m| m.frames).sum();
    if frames < MIN_FRAMES {
        return None;
    }
    let mut sigmas: Vec<f32> = measures.iter().map(|m| m.sigma).collect();
    sigmas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let sigma = sigmas[sigmas.len() / 2];
    let flat_fraction =
        measures.iter().map(|m| m.flat_fraction).sum::<f32>() / measures.len() as f32;
    if flat_fraction < MIN_FLAT_FRACTION {
        return None;
    }
    let detail = measures.iter().map(|m| m.detail).sum::<f32>() / measures.len() as f32;
    Some(GrainProfile {
        sigma,
        detail,
        flat_fraction,
        strength: strength_for(sigma),
        windows: measures.len(),
        frames,
        hottest: hottest.map_or((windows[0].0, windows[0].1), |(start, duration, _)| {
            (start, duration)
        }),
    })
}

/// SVT's parameter is a denoise strength, not a grain amplitude: 8 is the
/// documented live-action starting point, 10–15 heavy grain, 4–6 animation.
fn strength_for(sigma: f32) -> u8 {
    let s = if !sigma.is_finite() || sigma < 1.0 {
        0
    } else if sigma < 1.8 {
        4
    } else if sigma < 2.8 {
        6
    } else if sigma < 4.5 {
        8
    } else if sigma < 6.5 {
        11
    } else {
        14
    };
    if s == 0 {
        0
    } else {
        s.clamp(MIN_STRENGTH, MAX_STRENGTH)
    }
}

/// Pure candidate policy: pick the smallest tested strength whose texture
/// still resembles the source and which beats grain-off by `MIN_SIZE_GAIN`.
fn pick_strength(baseline_bytes: u64, options: &[(u8, u64, bool)]) -> u8 {
    let floor = (baseline_bytes as f64 * MIN_SIZE_GAIN) as u64;
    options
        .iter()
        .filter(|(strength, bytes, texture_ok)| *strength > 0 && *texture_ok && *bytes <= floor)
        .min_by_key(|(_, bytes, _)| *bytes)
        .map(|(strength, _, _)| *strength)
        .unwrap_or(0)
}

fn texture_ok(source: &GrainProfile, output: &GrainProfile) -> bool {
    if source.sigma < 0.5 {
        return false;
    }
    let grain_ratio = output.sigma / source.sigma;
    let detail_ratio = if source.detail > 0.5 {
        output.detail / source.detail
    } else {
        1.0
    };
    (MIN_GRAIN_RATIO..=MAX_GRAIN_RATIO).contains(&grain_ratio) && detail_ratio >= MIN_DETAIL_RATIO
}

/// Analyze the source, then test up to two SVT strengths against a plain
/// encode on one representative window. Returns 0 when the source is clean
/// or no candidate preserves enough texture/size.
pub fn choose_strength(
    media: &MediaFile,
    cq: u32,
    preset: NvencPreset,
    scale: ScalePolicy,
    caps: &HwCaps,
    root: &Path,
    cancel: &Arc<AtomicBool>,
) -> GrainChoice {
    let Some(profile) = analyze(media, root, cancel) else {
        return GrainChoice {
            strength: 0,
            profile: None,
            measured: false,
        };
    };
    if profile.strength == 0 {
        return GrainChoice {
            strength: 0,
            profile: Some(profile),
            measured: true,
        };
    }
    let dir = work_dir(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return GrainChoice {
            strength: 0,
            profile: Some(profile),
            measured: true,
        };
    }
    let choice = choose_in(media, &profile, cq, preset, scale, caps, &dir, cancel);
    let _ = std::fs::remove_dir_all(&dir);
    choice.unwrap_or(GrainChoice {
        strength: profile.strength,
        profile: Some(profile),
        measured: true,
    })
}

#[allow(clippy::too_many_arguments)]
fn choose_in(
    media: &MediaFile,
    profile: &GrainProfile,
    cq: u32,
    preset: NvencPreset,
    scale: ScalePolicy,
    caps: &HwCaps,
    dir: &Path,
    cancel: &Arc<AtomicBool>,
) -> Option<GrainChoice> {
    let (start, duration) = profile.hottest;
    let sample = dir.join("sample.mkv");
    crate::ffmpeg::extract_video_sample(media, start, duration.min(2.0), &sample, cancel).ok()?;
    let sample_media = crate::media::probe_file(&sample).ok()?;
    let sample_profile = analyze(&sample_media, dir, cancel)?;
    if sample_profile.sigma < 0.5 {
        return Some(GrainChoice {
            strength: 0,
            profile: Some(*profile),
            measured: true,
        });
    }
    let baseline_path = dir.join("grain-0.mkv");
    crate::ffmpeg::encode_sample(
        &sample_media,
        &baseline_path,
        crate::pipeline::VideoBackend::CpuAv1,
        preset,
        cq,
        scale,
        0,
        caps,
        cancel,
    )
    .ok()?;
    let baseline_bytes = std::fs::metadata(&baseline_path).ok()?.len();
    let candidates = [profile.strength, (profile.strength + 4).min(MAX_STRENGTH)];
    let mut tested = Vec::new();
    for strength in candidates {
        if tested.iter().any(|(s, _, _)| *s == strength) {
            continue;
        }
        let path = dir.join(format!("grain-{strength}.mkv"));
        if crate::ffmpeg::encode_sample(
            &sample_media,
            &path,
            crate::pipeline::VideoBackend::CpuAv1,
            preset,
            cq,
            scale,
            strength,
            caps,
            cancel,
        )
        .is_err()
        {
            continue;
        }
        let bytes = std::fs::metadata(&path)
            .ok()
            .map(|m| m.len())
            .unwrap_or(u64::MAX);
        let Ok(out_media) = crate::media::probe_file(&path) else {
            continue;
        };
        let out_profile = analyze(&out_media, dir, cancel);
        let ok = out_profile
            .as_ref()
            .is_some_and(|out| texture_ok(&sample_profile, out));
        tested.push((strength, bytes, ok));
    }
    let strength = pick_strength(baseline_bytes, &tested);
    Some(GrainChoice {
        strength,
        profile: Some(*profile),
        measured: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_frames(width: usize, height: usize, sigma: f32) -> Vec<Vec<u8>> {
        // Deterministic LCG; independent noise each frame.
        let mut state = 0x1234_5678u32;
        let mut frames = Vec::new();
        for _ in 0..8 {
            let mut frame = vec![120u8; width * height];
            for px in frame.iter_mut() {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Box-Muller-ish: sum of uniforms approximates Gaussian.
                let mut g = 0.0;
                for _ in 0..3 {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    g += (state >> 8) as f32 / 16_777_216.0 - 0.5;
                }
                let noise = g * 2.0 * sigma;
                *px = (120.0 + noise).clamp(0.0, 255.0) as u8;
            }
            frames.push(frame);
        }
        frames
    }

    #[test]
    fn flat_noise_reads_as_grain_and_slopes_strength() {
        let frames = synthetic_frames(160, 120, 6.0);
        let refs: Vec<&[u8]> = frames.iter().map(|f| f.as_slice()).collect();
        let m = measure_window(&refs, 160, 120).unwrap();
        assert!(m.sigma > 1.5, "sigma {}", m.sigma);
        assert!(m.flat_fraction > 0.5, "{}", m.flat_fraction);
        assert!(strength_for(m.sigma) >= 4);
    }

    #[test]
    fn clean_flat_frames_need_no_grain() {
        let frames = vec![vec![120u8; 160 * 120]; 8];
        let refs: Vec<&[u8]> = frames.iter().map(|f| f.as_slice()).collect();
        let m = measure_window(&refs, 160, 120).unwrap();
        assert!(m.sigma < 0.5, "sigma {}", m.sigma);
        assert_eq!(strength_for(m.sigma), 0);
    }

    #[test]
    fn strength_mapping_matches_the_svt_guidance() {
        assert_eq!(strength_for(0.0), 0);
        assert_eq!(strength_for(0.9), 0);
        assert_eq!(strength_for(1.2), 4);
        assert_eq!(strength_for(2.0), 6);
        assert_eq!(strength_for(3.0), 8);
        assert_eq!(strength_for(5.0), 11);
        assert_eq!(strength_for(9.0), 14);
    }

    #[test]
    fn candidate_picker_requires_texture_and_real_savings() {
        // Baseline 1000; 5% floor = 950.
        assert_eq!(pick_strength(1000, &[(8, 900, true)]), 8);
        assert_eq!(pick_strength(1000, &[(8, 960, true)]), 0, "not enough gain");
        assert_eq!(pick_strength(1000, &[(8, 900, false)]), 0, "texture failed");
        assert_eq!(
            pick_strength(1000, &[(8, 900, true), (12, 880, true)]),
            12,
            "smallest passing file wins"
        );
        assert_eq!(pick_strength(1000, &[(0, 900, true)]), 0);
    }

    #[test]
    fn analysis_windows_cover_short_and_long_sources() {
        assert!(analysis_windows(1.0).is_empty());
        assert_eq!(analysis_windows(2.0).len(), 1);
        assert_eq!(analysis_windows(3600.0).len(), 3);
    }
}
