//! Still-image shrinking: plan, estimate, preflight and encode.
//!
//! Images ride the same Shrink pipeline as videos (probe → preflight →
//! temp encode → verify → place) with a format-specific decision layer:
//!
//! * `jpg/jpeg` → re-encode JPEG (`mjpeg`) at the image quality slider
//! * `webp` (static) → re-encode WebP (`libwebp`), gated on encoder presence
//! * `png`/`bmp`/`tiff` → convert to JPEG when there is no alpha channel
//!   (the big wins: 60–90% off near-uncompressed sources); when the
//!   source carries transparency, convert to WebP instead — the alpha
//!   channel survives (`yuva420p`), which JPEG cannot offer
//! * a same-format encode that can't beat its source (the
//!   already-crushed-JPEG dead end) gets one measured WebP fallback at
//!   the same slider quality — kept only when it really is smaller
//! * animated images (any probe timeline) and formats outside the table
//!   skip with a reason
//!
//! Quality and resolution come from the image-specific settings
//! (`EstParams::image_cq` / `image_scale`, 18–40 CQ scale, lower =
//! better) — separate from the video knobs so an image batch never sees
//! NVENC/audio controls and vice versa. The same min-saving preflight
//! threshold, scale policy and replace/keep-both placement apply as for
//! video. Extra user ffmpeg flags are ignored here — `-crf`-style video
//! options would hard-fail mjpeg/libwebp, and the quality slider already
//! covers the one knob that matters.
//!
//! With `EstParams::image_preserve_format` the decision layer switches
//! to format-preserving plans (see [`plan_image`]): same-format output
//! via measured techniques (opaque alpha-plane drop, 256-color
//! palettes, TIFF deflate), conversions suppressed, and the WebP
//! fallback in [`encode_image_with_fallback`] disabled.

use crate::media::{human_bytes, MediaFile};
use crate::pipeline::{cq_factor, scale_factor, scale_target, Preflight, ScalePolicy};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Still-image extensions Shrink accepts. GIF (animated or already tiny)
/// and AVIF (already efficient, slow to encode) are deliberately absent —
/// the Convert tab covers them.
pub const IMAGE_SHRINK_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff", "ico"];

/// Stills below this size are left alone: a few-KB icon or avatar can't
/// pay for any generation of loss. Formerly 64 KB — with the
/// resolution-preserving techniques (alpha→WebP conversion, the measured
/// WebP fallback for dead-end JPEGs) small files now produce real,
/// verified gains, so the floor only keeps genuinely tiny icons out.
const MIN_IMAGE_BYTES: u64 = 16 * 1024;

/// An image output must beat its source by at least this fraction to be
/// placed. Re-encoding an already-crushed JPEG often lands within a few
/// hundred bytes of the source — identical size for a whole extra
/// generation of loss, so "barely smaller" counts as no gain.
const MIN_IMPROVEMENT: f64 = 0.02;

/// A probe reporting a longer timeline than this is an animation
/// (animated GIF/WebP, multi-frame containers), not a still. Single-frame
/// probes report one frame duration (≤ 0.1 s).
const MAX_STILL_DURATION_S: f64 = 0.5;

/// Hard cap for one image encode — huge TIFFs decode slowly, but a still
/// should never take minutes.
const ENCODE_TIMEOUT_S: u64 = 120;

/// 32x16 JPEG tagged EXIF orientation 6 (displays as 16x32 portrait).
/// Encoded once per process to measure whether this ffmpeg build applies
/// EXIF orientation itself during decode.
const EXIF_PROBE_JPEG: &[u8] = include_bytes!("../assets/exif_probe.jpg");

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

pub fn is_shrinkable_image(path: &Path) -> bool {
    IMAGE_SHRINK_EXTS.contains(&ext_of(path).as_str())
}

/// True for pixel formats carrying an alpha channel (or a palette, which
/// may hide transparency from GIF-sourced PNGs). Decides whether a
/// lossless source can safely become JPEG.
fn pix_fmt_has_alpha(pix: &str) -> bool {
    let p = pix.to_lowercase();
    p == "pal8"
        || p.starts_with("ya")
        || p.starts_with("gbrap")
        || p.contains("alpha")
        || ["rgba", "bgra", "argb", "abgr", "yuva"]
            .iter()
            .any(|k| p.contains(k))
}

/// True for 16-bit sources (RGB48, gray16) — mentioned in skip/plan rows
/// because the JPEG output is 8-bit.
fn pix_fmt_is_16bit(pix: &str) -> bool {
    let p = pix.to_lowercase();
    p.contains("48") || p.contains("64") || p.contains("16")
}

// ── EXIF orientation ─────────────────────────────────────────
//
// Phone photos store sensor-native pixels plus an EXIF orientation tag;
// viewers rotate at display time. Shrinking strips metadata, so the
// rotation must be baked into the pixels. Newer ffmpeg builds do this
// automatically during decode; older ones don't. [`ffmpeg_rotates_jpeg`]
// measures which kind this is, and [`rotation_filters`] supply the
// explicit chain for the latter — never both (that would double-rotate).

static ORIENT_CACHE: OnceLock<Mutex<HashMap<(PathBuf, u64), u16>>> = OnceLock::new();
static ROTATES_JPEG: OnceLock<bool> = OnceLock::new();

/// EXIF orientation of one file (1 = absent/normal). Parsed once per
/// (path, size) per session — plan/estimate call this on every UI render.
fn exif_orientation_cached(path: &Path, len: u64) -> u16 {
    let key = (path.to_path_buf(), len);
    if let Ok(g) = ORIENT_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        if let Some(&o) = g.get(&key) {
            return o;
        }
    }
    let o = exif_orientation(path);
    if let Ok(mut g) = ORIENT_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        g.insert(key, o);
    }
    o
}

/// Parse the orientation tag (0x0112) from a JPEG's EXIF APP1 segment or
/// a TIFF header (TIFF carries the same tag in IFD0). PNG/WebP
/// orientation chunks are rare enough to skip. Returns 1 when absent or
/// unparseable — orientation 1 means "display as stored".
fn exif_orientation(path: &Path) -> u16 {
    const HEAD: usize = 256 * 1024; // APP1/EXIF lives at the file start
    let Ok(mut f) = std::fs::File::open(path) else {
        return 1;
    };
    use std::io::Read;
    let mut head = vec![0u8; HEAD];
    let n = f.read(&mut head).unwrap_or(0);
    head.truncate(n);
    if head.len() < 4 {
        return 1;
    }
    if &head[0..2] == b"II" || &head[0..2] == b"MM" {
        // Bare TIFF: the header itself is the TIFF block.
        return tiff_orientation(&head).unwrap_or(1);
    }
    // JPEG: walk the marker segments to the first EXIF APP1.
    if head[0] != 0xFF || head[1] != 0xD8 {
        return 1;
    }
    let mut i = 2usize;
    while i + 4 <= head.len() {
        if head[i] != 0xFF {
            break;
        }
        let marker = head[i + 1];
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            i += 2; // standalone markers carry no length
            continue;
        }
        let seg_len = ((head[i + 2] as usize) << 8) | head[i + 3] as usize;
        if seg_len < 2 || i + 2 + seg_len > head.len() {
            break;
        }
        if marker == 0xE1 {
            let s = i + 4;
            if s + 6 <= head.len()
                && &head[s..s + 4] == b"Exif"
                && head[s + 4] == 0
                && head[s + 5] == 0
            {
                return tiff_orientation(&head[s + 6..]).unwrap_or(1);
            }
        }
        i += 2 + seg_len;
    }
    1
}

/// Orientation tag (0x0112) in IFD0 of a TIFF block (JPEG APP1 payload or
/// a bare .tif header). Handles both byte orders; SHORT is the type real
/// cameras write, LONG is tolerated.
fn tiff_orientation(b: &[u8]) -> Option<u16> {
    if b.len() < 8 {
        return None;
    }
    let le = match &b[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        if o + 2 > b.len() {
            return None;
        }
        Some(if le {
            u16::from_le_bytes([b[o], b[o + 1]])
        } else {
            u16::from_be_bytes([b[o], b[o + 1]])
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        if o + 4 > b.len() {
            return None;
        }
        let v: [u8; 4] = b[o..o + 4].try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(v)
        } else {
            u32::from_be_bytes(v)
        })
    };
    let ifd = u32_at(4)? as usize;
    let entries = u16_at(ifd)? as usize;
    for k in 0..entries {
        let e = ifd + 2 + k * 12;
        if u16_at(e)? == 0x0112 {
            return match u16_at(e + 2)? {
                3 => u16_at(e + 8), // SHORT: value inline
                4 => Some(u32_at(e + 8)? as u16),
                _ => Some(1),
            };
        }
    }
    Some(1)
}

/// True when this ffmpeg build applies EXIF orientation during decode
/// (FFmpeg ≥ 7.x does). Measured once per process by encoding the bundled
/// orientation-6 probe: rotators output it 16x32, others 32x16. When
/// false, the encode chain carries explicit transpose/flip filters.
fn ffmpeg_rotates_jpeg() -> bool {
    *ROTATES_JPEG.get_or_init(|| detect_jpeg_rotation().unwrap_or(true))
}

fn detect_jpeg_rotation() -> Option<bool> {
    let dir = std::env::temp_dir().join(format!(
        "shrinkr-rot-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let inp = dir.join("probe.jpg");
    std::fs::write(&inp, EXIF_PROBE_JPEG).ok()?;
    let out = dir.join("out.jpg");
    let st = crate::process::cmd("ffmpeg")
        .args(["-y", "-i"])
        .arg(&inp)
        .args(["-map", "0:v:0", "-frames:v", "1", "-c:v", "mjpeg"])
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .ok()?;
    let dims = if st.success() {
        crate::media::probe_file(&out).ok().map(|m| (m.width, m.height))
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&dir);
    match dims {
        Some((16, 32)) => Some(true),
        Some((32, 16)) => Some(false),
        _ => None,
    }
}

/// ffmpeg filter chain that bakes one EXIF orientation into the pixels
/// (applied left to right, before any scale).
fn rotation_filters(o: u16) -> &'static [&'static str] {
    match o {
        2 => &["hflip"],
        3 => &["hflip", "vflip"],
        4 => &["vflip"],
        5 => &["transpose=1", "hflip"],
        6 => &["transpose=1"],
        7 => &["transpose=1", "vflip"],
        8 => &["transpose=2"],
        _ => &[],
    }
}

/// Short label for a non-default EXIF orientation, appended to plan notes.
fn orientation_label(o: u16) -> &'static str {
    match o {
        2 => " +mirror",
        3 => " +rot180",
        4 => " +flip",
        5 => " +transpose",
        6 => " +rot90",
        7 => " +transverse",
        8 => " +rot270",
        _ => "",
    }
}

/// One image's encode decision (from [`plan_image`]).
#[derive(Clone, Debug)]
pub struct ImageEncode {
    /// Output extension, without dot (`"jpg"` / `"webp"`).
    pub target_ext: &'static str,
    /// ffmpeg encoder (`"mjpeg"` / `"libwebp"` / `"png"` / `"tiff"`).
    pub encoder: &'static str,
    /// Short note for logs/plan rows, e.g. `"png→jpeg"`.
    pub note: String,
    /// Conservative output/input byte ratio at the CQ28 reference,
    /// before the CQ curve and scale factor.
    pub base_ratio: f64,
    /// EXIF orientation (1 = none). 90° orientations (5–8) swap the
    /// output's width/height, which the scale policy must account for.
    pub orientation: u16,
    /// True for plain stills (one frame in, one frame out). False for
    /// ICO containers, where every entry stream must survive.
    pub single_frame: bool,
    /// False for formats whose pixel grid is meaningful (ICO entries are
    /// canonical icon sizes) — the scale policy never resizes them.
    pub scalable: bool,
    /// True when the output is built with the palettegen/paletteuse
    /// filter_complex recipe instead of a plain `-vf` chain (PNG
    /// 256-color output). The chain carries rotation/scale inside.
    pub palette: bool,
    /// True when output quality doesn't follow the quality slider
    /// (lossless same-format encodes: PNG/TIFF/ICO re-encodes) — the
    /// estimate then skips the CQ curve instead of pretending the
    /// slider shrinks a bitstream it never touches.
    pub ignore_quality: bool,
    /// Forced output pixel format (`"rgb24"`, `"gray"`), used by
    /// format-preserving plans that drop a constant-opaque alpha plane
    /// or an unused 16-bit depth. `None` lets ffmpeg negotiate.
    pub out_pix: Option<&'static str>,
}

/// Scale target computed on the DISPLAY-oriented dimensions: EXIF
/// orientations 5–8 rotate 90°, so a portrait phone photo stored as
/// 4000x3000 must hit the cap as 1080x1440, not be squashed to
/// 1440x1080. The preset short-side caps are swap-invariant (same answer
/// either way), but a custom WxH box is user-visible geometry, so the
/// policy is evaluated on the displayed image: portrait 3000x4000 in a
/// 1920x1080 box lands 810x1080, not 1080x1440.
fn oriented_scale_target(m: &MediaFile, orientation: u16, scale: ScalePolicy) -> Option<(u32, u32)> {
    if matches!(orientation, 5..=8) {
        let mut display = m.clone();
        std::mem::swap(&mut display.width, &mut display.height);
        scale_target(&display, scale)
    } else {
        scale_target(m, scale)
    }
}

/// Measured pixel facts used by format-preserving PNG plans: whether
/// every alpha sample is 255 (no transparency actually exists) and how
/// many distinct RGB colors the image uses. Both come from one decode
/// pass piped out as raw RGBA, cached per (path, size) like the EXIF
/// orientation. `None` = unknown (decode failed, or the image is too
/// big to pipe cheaply) — plans then stay conservative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PngFacts {
    pub fully_opaque: bool,
    pub distinct_colors: u32,
}

/// Raw-RGBA decode cap: a 30 MP image pipes ~120 MB, already the
/// practical ceiling for a planning-phase probe.
const FACTS_MAX_PIXELS: u64 = 30_000_000;

static FACTS_CACHE: OnceLock<Mutex<HashMap<(PathBuf, u64), Option<PngFacts>>>> = OnceLock::new();

fn png_facts_cached(m: &MediaFile) -> Option<PngFacts> {
    let key = (m.path.clone(), m.bytes);
    let cache = FACTS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(g) = cache.lock() {
        if let Some(&f) = g.get(&key) {
            return f;
        }
    }
    let f = measure_png_facts(m);
    if let Ok(mut g) = cache.lock() {
        g.insert(key, f);
    }
    f
}

/// Decode `m` once through ffmpeg as raw RGBA and measure alpha opacity
/// plus the distinct-color count (2^24 bitset, ~2 MB). Returns `None`
/// when the decode fails, the stream ends early, or the pixel count
/// exceeds [`FACTS_MAX_PIXELS`].
fn measure_png_facts(m: &MediaFile) -> Option<PngFacts> {
    if m.width == 0
        || m.height == 0
        || m.width as u64 * m.height as u64 > FACTS_MAX_PIXELS
    {
        return None;
    }
    use std::io::Read;
    let mut child = crate::process::cmd("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&m.path)
        .args(["-vf", "format=rgba", "-f", "rawvideo", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let expected = m.width as usize * m.height as usize;
    let mut seen = vec![0u8; 1 << 21]; // 2^24 color bits
    let mut opaque = true;
    let mut pixels = 0usize;
    let mut rem: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = out.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        rem.extend_from_slice(&buf[..n]);
        let whole = rem.len() / 4 * 4;
        for px in rem[..whole].chunks_exact(4) {
            if px[3] != 255 {
                opaque = false;
            }
            let idx = ((px[0] as usize) << 16) | ((px[1] as usize) << 8) | px[2] as usize;
            seen[idx >> 3] |= 1 << (idx & 7);
        }
        pixels += whole / 4;
        rem.drain(..whole);
    }
    let status = child.wait().ok()?;
    if !status.success() || pixels != expected || !rem.is_empty() {
        return None;
    }
    let distinct: u32 = seen.iter().map(|b| b.count_ones()).sum();
    Some(PngFacts {
        fully_opaque: opaque,
        distinct_colors: distinct,
    })
}

/// Format-preserving plan for `png`/`bmp`/`tiff` sources: output keeps
/// the source format, choosing the strongest technique the measured
/// pixel facts allow. BMP has no compression at all, so same-format
/// BMP cannot win — it skips with an honest reason instead of growing.
fn preserve_plan(m: &MediaFile, ext: &str) -> Result<ImageEncode, String> {
    const PLAIN: f64 = 0.95; // plain re-encode: wins only on weak writers
    match ext {
        "bmp" => Err(
            "BMP stores pixels raw — same-format output can't shrink (disable format preservation to convert)".into(),
        ),
        "tif" | "tiff" => {
            // Deflate beats the packbits/raw writers most TIFFs ship
            // with; measured ~95% off a raw scan. Lossless, any alpha.
            Ok(ImageEncode {
                target_ext: "tif",
                encoder: "tiff",
                note: "tiff→tiff (deflate)".into(),
                base_ratio: 0.35,
                orientation: 1,
                single_frame: true,
                scalable: true,
                palette: false,
                ignore_quality: true,
                out_pix: None,
            })
        }
        _ => {
            let alpha = pix_fmt_has_alpha(&m.pix_fmt);
            let depth_note = if pix_fmt_is_16bit(&m.pix_fmt) {
                " (16-bit → 8-bit)"
            } else {
                ""
            };
            let facts = png_facts_cached(m);
            if let Some(f) = facts.filter(|f| f.fully_opaque && f.distinct_colors <= 256) {
                // Palette output with dithering off. Not guaranteed
                // bit-exact — palettegen can quantize a few palette
                // entries by ±1 — but measured PSNR is 60+ dB, visually
                // identical for flat-color graphics.
                Ok(ImageEncode {
                    target_ext: "png",
                    encoder: "png",
                    note: format!(
                        "png→png ({} colors)",
                        if f.distinct_colors == 0 { 1 } else { f.distinct_colors }
                    ),
                    base_ratio: 0.65,
                    orientation: 1,
                    single_frame: true,
                    scalable: true,
                    palette: true,
                    ignore_quality: true,
                    out_pix: None,
                })
            } else if facts.is_some_and(|f| f.fully_opaque) {
                if alpha || pix_fmt_is_16bit(&m.pix_fmt) {
                    // Drop the constant-opaque alpha plane (and/or the
                    // unused 16-bit depth): smaller PNG, same pixels.
                    Ok(ImageEncode {
                        target_ext: "png",
                        encoder: "png",
                        note: if alpha {
                            format!("png→png (alpha dropped){depth_note}")
                        } else {
                            format!("png→png{depth_note}")
                        },
                        base_ratio: if alpha { 0.80 } else { 0.55 },
                        orientation: 1,
                        single_frame: true,
                        scalable: true,
                        palette: false,
                        ignore_quality: true,
                        out_pix: Some("rgb24"),
                    })
                } else {
                    Ok(ImageEncode {
                        target_ext: "png",
                        encoder: "png",
                        note: "png→png".into(),
                        base_ratio: PLAIN,
                        orientation: 1,
                        single_frame: true,
                        scalable: true,
                        palette: false,
                        ignore_quality: true,
                        out_pix: None,
                    })
                }
            } else {
                // Real transparency (or facts unknown): keep the alpha
                // channel and re-encode — WebP conversion is off in this
                // mode, so the no-gain guard decides honestly.
                let note = if alpha {
                    format!("png→png (alpha kept)")
                } else if pix_fmt_is_16bit(&m.pix_fmt) {
                    // gray16 → 8-bit gray; still format-preserving.
                    format!("png→png (16-bit → 8-bit)")
                } else {
                    "png→png".into()
                };
                let (base, out_pix) = if pix_fmt_is_16bit(&m.pix_fmt) && !alpha {
                    (0.55, Some("gray"))
                } else {
                    (PLAIN, None)
                };
                Ok(ImageEncode {
                    target_ext: "png",
                    encoder: "png",
                    note,
                    base_ratio: base,
                    orientation: 1,
                    single_frame: true,
                    scalable: true,
                    palette: false,
                    ignore_quality: true,
                    out_pix,
                })
            }
        }
    }
}

/// Decide what Shrink does with one still image. `Err` carries the
/// skip reason (surfaced in plan rows and preflight).
///
/// `preserve` selects the format-preserving mode: outputs keep the
/// source format (png→png, tiff→tiff) using lossless or near-lossless
/// techniques, and conversions are suppressed. The techniques are chosen
/// from measured pixel facts (decoded once, cached per file):
///
/// * a fully-opaque alpha plane is dropped (`rgba`→`rgb24`) — no
///   transparency existed, so nothing is lost
/// * images with ≤ 256 distinct colors re-encode as palette PNG with
///   dithering off — near-lossless (a few palette entries may quantize
///   by ±1, measured PSNR 60+ dB), often 50–80% off
/// * a 16-bit depth drops to 8-bit (disclosed in the note)
/// * everything else is a plain same-format re-encode, which only wins
///   on sources written with weak compression — the estimate predicts
///   ~5%, so most skip at the default threshold honestly
pub fn plan_image(m: &MediaFile, preserve: bool) -> Result<ImageEncode, String> {
    // ICO entries are resolution variants, not animation frames — a
    // multi-size icon reports a longer timeline but is still a still.
    let ext = ext_of(&m.path);
    if ext != "ico" && m.duration_s > MAX_STILL_DURATION_S {
        return Err("animated image — Shrink handles stills only".into());
    }
    let orientation = exif_orientation_cached(&m.path, m.bytes);
    let mut plan = {
        let depth_note = if pix_fmt_is_16bit(&m.pix_fmt) {
            " (16-bit → 8-bit)"
        } else {
            ""
        };
        match ext.as_str() {
            "jpg" | "jpeg" => ImageEncode {
                target_ext: "jpg",
                encoder: "mjpeg",
                note: "re-encode jpeg".into(),
                base_ratio: 0.75,
                orientation: 1,
                single_frame: true,
                scalable: true,
                palette: false,
                ignore_quality: false,
                out_pix: None,
            },
            "webp" => {
                // WebP keeps alpha on re-encode, so no transparency check.
                if !crate::convert::caps().has_encoder("libwebp") {
                    return Err(
                        "this ffmpeg build lacks libwebp — webp cannot be re-encoded".into()
                    );
                }
                ImageEncode {
                    target_ext: "webp",
                    encoder: "libwebp",
                    note: "re-encode webp".into(),
                    base_ratio: 0.70,
                    orientation: 1,
                    single_frame: true,
                    scalable: true,
                    palette: false,
                    ignore_quality: false,
                    out_pix: None,
                }
            }
            "png" | "bmp" | "tif" | "tiff" => {
                if preserve {
                    return preserve_plan(m, ext.as_str());
                }
                if pix_fmt_has_alpha(&m.pix_fmt) {
                    // JPEG would lose the alpha channel, but WebP keeps it
                    // (lossy RGB, lossless alpha via yuva420p) — so alpha
                    // sources convert to WebP instead of skipping. When
                    // this build lacks libwebp there is no alpha-safe
                    // output left, and the old skip applies.
                    if !crate::convert::caps().has_encoder("libwebp") {
                        return Err(
                            "has transparency and this ffmpeg build lacks libwebp — JPEG output would lose the alpha channel".into(),
                        );
                    }
                    let note = match ext.as_str() {
                        "png" => format!("png→webp (alpha kept){depth_note}"),
                        "bmp" => "bmp→webp (alpha kept)".to_string(),
                        _ => format!("tiff→webp (alpha kept){depth_note}"),
                    };
                    ImageEncode {
                        target_ext: "webp",
                        encoder: "libwebp",
                        note,
                        base_ratio: 0.35,
                        orientation: 1,
                        single_frame: true,
                        scalable: true,
                        palette: false,
                        ignore_quality: false,
                        out_pix: None,
                    }
                } else {
                    let note = match ext.as_str() {
                        "png" => format!("png→jpeg{depth_note}"),
                        "bmp" => "bmp→jpeg".to_string(),
                        _ => format!("tiff→jpeg{depth_note}"),
                    };
                    let base = match ext.as_str() {
                        "png" => 0.30,
                        "bmp" => 0.12,
                        _ => 0.20,
                    };
                    ImageEncode {
                        target_ext: "jpg",
                        encoder: "mjpeg",
                        note,
                        base_ratio: base,
                        orientation: 1,
                        single_frame: true,
                        scalable: true,
                        palette: false,
                        ignore_quality: false,
                        out_pix: None,
                    }
                }
            }
            "ico" => {
                // Every entry re-encodes to PNG inside the ICO: lossless,
                // alpha kept, all sizes kept. BMP-encoded entries (legacy
                // icons) shrink hard; PNG-entry icons land ~same size and
                // are caught by the no-gain guard.
                if !crate::convert::caps().has_muxer("ico") {
                    return Err("this ffmpeg build cannot write .ico".into());
                }
                ImageEncode {
                    target_ext: "ico",
                    encoder: "png",
                    note: "ico→png entries".into(),
                    base_ratio: 0.55,
                    orientation: 1,
                    single_frame: false,
                    scalable: false,
                    palette: false,
                    // Lossless entries — the quality slider never applies.
                    ignore_quality: true,
                    out_pix: None,
                }
            }
            other => {
                return Err(format!(".{other} images aren't shrinkable (try Convert)"));
            }
        }
    };
    plan.orientation = orientation;
    plan.note.push_str(orientation_label(orientation));
    Ok(plan)
}

/// CQ slider (18–40, lower = better) → mjpeg `-q:v` (2–18, lower =
/// better). Same direction as CQ so the shared slider reads naturally;
/// cq28 lands on q9 ≈ JPEG quality 88.
pub fn jpeg_q_for(cq: u32) -> u32 {
    (2.0 + (cq.clamp(18, 40) as f64 - 18.0) * (16.0 / 22.0)).round() as u32
}

/// CQ slider → libwebp `-quality` (higher = better, clamped 60–95).
pub fn webp_quality_for(cq: u32) -> u32 {
    (100.0 - (cq.clamp(18, 40) as f64 - 18.0) * 1.4).round().clamp(60.0, 95.0) as u32
}
/// Conservative output/input byte ratio for one image encode, mirroring
/// the video estimator's shape: format base × CQ curve × resolution.
/// Under-promises on purpose; the post-encode no-gain guard catches the
/// rest (re-encoding a low-quality JPEG can grow). Non-scalable formats
/// (ICO) are never resized, so the resolution factor doesn't apply.
/// Plans with [`ImageEncode::ignore_quality`] (lossless same-format
/// encodes) skip the CQ curve — the slider doesn't touch their bits.
pub fn estimate_image_ratio(m: &MediaFile, plan: &ImageEncode, cq: u32, scale: ScalePolicy) -> f64 {
    let sf = if plan.scalable {
        scale_factor(m, scale)
    } else {
        1.0
    };
    let cf = if plan.ignore_quality {
        1.0
    } else {
        cq_factor(cq)
    };
    plan.base_ratio * cf * sf
}

/// Estimated output bytes for one image (skipped images count as
/// unchanged). Used by the GUI size/time estimate.
pub fn estimate_image_bytes(m: &MediaFile, cq: u32, scale: ScalePolicy, preserve: bool) -> u64 {
    match plan_image(m, preserve) {
        Ok(plan) => (m.bytes as f64 * estimate_image_ratio(m, &plan, cq, scale)) as u64,
        Err(_) => m.bytes,
    }
}

/// Image preflight with the same shape and threshold semantics as the
/// video [`crate::pipeline::preflight`].
pub fn preflight_image(
    m: &MediaFile,
    cq: u32,
    scale: ScalePolicy,
    preserve: bool,
    min_saving_pct: f64,
) -> Preflight {
    if m.bytes < MIN_IMAGE_BYTES {
        return Preflight::Skip {
            reason: format!("already small ({})", human_bytes(m.bytes)),
        };
    }
    let plan = match plan_image(m, preserve) {
        Ok(p) => p,
        Err(reason) => {
            return Preflight::Skip { reason };
        }
    };
    let ratio = estimate_image_ratio(m, &plan, cq, scale);
    let new_bytes = (m.bytes as f64 * ratio) as u64;
    let saving = (1.0 - ratio) * 100.0;
    // Images use the plain short-side cap on display-oriented dims (no
    // even-dimension shave — JPEG/WebP accept odd sizes, and a needless
    // 1px resample costs quality for nothing). ICOs never resize: their
    // entry sizes are meaningful.
    let res_note = if plan.scalable {
        match oriented_scale_target(m, plan.orientation, scale) {
            Some((w, h)) => format!("{}→{}x{}", m.res_label(), w, h),
            None => m.res_label(),
        }
    } else if m.video_stream_count > 1 {
        // ICO entry streams — the probed width/height is just the first
        // (smallest) entry, so report the entry count instead.
        format!("{} entries", m.video_stream_count)
    } else {
        m.res_label()
    };
    if saving < min_saving_pct {
        Preflight::Skip {
            reason: format!(
                "est. {} → {} ({:.0}% < {:.0}% threshold; {}, {})",
                human_bytes(m.bytes),
                human_bytes(new_bytes),
                saving,
                min_saving_pct,
                plan.note,
                res_note
            ),
        }
    } else {
        Preflight::Shrink {
            reason: format!(
                "est. {} → {} ({:.0}% off; {}, {})",
                human_bytes(m.bytes),
                human_bytes(new_bytes),
                saving,
                plan.note,
                res_note
            ),
        }
    }
}

/// Exact ffmpeg arguments for one still-image encode: single frame,
/// metadata stripped, EXIF rotation baked in when this build won't do it
/// itself, and an optional downscale computed on display-oriented dims.
pub fn build_image_args(
    m: &MediaFile,
    plan: &ImageEncode,
    cq: u32,
    scale: ScalePolicy,
    out_path: &Path,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-i".into(),
        m.path.to_string_lossy().into_owned(),
    ];
    let mut filters: Vec<String> = vec![];
    if plan.orientation >= 2 && !ffmpeg_rotates_jpeg() {
        filters.extend(rotation_filters(plan.orientation).iter().map(|s| s.to_string()));
    }
    if plan.scalable {
        if let Some((w, h)) = oriented_scale_target(m, plan.orientation, scale) {
            filters.push(format!("scale={w}:{h}"));
        }
    }
    if plan.palette {
        // Palette output needs the palettegen/paletteuse recipe over a
        // split graph; rotation/scale fold into the chain head. Dithering
        // stays off so pixels keep their colors (palette entries may
        // quantize by ±1 — near-lossless), and no transparency slot is
        // reserved (palette plans are opaque).
        let head = if filters.is_empty() {
            String::new()
        } else {
            format!("{},", filters.join(","))
        };
        a.push("-filter_complex".into());
        a.push(format!(
            "{head}split[a][b];[a]palettegen=reserve_transparent=0[p];[b][p]paletteuse=dither=none[out]"
        ));
        a.push("-map".into());
        a.push("[out]".into());
        a.push("-frames:v".into());
        a.push("1".into());
    } else {
        if !filters.is_empty() {
            a.push("-vf".into());
            a.push(filters.join(","));
        }
        // ICO containers hold one stream per entry — map them all and keep
        // every frame; plain stills take just the primary video stream.
        a.push("-map".into());
        a.push(if plan.single_frame { "0:v:0" } else { "0:v" }.into());
        if plan.single_frame {
            a.push("-frames:v".into());
            a.push("1".into());
        }
    }
    if let Some(pix) = plan.out_pix {
        a.push("-pix_fmt".into());
        a.push(pix.into());
    }
    a.push("-map_metadata".into());
    a.push("-1".into());
    a.push("-an".into());
    a.push("-sn".into());
    a.push("-dn".into());
    match plan.encoder {
        "libwebp" => {
            a.push("-c:v".into());
            a.push("libwebp".into());
            a.push("-quality".into());
            a.push(webp_quality_for(cq).to_string());
        }
        "png" => {
            // Lossless — the ICO muxer wraps each entry stream as-is.
            a.push("-c:v".into());
            a.push("png".into());
        }
        "tiff" => {
            // Lossless with real compression: deflate beats the
            // packbits/raw writers most TIFF files ship with.
            a.push("-c:v".into());
            a.push("tiff".into());
            a.push("-compression_algo".into());
            a.push("deflate".into());
        }
        _ => {
            a.push("-c:v".into());
            a.push("mjpeg".into());
            a.push("-q:v".into());
            a.push(jpeg_q_for(cq).to_string());
        }
    }
    a.push(out_path.to_string_lossy().into_owned());
    a
}

/// Verified per-image metrics (the image counterpart of the video
/// [`crate::ffmpeg::EncodeResult`]).
#[derive(Clone, Debug)]
pub struct ImageResult {
    pub encoder: String,
    pub quality: String,
    pub output_bytes: u64,
    pub elapsed_s: f64,
    pub ratio: f64,
}

/// One image encode's outcome. `NoGain` is a valid encode that didn't
/// beat its source by [`MIN_IMPROVEMENT`] — growing files and
/// same-size re-encodes (a wasted JPEG generation) both land here.
/// Callers delete the temp output and keep the original.
#[derive(Clone, Debug)]
pub enum ImageOutcome {
    Done(ImageResult),
    NoGain { output_bytes: u64 },
}

fn quality_label(plan: &ImageEncode, cq: u32) -> String {
    match plan.encoder {
        "libwebp" => format!("q{}", webp_quality_for(cq)),
        "png" if plan.palette => "256c palette".into(),
        "png" => "lossless".into(),
        "tiff" => "deflate".into(),
        _ => format!("q:v{}", jpeg_q_for(cq)),
    }
}

/// Encode one still image to `out_path` (temp), then verify: size floor,
/// probe must find an image stream, and the output must actually be
/// smaller than the source. Encodes are sub-second, so the wait loop is
/// a cancellation check rather than a progress pipe.
pub fn encode_image(
    m: &MediaFile,
    plan: &ImageEncode,
    cq: u32,
    scale: ScalePolicy,
    out_path: &Path,
    cancel: &AtomicBool,
) -> Result<ImageOutcome, String> {
    let args = build_image_args(m, plan, cq, scale, out_path);
    crate::ffmpeg::log::debug_log(&format!(
        "[image {}] {}",
        m.path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?"),
        crate::ffmpeg::command_line(&args)
    ));
    let _ = std::fs::remove_file(out_path);
    let t0 = Instant::now();
    let mut child = crate::process::cmd("ffmpeg")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn ffmpeg ({}): {e}", plan.encoder))?;
    // stderr tail for the error message (ffmpeg prints little for stills,
    // but the pipe must be drained so it can never block).
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let tail2 = stderr_tail.clone();
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut r = err;
            let _ = r.read_to_end(&mut buf);
            let s = String::from_utf8_lossy(&buf);
            let tail: String = if s.len() > 2000 {
                s[s.len() - 2000..].to_string()
            } else {
                s.into_owned()
            };
            if let Ok(mut g) = tail2.lock() {
                *g = tail;
            }
        });
    }
    let deadline = t0 + Duration::from_secs(ENCODE_TIMEOUT_S);
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait ffmpeg: {e}"))? {
            Some(st) => break st,
            None => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(out_path);
                    return Err("cancelled".into());
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(out_path);
                    return Err(format!(
                        "image encode timed out ({}s) — file may be pathological",
                        ENCODE_TIMEOUT_S
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(out_path);
        let tail = stderr_tail.lock().map(|g| g.clone()).unwrap_or_default();
        let last = tail
            .lines()
            .last()
            .unwrap_or("ffmpeg failed")
            .to_string();
        return Err(format!(
            "ffmpeg ({}) failed: {}",
            plan.encoder,
            last.chars().take(300).collect::<String>()
        ));
    }
    let out_bytes = std::fs::metadata(out_path).map(|x| x.len()).unwrap_or(0);
    if out_bytes < 256 {
        let _ = std::fs::remove_file(out_path);
        return Err(format!(
            "output too small ({out_bytes} bytes) — likely failed encode"
        ));
    }
    let vf = crate::media::probe_file(out_path).map_err(|e| {
        let _ = std::fs::remove_file(out_path);
        format!("verify probe: {e:#}")
    })?;
    if vf.video_stream_count == 0 {
        let _ = std::fs::remove_file(out_path);
        return Err("output has no image stream — encode failed".into());
    }
    let elapsed = t0.elapsed().as_secs_f64().max(0.01);
    if out_bytes as f64 > m.bytes as f64 * (1.0 - MIN_IMPROVEMENT) {
        return Ok(ImageOutcome::NoGain { output_bytes: out_bytes });
    }
    Ok(ImageOutcome::Done(ImageResult {
        encoder: plan.encoder.into(),
        quality: quality_label(plan, cq),
        output_bytes: out_bytes,
        elapsed_s: elapsed,
        ratio: out_bytes as f64 / m.bytes.max(1) as f64,
    }))
}

/// [`encode_image`] plus one advanced retry: when the same-format encode
/// lands [`ImageOutcome::NoGain`] — the classic already-crushed-JPEG dead
/// end, where another mjpeg generation lands within a few percent — the
/// same pixels are attempted through libwebp at the same slider quality.
/// WebP quantizes differently from JPEG, so it often still finds 5–15%
/// where JPEG has plateaued. The fallback only fires for `mjpeg` plans
/// (alpha sources plan WebP directly, and WebP sources have nothing to
/// fall back to), and the WebP output is kept only when it beats the
/// SOURCE by [`MIN_IMPROVEMENT`] — [`encode_image`]'s own guard. On
/// success the plan is switched to the WebP encoder so callers place the
/// right extension, and the returned path is the temp file that actually
/// holds the winning encode (== `out_path` when no fallback fired).
/// A failed fallback is not a job failure: the original no-gain verdict
/// stands (cancellation still propagates).
///
/// `preserve_format` disables the fallback entirely: the user asked for
/// the source format, so a no-gain encode skips instead of converting.
pub fn encode_image_with_fallback(
    m: &MediaFile,
    plan: &mut ImageEncode,
    cq: u32,
    scale: ScalePolicy,
    out_path: &Path,
    cancel: &AtomicBool,
    preserve_format: bool,
) -> Result<(PathBuf, ImageOutcome), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let outcome = encode_image(m, plan, cq, scale, out_path, cancel)?;
    let ImageOutcome::NoGain { .. } = outcome else {
        return Ok((out_path.to_path_buf(), outcome));
    };
    if preserve_format || plan.encoder != "mjpeg" || !crate::convert::caps().has_encoder("libwebp")
    {
        return Ok((out_path.to_path_buf(), outcome));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let mut wp = plan.clone();
    wp.target_ext = "webp";
    wp.encoder = "libwebp";
    wp.note.push_str(" →webp");
    // libwebp writes via the image2 muxer, which picks the container from
    // the output extension — the attempt needs a real .webp temp file.
    let webp_path = out_path.with_extension("webp");
    let _ = std::fs::remove_file(&webp_path);
    match encode_image(m, &wp, cq, scale, &webp_path, cancel) {
        Ok(ImageOutcome::Done(res)) => {
            let _ = std::fs::remove_file(out_path);
            *plan = wp;
            Ok((webp_path, ImageOutcome::Done(res)))
        }
        Ok(ImageOutcome::NoGain { output_bytes }) => {
            let _ = std::fs::remove_file(&webp_path);
            Ok((out_path.to_path_buf(), ImageOutcome::NoGain { output_bytes }))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&webp_path);
            if cancel.load(Ordering::Relaxed) {
                return Err(e);
            }
            Ok((out_path.to_path_buf(), outcome))
        }
    }
}

/// Collect shrinkable inputs (videos + still images) under `arg` (file
/// or recursive dir) for the CLI `--shrink` path. Benchmarking keeps its
/// video-only collector.
pub fn collect_shrink_inputs(arg: &Path) -> Vec<PathBuf> {
    if arg.is_file() {
        return vec![arg.to_path_buf()];
    }
    let mut hits = vec![];
    for e in walkdir::WalkDir::new(arg).into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        if let Some(ext) = e.path().extension().and_then(|s| s.to_str()) {
            let l = ext.to_lowercase();
            if crate::media::MEDIA_EXTS.contains(&l.as_str())
                || IMAGE_SHRINK_EXTS.contains(&l.as_str())
            {
                hits.push(e.path().to_path_buf());
            }
        }
    }
    hits.sort();
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::cq_factor;

    fn img(name: &str, pix: &str, w: u32, h: u32, bytes: u64) -> MediaFile {
        MediaFile {
            path: PathBuf::from(name),
            bytes,
            vcodec: "mjpeg".into(),
            width: w,
            height: h,
            fps: 25.0,
            pix_fmt: pix.into(),
            bit_depth: 8,
            color_space: String::new(),
            color_primaries: String::new(),
            color_transfer: String::new(),
            color_range: String::new(),
            vbitrate: None,
            format_bitrate: None,
            duration_s: 0.0,
            audio: vec![],
            acodec: "none".into(),
            video_stream_count: 1,
            audio_stream_count: 0,
            sub_count: 0,
            attach_count: 0,
            has_chapters: false,
        }
    }

    #[test]
    fn plan_per_format() {
        // Same-format re-encodes.
        let jpg = img("photo.jpg", "yuvj420p", 4000, 3000, 4_000_000);
        let p = plan_image(&jpg, false).unwrap();
        assert_eq!(p.target_ext, "jpg");
        assert_eq!(p.encoder, "mjpeg");
        // png without alpha converts to jpeg.
        let png = img("shot.png", "rgb24", 1920, 1080, 3_000_000);
        let p = plan_image(&png, false).unwrap();
        assert_eq!(p.target_ext, "jpg");
        assert!(p.note.contains("png→jpeg"), "{}", p.note);
        // bmp and tiff convert to jpeg with the format in the note.
        assert_eq!(plan_image(&img("s.bmp", "rgb24", 800, 600, 1_000_000), false).unwrap().note, "bmp→jpeg");
        assert!(plan_image(&img("s.tiff", "rgb24", 800, 600, 1_000_000), false)
            .unwrap()
            .note
            .starts_with("tiff→jpeg"));
        assert_eq!(plan_image(&img("s.tif", "rgb24", 800, 600, 1_000_000), false).unwrap().target_ext, "jpg");
    }

    #[test]
    fn plan_skips_alpha_and_animation() {
        // Alpha sources plan WebP when this build can encode it (the
        // alpha channel is kept) and skip with the transparency reason
        // otherwise — they never silently land on a JPEG.
        let webp_ok = crate::convert::caps().has_encoder("libwebp");
        for pix in ["rgba", "pal8", "yuva420p", "ya8"] {
            let m = img("alpha.png", pix, 800, 600, 1_000_000);
            match plan_image(&m, false) {
                Ok(p) => {
                    assert!(webp_ok, "{pix}: planned webp but libwebp expected missing");
                    assert_eq!(p.target_ext, "webp");
                    assert!(p.note.contains("alpha"), "{pix}: {}", p.note);
                }
                Err(e) => {
                    assert!(!webp_ok, "{pix}: skipped despite libwebp: {e}");
                    assert!(e.contains("transparency"), "{pix}: {e}");
                }
            }
        }
        // Animated probes skip regardless of format.
        let mut gif_ish = img("anim.webp", "yuv420p", 480, 480, 2_000_000);
        gif_ish.duration_s = 3.0;
        let e = plan_image(&gif_ish, false).unwrap_err();
        assert!(e.contains("animated"), "{e}");
        // Single-frame probe durations (≤0.1s) stay stills.
        let mut still = img("still.jpg", "yuvj420p", 800, 600, 1_000_000);
        still.duration_s = 0.04;
        assert!(plan_image(&still, false).is_ok());
        // Unsupported image formats skip with a reason.
        assert!(plan_image(&img("f.avif", "yuv420p", 800, 600, 100_000), false)
            .unwrap_err()
            .contains("shrinkable"));
    }

    #[test]
    fn preserve_mode_keeps_the_source_format() {
        // JPEG/WebP plans are already same-format; PNG under preserve
        // mode must produce a png→png plan. Facts for a fake path are
        // None (no real file), so the conservative alpha-kept/plain
        // branch applies — conversion plans (jpeg/webp) never appear.
        let jpg = img("p.jpg", "yuvj420p", 800, 600, 1_000_000);
        let p = plan_image(&jpg, true).unwrap();
        assert_eq!(p.target_ext, "jpg");
        // Opaque rgb24 png: plain lossless re-encode prediction (~5%).
        let png = img("shot.png", "rgb24", 1920, 1080, 3_000_000);
        let p = plan_image(&png, true).unwrap();
        assert_eq!(p.target_ext, "png");
        assert_eq!(p.encoder, "png");
        assert_eq!(p.note, "png→png");
        assert!(p.ignore_quality, "lossless output ignores the slider");
        // No conversion fallback in preserve mode: mjpeg-only plans fall
        // back to webp, png plans have nowhere to convert.
        assert!(!p.palette);
        // BMP can't shrink as BMP — honest skip instead of a conversion.
        let bmp = img("s.bmp", "rgb24", 800, 600, 1_000_000);
        let e = plan_image(&bmp, true).unwrap_err();
        assert!(e.contains("BMP"), "{e}");
        // TIFF keeps its format via deflate compression.
        let tif = img("s.tiff", "rgb24", 800, 600, 4_000_000);
        let p = plan_image(&tif, true).unwrap();
        assert_eq!(p.target_ext, "tif");
        assert_eq!(p.encoder, "tiff");
        assert!(p.note.contains("deflate"), "{}", p.note);
        assert!(p.ignore_quality);
        // TIFF args carry the deflate option.
        let args = build_image_args(&tif, &p, 28, ScalePolicy::Preserve, Path::new("o.tif"));
        assert!(args.join(" ").contains("-compression_algo deflate"));
    }

    #[test]
    fn preserve_estimates_ignore_the_quality_slider() {
        let png = img("shot.png", "rgb24", 1920, 1080, 3_000_000);
        let p = plan_image(&png, true).unwrap();
        let r18 = estimate_image_ratio(&png, &p, 18, ScalePolicy::Preserve);
        let r40 = estimate_image_ratio(&png, &p, 40, ScalePolicy::Preserve);
        assert!((r18 - r40).abs() < 1e-9, "lossless plans ignore cq: {r18} vs {r40}");
        // A jpeg plan (slider-driven) does move with cq.
        let jpg = img("p.jpg", "yuvj420p", 800, 600, 1_000_000);
        let jp = plan_image(&jpg, false).unwrap();
        let j18 = estimate_image_ratio(&jpg, &jp, 18, ScalePolicy::Preserve);
        let j40 = estimate_image_ratio(&jpg, &jp, 40, ScalePolicy::Preserve);
        assert!(j40 < j18);
    }

    #[test]
    fn palette_and_alpha_drop_args() {
        // Hand-built palette plan: args use the filter_complex recipe,
        // not -vf, and map the [out] label.
        let mut pal = img("shot.png", "rgb24", 1920, 1080, 3_000_000);
        pal.path = PathBuf::from("shot.png");
        let plan = ImageEncode {
            target_ext: "png",
            encoder: "png",
            note: "png→png (8 colors)".into(),
            base_ratio: 0.65,
            orientation: 1,
            single_frame: true,
            scalable: true,
            palette: true,
            ignore_quality: true,
            out_pix: None,
        };
        let args = build_image_args(&pal, &plan, 28, ScalePolicy::Preserve, Path::new("o.png"));
        let joined = args.join(" ");
        assert!(joined.contains("-filter_complex"), "{joined}");
        assert!(joined.contains("palettegen=reserve_transparent=0"), "{joined}");
        assert!(joined.contains("paletteuse=dither=none"), "{joined}");
        assert!(joined.contains("-map [out]"), "{joined}");
        assert!(!joined.contains("-vf"), "{joined}");
        // Scale folds into the chain head when the policy asks.
        let args = build_image_args(&pal, &plan, 28, ScalePolicy::Force480p, Path::new("o.png"));
        let joined = args.join(" ");
        assert!(joined.contains("scale=854:480,split[a][b]"), "{joined}");
        // Alpha-drop plan: -pix_fmt rgb24 replaces the alpha plane.
        let drop = ImageEncode {
            target_ext: "png",
            encoder: "png",
            note: "png→png (alpha dropped)".into(),
            base_ratio: 0.80,
            orientation: 1,
            single_frame: true,
            scalable: true,
            palette: false,
            ignore_quality: true,
            out_pix: Some("rgb24"),
        };
        let args = build_image_args(&pal, &drop, 28, ScalePolicy::Preserve, Path::new("o.png"));
        assert!(args.join(" ").contains("-pix_fmt rgb24"));
    }

    #[test]
    fn measured_facts_drive_real_png_plans() {
        // Needs a real file: encode a flat 2-color PNG via ffmpeg, then
        // the preserve plan must pick the palette technique.
        let d = std::env::temp_dir().join(format!(
            "shrinkr-facts-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let png = d.join("flat.png");
        let st = crate::process::cmd("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("color=c=red:s=96x64,drawbox=x=10:y=10:w=30:h=30:color=navy:t=fill")
            .args(["-frames:v", "1", "-pix_fmt", "rgb24"])
            .arg(&png)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .status();
        if st.map(|s| s.success()).unwrap_or(false) {
            let bytes = std::fs::metadata(&png).unwrap().len();
            let m = img("flat.png", "rgb24", 96, 64, bytes);
            let m = MediaFile {
                path: png.clone(),
                ..m
            };
            let facts = png_facts_cached(&m).expect("facts for a real png");
            assert!(facts.fully_opaque);
            assert_eq!(facts.distinct_colors, 2, "got {:?}", facts);
            let p = plan_image(&m, true).unwrap();
            assert!(p.palette, "{}", p.note);
            assert!(p.note.contains("2 colors"), "{}", p.note);
            // Facts are cached: second call hits the same entry.
            assert_eq!(png_facts_cached(&m), Some(facts));
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn alpha_png_plans_webp_with_alpha_note() {
        if !crate::convert::caps().has_encoder("libwebp") {
            return; // transparency-skip branch covered in the other test
        }
        let png = img("shot.png", "rgba", 1920, 1080, 3_000_000);
        let p = plan_image(&png, false).unwrap();
        assert_eq!(p.target_ext, "webp");
        assert_eq!(p.encoder, "libwebp");
        assert!(p.note.contains("alpha kept"), "{}", p.note);
        // A format conversion, so the base sits between png→jpeg (0.30)
        // and webp→webp (0.70) — a real predicted saving, not a freebie.
        assert!(p.base_ratio > 0.30 && p.base_ratio < 0.70);
        // Args stay webp-shaped (encoder + mapped quality, no q:v).
        let args = build_image_args(&png, &p, 28, ScalePolicy::Preserve, Path::new("o.webp"));
        let joined = args.join(" ");
        assert!(joined.contains("-c:v libwebp -quality 86"), "{joined}");
        assert!(!joined.contains("q:v"), "{joined}");
    }

    #[test]
    fn quality_mapping_is_monotonic_and_bounded() {
        assert_eq!(jpeg_q_for(18), 2);
        assert_eq!(jpeg_q_for(28), 9);
        assert_eq!(jpeg_q_for(40), 18);
        for cq in 18..40 {
            assert!(jpeg_q_for(cq + 1) >= jpeg_q_for(cq));
            assert!(webp_quality_for(cq + 1) <= webp_quality_for(cq));
        }
        assert_eq!(webp_quality_for(28), 86);
        assert!(webp_quality_for(18) <= 95);
    }

    #[test]
    fn estimates_order_formats_sensibly() {
        let cq = 28u32;
        let scale = ScalePolicy::Preserve;
        let jpg = img("p.jpg", "yuvj420p", 4000, 3000, 4_000_000);
        let png = img("p.png", "rgb24", 4000, 3000, 12_000_000);
        let pjpg = plan_image(&jpg, false).unwrap();
        let ppng = plan_image(&png, false).unwrap();
        let r_jpg = estimate_image_ratio(&jpg, &pjpg, cq, scale);
        let r_png = estimate_image_ratio(&png, &ppng, cq, scale);
        assert!(r_jpg < 1.0, "jpg re-encode must predict a saving");
        assert!(r_png < r_jpg, "png→jpeg ({r_png}) must beat jpg→jpg ({r_jpg})");
        // Higher CQ (worse quality) predicts a smaller file.
        let r40 = estimate_image_ratio(&jpg, &pjpg, 40, scale);
        assert!(r40 < r_jpg);
        // Max quality predicts growth → preflight skips.
        assert!(estimate_image_ratio(&jpg, &pjpg, 18, scale) > 1.0);
    }

    #[test]
    fn preflight_threshold_and_scale_flow() {
        let mut m = img("big.png", "rgb24", 4000, 3000, 12_000_000);
        match preflight_image(&m, 28, ScalePolicy::Preserve, false, 10.0) {
            Preflight::Shrink { reason } => {
                assert!(reason.contains("png→jpeg"), "{reason}");
                assert!(reason.contains("4000x3000"), "{reason}");
            }
            Preflight::Skip { reason } => panic!("70% off should shrink: {reason}"),
        }
        // Downscale appears in the reason and lowers the estimate.
        match preflight_image(&m, 28, ScalePolicy::Force1080p, false, 10.0) {
            Preflight::Shrink { reason } => assert!(reason.contains("1440x1080"), "{reason}"),
            Preflight::Skip { reason } => panic!("should shrink: {reason}"),
        }
        // JPEG→JPEG at max quality predicts growth (0.75 × 2.0) → skip
        // with the threshold reason. Conversions (png→jpeg) don't hit
        // this — even at q2 they legitimately predict a saving.
        let jpg = img("big.jpg", "yuvj420p", 4000, 3000, 12_000_000);
        match preflight_image(&jpg, 18, ScalePolicy::Preserve, false, 10.0) {
            Preflight::Skip { reason } => assert!(reason.contains("threshold"), "{reason}"),
            Preflight::Shrink { reason } => panic!("q2 re-encode must skip: {reason}"),
        }
        // Tiny files skip outright.
        m.bytes = 1_000;
        match preflight_image(&m, 28, ScalePolicy::Preserve, false, 0.0) {
            Preflight::Skip { reason } => assert!(reason.contains("already small"), "{reason}"),
            Preflight::Shrink { .. } => panic!("16 KB floor must hold"),
        }
    }

    #[test]
    fn args_carry_frame_quality_and_scale() {
        let m = img("p.jpg", "yuvj420p", 4000, 3000, 4_000_000);
        let plan = plan_image(&m, false).unwrap();
        let args = build_image_args(&m, &plan, 28, ScalePolicy::Preserve, Path::new("out.jpg"));
        let joined = args.join(" ");
        assert!(joined.contains("-frames:v 1"), "{joined}");
        assert!(joined.contains("-c:v mjpeg -q:v 9"), "{joined}");
        assert!(joined.contains("-map_metadata -1"), "{joined}");
        assert!(!joined.contains("scale="), "{joined}");
        assert_eq!(args.last().unwrap(), "out.jpg");
        // Downscale flag appears when the policy asks for it.
        let args = build_image_args(&m, &plan, 28, ScalePolicy::Force1080p, Path::new("out.jpg"));
        assert!(args.join(" ").contains("-vf scale=1440:1080"));
        // WebP plan uses -quality with the mapped value.
        let wp = ImageEncode {
            target_ext: "webp",
            encoder: "libwebp",
            note: "re-encode webp".into(),
            base_ratio: 0.70,
            orientation: 1,
            single_frame: true,
            scalable: true,
            palette: false,
            ignore_quality: false,
            out_pix: None,
        };
        let args = build_image_args(&m, &wp, 28, ScalePolicy::Preserve, Path::new("out.webp"));
        assert!(args.join(" ").contains("-c:v libwebp -quality 86"));
    }

    #[test]
    fn orientation_swaps_the_scale_target() {
        let m = img("p.jpg", "yuvj420p", 4000, 3000, 4_000_000);
        // Portrait phone photo (orientation 6): the cap applies to the
        // display-oriented short side, so 1080p lands portrait.
        assert_eq!(
            oriented_scale_target(&m, 6, ScalePolicy::Force1080p),
            Some((1080, 1440))
        );
        // Normal orientation behaves like the plain policy.
        assert_eq!(
            oriented_scale_target(&m, 1, ScalePolicy::Force1080p),
            Some((1440, 1080))
        );
        assert_eq!(oriented_scale_target(&m, 1, ScalePolicy::Preserve), None);
        // A custom box caps the DISPLAYED image: stored 4000x3000 shown
        // portrait (orientation 6) fits a 1920x1080 box as 810x1080, not
        // the 1080x1440 the swapped box would give.
        assert_eq!(
            oriented_scale_target(&m, 6, ScalePolicy::Custom(1920, 1080)),
            Some((810, 1080))
        );
        // Landscape stored grid (orientation 1) hits the width cap.
        assert_eq!(
            oriented_scale_target(&m, 1, ScalePolicy::Custom(1920, 1080)),
            Some((1440, 1080))
        );
    }

    #[test]
    fn rotation_chains_match_the_exif_placement_rule() {
        // Stored grid 3 wide x 2 tall. For each orientation the filter
        // chain must land stored row 0 [a b c] and stored col 0 [a d] on
        // the sides the EXIF spec mandates.
        let stored = vec![vec!['a', 'b', 'c'], vec!['d', 'e', 'f']];
        let cases: Vec<(u16, Vec<Vec<char>>)> = vec![
            // 2: row0 top, col0 right.
            (2, vec![vec!['c', 'b', 'a'], vec!['f', 'e', 'd']]),
            // 3: row0 bottom, col0 right.
            (3, vec![vec!['f', 'e', 'd'], vec!['c', 'b', 'a']]),
            // 4: row0 bottom, col0 left.
            (4, vec![vec!['d', 'e', 'f'], vec!['a', 'b', 'c']]),
            // 5: row0 left, col0 top.
            (5, vec![vec!['a', 'd'], vec!['b', 'e'], vec!['c', 'f']]),
            // 6: row0 right, col0 top.
            (6, vec![vec!['d', 'a'], vec!['e', 'b'], vec!['f', 'c']]),
            // 7: row0 right, col0 bottom.
            (7, vec![vec!['f', 'c'], vec!['e', 'b'], vec!['d', 'a']]),
            // 8: row0 left, col0 bottom.
            (8, vec![vec!['c', 'f'], vec!['b', 'e'], vec!['a', 'd']]),
        ];
        for (o, expected) in cases {
            let mut g = stored.clone();
            for f in rotation_filters(o) {
                g = match *f {
                    "hflip" => hflip(&g),
                    "vflip" => vflip(&g),
                    "transpose=1" => transpose1(&g),
                    "transpose=2" => transpose2(&g),
                    other => panic!("unknown filter {other}"),
                };
            }
            assert_eq!(g, expected, "orientation {o}");
        }
    }

    fn hflip(g: &[Vec<char>]) -> Vec<Vec<char>> {
        g.iter().map(|r| r.iter().rev().cloned().collect()).collect()
    }

    fn vflip(g: &[Vec<char>]) -> Vec<Vec<char>> {
        g.iter().rev().cloned().collect()
    }

    fn transpose1(g: &[Vec<char>]) -> Vec<Vec<char>> {
        let (h, w) = (g.len(), g[0].len());
        let mut out = vec![vec![' '; h]; w];
        for r in 0..h {
            for c in 0..w {
                out[c][h - 1 - r] = g[r][c];
            }
        }
        out
    }

    fn transpose2(g: &[Vec<char>]) -> Vec<Vec<char>> {
        let (h, w) = (g.len(), g[0].len());
        let mut out = vec![vec![' '; h]; w];
        for r in 0..h {
            for c in 0..w {
                out[w - 1 - c][r] = g[r][c];
            }
        }
        out
    }

    #[test]
    fn args_rotate_only_when_the_build_does_not() {
        // Builds that auto-rotate must not see transpose flags (double
        // rotation); builds that don't must carry the explicit chain.
        // Whichever this machine runs, both assertions pin the branch.
        let m = img("r.jpg", "yuvj420p", 4000, 3000, 1_000_000);
        let plan = ImageEncode {
            target_ext: "jpg",
            encoder: "mjpeg",
            note: "re-encode jpeg +rot90".into(),
            base_ratio: 0.75,
            orientation: 6,
            single_frame: true,
            scalable: true,
            palette: false,
            ignore_quality: false,
            out_pix: None,
        };
        let rotates = ffmpeg_rotates_jpeg();
        let args = build_image_args(&m, &plan, 28, ScalePolicy::Preserve, Path::new("o.jpg"));
        assert_eq!(args.join(" ").contains("transpose"), !rotates);
        // Orientation 6 swaps dims: the 1080p cap lands portrait.
        let args = build_image_args(&m, &plan, 28, ScalePolicy::Force1080p, Path::new("o.jpg"));
        let joined = args.join(" ");
        assert!(joined.contains("scale=1080:1440"), "{joined}");
        assert_eq!(joined.contains("transpose"), !rotates, "{joined}");
    }

    #[test]
    fn exif_orientation_parsed_from_jpeg_and_tiff() {
        let d = std::env::temp_dir().join(format!(
            "shrinkr-exif-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        // Little-endian TIFF EXIF block with orientation 6, wrapped in a
        // JPEG APP1 segment.
        let tiff_le: Vec<u8> = {
            let mut t = vec![b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00];
            t.extend([0x01, 0x00]); // 1 IFD entry
            t.extend([0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00]);
            t.extend([0x00, 0x00, 0x00, 0x00]); // next IFD
            t
        };
        let mut jpeg = vec![0xFF, 0xD8];
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(&tiff_le);
        let len = (app1.len() + 2) as u16;
        jpeg.extend([0xFF, 0xE1, (len >> 8) as u8, (len & 0xFF) as u8]);
        jpeg.extend(app1);
        jpeg.extend([0xFF, 0xD9]);
        let p = d.join("o6.jpg");
        std::fs::write(&p, &jpeg).unwrap();
        assert_eq!(exif_orientation(&p), 6);
        // Big-endian TIFF header (MM) with orientation 8.
        let mut tiff_be = vec![b'M', b'M', 0x00, 0x2A, 0x00, 0x00, 0x00, 0x08];
        tiff_be.extend([0x00, 0x01]);
        tiff_be.extend([0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x08, 0x00, 0x00]);
        tiff_be.extend([0x00, 0x00, 0x00, 0x00]);
        let p = d.join("o8.tif");
        std::fs::write(&p, &tiff_be).unwrap();
        assert_eq!(exif_orientation(&p), 8);
        // No EXIF → 1.
        let p = d.join("plain.jpg");
        std::fs::write(&p, [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
        assert_eq!(exif_orientation(&p), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn ico_plans_keep_every_entry_and_never_scale() {
        // Legacy BMP-entry icon: re-encoded in place as PNG entries.
        let ico = img("icon.ico", "bmp", 256, 192, 200_000);
        let p = plan_image(&ico, false).unwrap();
        assert_eq!(p.target_ext, "ico");
        assert_eq!(p.encoder, "png");
        assert!(!p.single_frame, "every entry must survive");
        assert!(!p.scalable, "icon sizes are meaningful — never resize");
        assert!(p.note.contains("ico→png"), "{}", p.note);
        // BMP-entry icons predict a real saving; PNG-entry icons land
        // near 1.0 and get skipped by threshold or the no-gain guard.
        let png_ico = img("icon.ico", "png", 256, 192, 20_000);
        // The plan doesn't key on the probed codec — a single
        // conservative base keeps both paths honest (measured: bmp
        // entries shrink ~90%, png entries ~same → guard).
        assert_eq!(plan_image(&png_ico, false).unwrap().base_ratio, p.base_ratio);
        // Args: all entry streams mapped, no frame limit, no scale even
        // when the policy asks for one.
        let args = build_image_args(&ico, &p, 28, ScalePolicy::Force1080p, Path::new("out.ico"));
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:v "), "{joined}");
        assert!(!joined.contains("0:v:0"), "{joined}");
        assert!(!joined.contains("-frames:v"), "{joined}");
        assert!(!joined.contains("scale="), "{joined}");
        assert!(joined.contains("-c:v png"), "{joined}");
        // ICO ignores the animated-duration guard (entries ≠ frames).
        let mut big = ico.clone();
        big.duration_s = 3.0;
        assert!(plan_image(&big, false).is_ok());
    }

    #[test]
    fn ico_estimate_ignores_the_scale_policy() {
        let ico = img("icon.ico", "bmp", 256, 192, 200_000);
        let p = plan_image(&ico, false).unwrap();
        let r1 = estimate_image_ratio(&ico, &p, 28, ScalePolicy::Preserve);
        let r2 = estimate_image_ratio(&ico, &p, 28, ScalePolicy::Force480p);
        assert_eq!(r1, r2, "ico is never resized — policy can't change the estimate");
        assert!(r1 < 1.0);
    }

    #[test]
    fn plan_note_carries_rotation_label() {
        let d = std::env::temp_dir().join(format!(
            "shrinkr-note-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        // Use the bundled probe image itself (orientation 6, JPEG).
        let p = d.join("probe.jpg");
        std::fs::write(&p, EXIF_PROBE_JPEG).unwrap();
        let m = img("probe.jpg", "yuvj420p", 32, 16, 200_000);
        let m = MediaFile {
            path: p,
            ..m
        };
        let plan = plan_image(&m, false).unwrap();
        assert_eq!(plan.orientation, 6);
        assert!(plan.note.contains("+rot90"), "{}", plan.note);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn collect_picks_videos_and_images() {
        let d = std::env::temp_dir().join(format!(
            "shrinkr-img-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        for name in ["a.jpg", "b.PNG", "c.mkv", "d.txt", "e.webp"] {
            std::fs::write(d.join(name), b"x").unwrap();
        }
        let hits = collect_shrink_inputs(&d);
        let names: Vec<String> = hits
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.jpg", "b.PNG", "c.mkv", "e.webp"]);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn estimate_bytes_passthrough_on_skip() {
        // Animated webp always skips (checked before encoder presence).
        let mut anim = img("anim.webp", "yuv420p", 800, 600, 1_000_000);
        anim.duration_s = 3.0;
        assert_eq!(estimate_image_bytes(&anim, 28, ScalePolicy::Preserve, false), anim.bytes);
        let jpg = img("p.jpg", "yuvj420p", 4000, 3000, 4_000_000);
        assert!(estimate_image_bytes(&jpg, 28, ScalePolicy::Preserve, false) < jpg.bytes);
        // cq_factor(28) == 1.0, so the estimate is exactly base × bytes.
        let plan = plan_image(&jpg, false).unwrap();
        assert_eq!(
            estimate_image_bytes(&jpg, 28, ScalePolicy::Preserve, false),
            (jpg.bytes as f64 * plan.base_ratio) as u64
        );
        assert!((cq_factor(28) - 1.0).abs() < 1e-9);
    }
}
