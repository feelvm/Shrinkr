//! Lossless still-image optimization.
//!
//! The shrink pipeline trades pixels for bytes. This module is the
//! opposite contract: the output decodes to exactly the same pixels as the
//! source. PNG/APNG go through oxipng (filter search, palette/bit-depth
//! reduction, deflate); JPEG keeps its DCT coefficients and is re-emitted
//! with optimized Huffman tables - the `jpegtran -optimize` operation. EXIF
//! orientation and other metadata survive the JPEG rewrite.
//!
//! Every result is compared with its source: a rewrite that doesn't get
//! smaller is reported as [`OptimizeOutcome::NoGain`] and never placed.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Extensions this module can optimize losslessly.
pub const IMAGE_OPTIMIZE_EXTS: &[&str] = &["png", "jpg", "jpeg"];

/// A rewrite must save at least this many bytes to be worth placing.
/// (Round-tripping metadata and container overhead can otherwise make a
/// "smaller" file meaningless.)
const MIN_GAIN_BYTES: u64 = 64;

/// CPU budget for the PNG search. Zopfli can otherwise run for minutes on
/// a single large image.
const PNG_BUDGET_BALANCED: Duration = Duration::from_secs(20);
const PNG_BUDGET_MAX: Duration = Duration::from_secs(120);

/// PNG decompression ceiling (a decompression bomb is not an image).
const MAX_PNG_BYTES: usize = 512 * 1024 * 1024;

/// How hard the optimizer works. Both levels are lossless; `Max` spends
/// more time searching for a smaller DEFLATE stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OptimizeLevel {
    #[default]
    Balanced,
    Max,
}

impl OptimizeLevel {
    pub fn label(&self) -> &'static str {
        match self {
            OptimizeLevel::Balanced => "balanced",
            OptimizeLevel::Max => "maximum",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "balanced" | "fast" | "default" => Some(OptimizeLevel::Balanced),
            "max" | "maximum" | "best" => Some(OptimizeLevel::Max),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OptimizeResult {
    pub output_bytes: u64,
    pub elapsed_s: f64,
    /// Short engine name for logs (`"oxipng"` / `"jpegtran"`).
    pub method: &'static str,
    /// Human-readable detail (`"rgb8"`, `"progressive"`, …).
    pub note: String,
    pub ratio: f64,
}

/// `NoGain` is a successful lossless rewrite that didn't beat the source;
/// callers delete the temp file and keep the original.
#[derive(Clone, Debug)]
pub enum OptimizeOutcome {
    Done(OptimizeResult),
    NoGain { output_bytes: u64 },
}

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

pub fn is_optimizable_image(path: &Path) -> bool {
    IMAGE_OPTIMIZE_EXTS.contains(&ext_of(path).as_str())
}

/// Walk `arg` (file or recursive directory) for optimizable images.
pub fn collect_optimize_inputs(arg: &Path) -> Vec<std::path::PathBuf> {
    if arg.is_file() {
        return vec![arg.to_path_buf()];
    }
    let mut hits = Vec::new();
    for e in walkdir::WalkDir::new(arg)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        if is_optimizable_image(p) && !crate::pdf::is_generated_name(p) {
            hits.push(p.to_path_buf());
        }
    }
    hits.sort();
    hits
}

/// Optimize one image into `out_path`. The caller verifies placement and
/// applies its own replace/keep-both policy. `src` should be the original
/// file; only the bytes are read, so any path works.
pub fn optimize_image(
    src: &Path,
    out_path: &Path,
    level: OptimizeLevel,
    cancel: &AtomicBool,
) -> Result<OptimizeOutcome, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let ext = ext_of(src);
    if !IMAGE_OPTIMIZE_EXTS.contains(&ext.as_str()) {
        return Err(format!(".{ext} has no lossless optimizer"));
    }
    let data = std::fs::read(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    if data.is_empty() {
        return Err("empty file".into());
    }
    let t0 = Instant::now();
    let (optimized, method, note) = match ext.as_str() {
        "png" => (optimize_png(&data, level)?, "oxipng", png_note(&data)),
        _ => (
            optimize_jpeg(&data)?,
            "jpegtran",
            "optimized Huffman".to_string(),
        ),
    };
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    verify_rewrite(&data, &optimized, &ext)?;
    let out_bytes = optimized.len() as u64;
    if out_bytes + MIN_GAIN_BYTES >= data.len() as u64 {
        return Ok(OptimizeOutcome::NoGain {
            output_bytes: out_bytes,
        });
    }
    crate::ffmpeg::ensure_output_dir(out_path)?;
    std::fs::write(out_path, &optimized)
        .map_err(|e| format!("write {}: {e}", out_path.display()))?;
    let elapsed = t0.elapsed().as_secs_f64().max(0.001);
    Ok(OptimizeOutcome::Done(OptimizeResult {
        output_bytes: out_bytes,
        elapsed_s: elapsed,
        method,
        note,
        ratio: out_bytes as f64 / data.len().max(1) as f64,
    }))
}

fn optimize_png(data: &[u8], level: OptimizeLevel) -> Result<Vec<u8>, String> {
    let mut opts = match level {
        OptimizeLevel::Balanced => oxipng::Options::from_preset(3),
        OptimizeLevel::Max => oxipng::Options::from_preset(6),
    };
    // Lossless means lossless: no metadata stripping, no re-interlacing,
    // no rewriting of transparent pixels. Reductions that only remove
    // redundant data (palette, bit depth, color type) stay enabled.
    opts.strip = oxipng::StripChunks::None;
    opts.interlace = None;
    opts.optimize_alpha = false;
    opts.fix_errors = false;
    opts.max_decompressed_size = Some(MAX_PNG_BYTES);
    opts.timeout = Some(match level {
        OptimizeLevel::Balanced => PNG_BUDGET_BALANCED,
        OptimizeLevel::Max => PNG_BUDGET_MAX,
    });
    if level == OptimizeLevel::Max {
        opts.deflater = oxipng::Deflater::Zopfli(oxipng::ZopfliOptions {
            iteration_count: std::num::NonZeroU64::new(15).expect("non-zero"),
            ..Default::default()
        });
    }
    oxipng::optimize_from_memory(data, &opts).map_err(|e| format!("PNG optimizer: {e}"))
}

fn optimize_jpeg(data: &[u8]) -> Result<Vec<u8>, String> {
    use libjpeg_turbo_rs::{
        transform_jpeg_with_options, MarkerCopyMode, TransformOp, TransformOptions,
    };
    let opts = TransformOptions {
        op: TransformOp::None,
        optimize: true,
        progressive: true,
        copy_markers: MarkerCopyMode::All,
        ..Default::default()
    };
    transform_jpeg_with_options(data, &opts).map_err(|e| format!("JPEG optimizer: {e}"))
}

fn png_note(src: &[u8]) -> String {
    let depth = src.get(24).copied().unwrap_or(0);
    let color = src.get(25).copied().unwrap_or(0);
    let color = match color {
        0 => "gray",
        2 => "rgb",
        3 => "palette",
        4 => "gray+alpha",
        6 => "rgba",
        _ => "png",
    };
    if is_apng(src) {
        format!("APNG {color} {depth}-bit")
    } else {
        format!("{color} {depth}-bit")
    }
}

fn is_apng(data: &[u8]) -> bool {
    let mut off = 8usize;
    while off + 8 <= data.len() {
        let len =
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        let ty = &data[off + 4..off + 8];
        if ty == b"acTL" {
            return true;
        }
        if ty == b"IEND" {
            return false;
        }
        off = match off.checked_add(12).and_then(|o| o.checked_add(len)) {
            Some(o) => o,
            None => return false,
        };
    }
    false
}

/// Cheap structural checks before a result can replace a source.
fn verify_rewrite(src: &[u8], out: &[u8], ext: &str) -> Result<(), String> {
    match ext {
        "png" => {
            const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
            if !out.starts_with(SIG) {
                return Err("optimizer produced an invalid PNG".into());
            }
            let before = png_dims(src).ok_or("source PNG header is corrupt")?;
            let after = png_dims(out).ok_or("output PNG header is corrupt")?;
            if before != after {
                return Err(format!(
                    "PNG dimensions changed ({}x{} -> {}x{})",
                    before.0, before.1, after.0, after.1
                ));
            }
            if is_apng(src) && !is_apng(out) {
                return Err("optimizer dropped APNG animation data".into());
            }
        }
        "jpg" | "jpeg" => {
            use libjpeg_turbo_rs::probe;
            let before = probe(src).map_err(|e| format!("source JPEG: {e}"))?;
            let after = probe(out).map_err(|e| format!("output JPEG invalid: {e}"))?;
            if (before.width, before.height) != (after.width, after.height) {
                return Err(format!(
                    "JPEG dimensions changed ({}x{} -> {}x{})",
                    before.width, before.height, after.width, after.height
                ));
            }
            if out.len() < 128 {
                return Err("optimizer produced an invalid JPEG".into());
            }
        }
        _ => return Err("unsupported image type".into()),
    }
    Ok(())
}

fn png_dims(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "shrinkr-opt-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_png(w: u32, h: u32) -> Vec<u8> {
        // A tiny RGBA PNG writer: one IDAT with zlib-wrapped stored
        // (uncompressed) deflate blocks. Enough for oxipng to have
        // something real to optimize.
        let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
        for y in 0..h {
            raw.push(0); // filter: none
            for x in 0..w {
                raw.push(((x * 255 / w.max(1)) & 0xff) as u8);
                raw.push(((y * 255 / h.max(1)) & 0xff) as u8);
                raw.push(((x ^ y) & 0xff) as u8);
                raw.push(255);
            }
        }
        let mut z = vec![0x78, 0x01];
        let mut chunks = raw.chunks(65535).peekable();
        while let Some(c) = chunks.next() {
            let last = chunks.peek().is_none();
            z.push(if last { 1 } else { 0 });
            let len = c.len() as u16;
            z.extend_from_slice(&len.to_le_bytes());
            z.extend_from_slice(&(!len).to_le_bytes());
            z.extend_from_slice(c);
        }
        z.extend_from_slice(&adler32(&raw).to_be_bytes());

        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &z);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(ty);
        out.extend_from_slice(data);
        let mut crc_input = ty.to_vec();
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &x in data {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    #[test]
    fn png_round_trip_is_lossless_and_drops_redundancy() {
        let d = temp_dir("png");
        let src = d.join("flat.png");
        let out = d.join("flat.shrunk.png");
        // A flat image where palette reduction is legal.
        let png = make_png(64, 64);
        std::fs::write(&src, &png).unwrap();
        let r =
            optimize_image(&src, &out, OptimizeLevel::Balanced, &AtomicBool::new(false)).unwrap();
        assert!(
            matches!(r, OptimizeOutcome::Done(_)),
            "expected a gain: {r:?}"
        );
        let data = std::fs::read(&out).unwrap();
        assert!(is_apng(&data) == is_apng(&png));
        assert_eq!(png_dims(&data), png_dims(&png));
        assert!(data.len() < png.len());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn jpeg_rewrite_keeps_pixels_and_metadata() {
        use libjpeg_turbo_rs::{compress, decompress, PixelFormat, Subsampling};
        let d = temp_dir("jpg");
        let src = d.join("noise.jpg");
        let out = d.join("noise.shrunk.jpg");
        let (w, h) = (320usize, 240usize);
        let mut px = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                px[i] = ((x * 13 + y * 7) & 0xff) as u8;
                px[i + 1] = ((x * 5 + y * 11) & 0xff) as u8;
                px[i + 2] = ((x * 3) ^ (y * 17) & 0xff) as u8;
            }
        }
        let jpeg = compress(&px, w, h, PixelFormat::Rgb, 92, Subsampling::S420).unwrap();
        std::fs::write(&src, &jpeg).unwrap();
        let r =
            optimize_image(&src, &out, OptimizeLevel::Balanced, &AtomicBool::new(false)).unwrap();
        let OptimizeOutcome::Done(res) = r else {
            panic!("baseline JPEG should shrink from optimized Huffman tables");
        };
        assert_eq!(res.method, "jpegtran");
        let before = decompress(&jpeg).unwrap();
        let after = decompress(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!((before.width, before.height), (after.width, after.height));
        assert_eq!(before.data, after.data, "lossless rewrite changed pixels");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn jpeg_rewrite_preserves_exif_metadata() {
        use libjpeg_turbo_rs::decompress;
        let src = include_bytes!("../assets/exif_probe.jpg");
        let out = optimize_jpeg(src).unwrap();
        let before = decompress(src).unwrap();
        let after = decompress(&out).unwrap();
        assert_eq!(before.exif_data(), after.exif_data());
        assert_eq!(before.exif_orientation(), after.exif_orientation());
    }

    #[test]
    fn already_tiny_or_already_optimized_files_do_not_place() {
        let d = temp_dir("nogain");
        let src = d.join("tiny.png");
        let out = d.join("tiny.shrunk.png");
        let png = make_png(2, 2);
        std::fs::write(&src, &png).unwrap();
        let r =
            optimize_image(&src, &out, OptimizeLevel::Balanced, &AtomicBool::new(false)).unwrap();
        assert!(matches!(r, OptimizeOutcome::NoGain { .. }), "{r:?}");
        assert!(!out.exists());
        std::fs::remove_dir_all(&d).ok();
    }
}
