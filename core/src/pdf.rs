//! Native PDF shrinking.
//!
//! Two layers, in the order they run:
//!
//! 1. **Images.** Embedded JPEGs (`DCTDecode`) and raw images
//!    (`FlateDecode`, 8-bit RGB/gray) can be decoded, downsampled to a
//!    display-appropriate long edge, re-encoded at the preset's JPEG
//!    quality, and written back as `DCTDecode`. Images are only replaced
//!    when the new stream is smaller than the old one.
//! 2. **Structure.** Every Flate stream is re-deflated at level 9 when
//!    that wins, unused objects are pruned, and the file is written with
//!    object streams and a cross-reference stream. Nothing is removed
//!    from the page content: text, fonts, links, forms, annotations and
//!    metadata stay in place.
//!
//! The lossless preset skips layer 1 and only performs layer 2, so the
//! rendered pages are byte-identical content-wise. Balanced/Small trade
//! embedded image detail for size.
//!
//! PDF is a hostile format, so every step is defensive: encrypted files,
//! unsupported filters, color spaces, bit depths, and malformed image
//! dictionaries are skipped individually instead of failing the document.
//! Before placement the output is re-parsed, its page count is compared
//! with the source, and its content streams are decompressed once.

use lopdf::{Document, Object, ObjectId, SaveOptions, Stream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub const PDF_EXT: &str = "pdf";

/// Don't spend time on tiny icons and logos: the JPEG round trip can only
/// make them worse, and their share of the document is negligible.
const MIN_IMAGE_BYTES: u64 = 12 * 1024;
const MIN_IMAGE_EDGE: u32 = 320;

/// Bounds for decoding a single image / stream. A PDF can claim absurd
/// dimensions; refusing them is cheaper than an OOM.
const MAX_IMAGE_PIXELS: u64 = 80_000_000;
const MAX_FLATE_STREAM: usize = 128 * 1024 * 1024;
const MAX_VERIFY_STREAM: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PdfPreset {
    /// Structure/stream cleanup only; pages and images are untouched.
    Lossless,
    /// Downsample large images to a ~200 dpi long edge, JPEG q80.
    #[default]
    Balanced,
    /// Stronger downsample and q66.
    Small,
}

impl PdfPreset {
    pub fn label(&self) -> &'static str {
        match self {
            PdfPreset::Lossless => "Lossless",
            PdfPreset::Balanced => "Balanced",
            PdfPreset::Small => "Small",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "lossless" | "clean" => Some(PdfPreset::Lossless),
            "balanced" | "default" => Some(PdfPreset::Balanced),
            "small" | "tiny" => Some(PdfPreset::Small),
            _ => None,
        }
    }

    fn image_plan(&self) -> Option<ImagePlan> {
        match self {
            PdfPreset::Lossless => None,
            PdfPreset::Balanced => Some(ImagePlan {
                max_edge: 2200,
                max_pixels: 4_800_000,
                quality: 80,
                min_bytes: MIN_IMAGE_BYTES,
            }),
            PdfPreset::Small => Some(ImagePlan {
                max_edge: 1500,
                max_pixels: 2_300_000,
                quality: 66,
                min_bytes: 8 * 1024,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ImagePlan {
    max_edge: u32,
    max_pixels: u64,
    quality: u8,
    min_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct PdfResult {
    pub output_bytes: u64,
    pub elapsed_s: f64,
    pub pages: usize,
    /// Total embedded images found (including those left untouched).
    pub images: usize,
    /// Images actually replaced with a smaller/reduced stream.
    pub images_recompressed: usize,
    /// Among those, how many also lost resolution.
    pub images_downscaled: usize,
    /// Flate streams that were re-deflated smaller.
    pub streams_recompressed: usize,
    pub preset: PdfPreset,
    pub ratio: f64,
}

#[derive(Clone, Debug)]
pub enum PdfOutcome {
    Done(PdfResult),
    /// The rewrite parsed and verified but was not smaller. Callers delete
    /// the temp file and keep the source.
    NoGain {
        output_bytes: u64,
    },
}

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

pub fn is_pdf(path: &Path) -> bool {
    ext_of(path) == PDF_EXT
}

/// Files this tool wrote (`report.shrunk.pdf`, `report.shrunk-1.png`, …).
/// Directory scans skip them so one run doesn't feed the next.
pub fn is_generated_name(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    stem.ends_with(".shrunk") || stem.contains(".shrunk-")
}

/// Walk `arg` (file or recursive directory) for PDFs.
pub fn collect_pdf_inputs(arg: &Path) -> Vec<PathBuf> {
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
        if is_pdf(p) && !is_generated_name(p) {
            hits.push(p.to_path_buf());
        }
    }
    hits.sort();
    hits
}

/// Compress one PDF into `out_path`. The output is parsed and verified
/// before this returns `Done`; malformed output is an error, never a
/// placed file.
pub fn compress_pdf(
    src: &Path,
    preset: PdfPreset,
    out_path: &Path,
    cancel: &AtomicBool,
) -> Result<PdfOutcome, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let src_bytes = std::fs::metadata(src)
        .map_err(|e| format!("read {}: {e}", src.display()))?
        .len();
    let mut doc = load_pdf(src)?;
    if doc.is_encrypted() {
        return Err("password-protected PDF - unlock it first".into());
    }
    let pages = doc.get_pages().len();
    if pages == 0 {
        return Err("PDF has no pages".into());
    }
    if has_signature(&doc) {
        return Err("digitally signed PDF - skipped so the signature stays valid".into());
    }
    let t0 = Instant::now();

    let mut images = 0usize;
    let mut images_recompressed = 0usize;
    let mut images_downscaled = 0usize;
    if let Some(plan) = preset.image_plan() {
        let stats = recompress_images(&mut doc, plan, cancel)?;
        images = stats.seen;
        images_recompressed = stats.recompressed;
        images_downscaled = stats.downscaled;
    }

    let streams_recompressed = recompress_flate_streams(&mut doc, cancel);
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    doc.prune_objects();
    doc.compress();

    crate::ffmpeg::ensure_output_dir(out_path)?;
    let _ = std::fs::remove_file(out_path);
    // Object streams + xref streams usually win on real documents, but
    // they carry per-stream overhead: a tiny one-page PDF can come out
    // bigger. Keep a copy so the older layout is available as a fallback.
    let mut traditional = doc.clone();
    let modern = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .max_objects_per_stream(200)
        .compression_level(9)
        .build();
    write_pdf(&mut doc, out_path, modern)?;
    let mut out_bytes = std::fs::metadata(out_path)
        .map_err(|e| format!("stat {}: {e}", out_path.display()))?
        .len();
    if out_bytes >= src_bytes {
        traditional.reference_table.cross_reference_type =
            lopdf::xref::XrefType::CrossReferenceTable;
        let alt = out_path.with_extension("plain.pdf");
        write_pdf(&mut traditional, &alt, SaveOptions::default())?;
        let alt_bytes = std::fs::metadata(&alt)
            .map_err(|e| format!("stat {}: {e}", alt.display()))?
            .len();
        if alt_bytes < out_bytes {
            std::fs::rename(&alt, out_path)
                .map_err(|e| format!("keep {}: {e}", out_path.display()))?;
            out_bytes = alt_bytes;
        } else {
            let _ = std::fs::remove_file(&alt);
        }
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(out_path);
        return Err("cancelled".into());
    }

    if let Err(e) = verify_pdf(out_path, pages) {
        let _ = std::fs::remove_file(out_path);
        return Err(e);
    }
    if out_bytes >= src_bytes {
        return Ok(PdfOutcome::NoGain {
            output_bytes: out_bytes,
        });
    }
    let elapsed = t0.elapsed().as_secs_f64().max(0.001);
    Ok(PdfOutcome::Done(PdfResult {
        output_bytes: out_bytes,
        elapsed_s: elapsed,
        pages,
        images,
        images_recompressed,
        images_downscaled,
        streams_recompressed,
        preset,
        ratio: out_bytes as f64 / src_bytes.max(1) as f64,
    }))
}

fn write_pdf(doc: &mut Document, out_path: &Path, opts: SaveOptions) -> Result<(), String> {
    let mut file = std::fs::File::create(out_path)
        .map_err(|e| format!("create {}: {e}", out_path.display()))?;
    if let Err(e) = doc.save_with_options(&mut file, opts) {
        let _ = std::fs::remove_file(out_path);
        return Err(format!("write {}: {e}", out_path.display()));
    }
    Ok(())
}

/// A rewrite invalidates any digital signature, so signed documents are
/// refused instead of silently breaking them. Checks the AcroForm
/// `SigFlags` bit and signature-field objects.
fn has_signature(doc: &Document) -> bool {
    if let Ok(catalog) = doc.catalog() {
        if let Ok(obj) = catalog.get(b"AcroForm") {
            if let Ok((_, form)) = doc.dereference(obj) {
                if let Ok(d) = form.as_dict() {
                    if d.get(b"SigFlags").and_then(Object::as_i64).unwrap_or(0) & 1 != 0 {
                        return true;
                    }
                }
            }
        }
    }
    doc.objects.values().any(|obj| {
        obj.as_dict()
            .ok()
            .is_some_and(|d| d.get(b"FT").and_then(Object::as_name).ok() == Some(b"Sig"))
    })
}

fn load_pdf(path: &Path) -> Result<Document, String> {
    Document::load(path).map_err(|e| {
        let text = e.to_string();
        if text.to_ascii_lowercase().contains("password") {
            "password-protected PDF - unlock it first".to_string()
        } else {
            format!("PDF parse failed: {text}")
        }
    })
}

// ── embedded images ──────────────────────────────────────────────────

#[derive(Default)]
struct ImageStats {
    seen: usize,
    recompressed: usize,
    downscaled: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColorSpace {
    Rgb,
    Gray,
}

impl ColorSpace {
    fn channels(self) -> usize {
        match self {
            ColorSpace::Rgb => 3,
            ColorSpace::Gray => 1,
        }
    }

    fn name(self) -> &'static [u8] {
        match self {
            ColorSpace::Rgb => b"DeviceRGB",
            ColorSpace::Gray => b"DeviceGray",
        }
    }
}

struct ImageSource {
    width: usize,
    height: usize,
    colorspace: ColorSpace,
    /// `/SMask` (soft alpha) data is left untouched, so the base image may
    /// be re-encoded but must not change dimensions.
    has_smask: bool,
    /// `DCTDecode` bytes or raw `FlateDecode` pixels, already decoded.
    data: Vec<u8>,
    jpeg: bool,
    stream_len: u64,
}

fn recompress_images(
    doc: &mut Document,
    plan: ImagePlan,
    cancel: &AtomicBool,
) -> Result<ImageStats, String> {
    let ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| {
            let stream = obj.as_stream().ok()?;
            let subtype = stream.dict.get(b"Subtype").and_then(Object::as_name).ok()?;
            (subtype == b"Image").then_some(*id)
        })
        .collect();
    let seen = ids.len();
    let mut stats = ImageStats {
        seen,
        ..Default::default()
    };
    for id in ids {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        // Snapshot everything needed while borrowing the document
        // immutably; the rewrite below needs a mutable borrow.
        let Some(src) = read_image_source(doc, id)? else {
            continue;
        };
        if src.stream_len < plan.min_bytes
            || src.width < MIN_IMAGE_EDGE as usize
            || src.height < MIN_IMAGE_EDGE as usize
        {
            continue;
        }
        let (mut dw, mut dh) = scaled_dims(src.width, src.height, plan.max_edge, plan.max_pixels);
        // Keep soft-masked images at their original grid: the mask stream
        // is not resampled, and a size mismatch would break the pair.
        if src.has_smask {
            dw = src.width;
            dh = src.height;
        }
        // Recompressing a small, already-compressed JPEG at the same size
        // rarely wins; require either a downscale or a large stream.
        if (dw, dh) == (src.width, src.height) && src.stream_len < 64 * 1024 {
            continue;
        }
        let pixels = if src.jpeg {
            let img = match libjpeg_turbo_rs::decompress(&src.data) {
                Ok(img) => img,
                Err(_) => continue,
            };
            if img.precision != 8 || (img.width, img.height) != (src.width, src.height) {
                continue;
            }
            match (img.pixel_format, src.colorspace) {
                (libjpeg_turbo_rs::PixelFormat::Rgb, ColorSpace::Rgb) => img.data,
                (libjpeg_turbo_rs::PixelFormat::Grayscale, ColorSpace::Gray) => img.data,
                _ => continue,
            }
        } else {
            let (channels, w, h) = (src.colorspace.channels(), src.width, src.height);
            let expected = w
                .checked_mul(h)
                .and_then(|n| n.checked_mul(channels))
                .ok_or("image dimensions overflow")?;
            if src.data.len() != expected {
                continue;
            }
            src.data.clone()
        };
        let resized = resize(
            &pixels,
            src.width,
            src.height,
            dw,
            dh,
            src.colorspace.channels(),
        );
        let subsampling = if src.colorspace == ColorSpace::Gray {
            libjpeg_turbo_rs::Subsampling::S444
        } else {
            libjpeg_turbo_rs::Subsampling::S420
        };
        let format = match src.colorspace {
            ColorSpace::Rgb => libjpeg_turbo_rs::PixelFormat::Rgb,
            ColorSpace::Gray => libjpeg_turbo_rs::PixelFormat::Grayscale,
        };
        let jpeg =
            match libjpeg_turbo_rs::compress(&resized, dw, dh, format, plan.quality, subsampling) {
                Ok(j) => j,
                Err(_) => continue,
            };
        // Never replace a stream with a larger one. The margin covers the
        // dictionary changes and the new `/Length`.
        if jpeg.len() as u64 + 128 >= src.stream_len {
            continue;
        }
        let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) else {
            continue;
        };
        stream.dict.set("Width", dw as i64);
        stream.dict.set("Height", dh as i64);
        stream.dict.set("BitsPerComponent", 8i64);
        stream
            .dict
            .set("ColorSpace", Object::Name(src.colorspace.name().to_vec()));
        stream
            .dict
            .set("Filter", Object::Name(b"DCTDecode".to_vec()));
        stream.dict.remove(b"DecodeParms");
        stream.dict.remove(b"Decode");
        stream.set_content(jpeg);
        stream.allows_compression = false;
        stats.recompressed += 1;
        if (dw, dh) != (src.width, src.height) {
            stats.downscaled += 1;
        }
    }
    Ok(stats)
}

/// Read the decoded pixels of one image XObject, or `None` when the
/// dictionary uses a feature this module deliberately leaves alone.
fn read_image_source(doc: &Document, id: ObjectId) -> Result<Option<ImageSource>, String> {
    let stream = match doc.objects.get(&id) {
        Some(Object::Stream(s)) => s,
        _ => return Ok(None),
    };
    let dict = &stream.dict;
    if dict
        .get(b"ImageMask")
        .and_then(Object::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    // A stencil mask and the color image are separate XObjects. Re-encoding
    // the color image can make the mask edge inaccurate; leave the pair
    // alone. (`/SMask` is a soft alpha channel and is fine.)
    if dict.get(b"Mask").is_ok() || dict.get(b"Decode").is_ok() {
        return Ok(None);
    }
    let width = match dict.get(b"Width").and_then(Object::as_i64) {
        Ok(w) if w > 0 => w as usize,
        _ => return Ok(None),
    };
    let height = match dict.get(b"Height").and_then(Object::as_i64) {
        Ok(h) if h > 0 => h as usize,
        _ => return Ok(None),
    };
    if (width as u64)
        .checked_mul(height as u64)
        .map(|n| n > MAX_IMAGE_PIXELS)
        .unwrap_or(true)
    {
        return Ok(None);
    }
    if dict
        .get(b"BitsPerComponent")
        .and_then(Object::as_i64)
        .unwrap_or(8)
        != 8
    {
        return Ok(None);
    }
    let colorspace = match resolved_name(doc, dict.get(b"ColorSpace").ok()) {
        Some(b"DeviceRGB") => ColorSpace::Rgb,
        Some(b"DeviceGray") => ColorSpace::Gray,
        _ => return Ok(None),
    };
    let filters = match stream.filters() {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    if filters.len() != 1 {
        return Ok(None);
    }
    let data = if filters[0] == b"DCTDecode" {
        (stream.content.clone(), true)
    } else if filters[0] == b"FlateDecode" {
        let raw = match stream.decompressed_content_with_limit(MAX_FLATE_STREAM) {
            Ok(raw) => raw,
            Err(_) => return Ok(None),
        };
        (raw, false)
    } else {
        return Ok(None);
    };
    Ok(Some(ImageSource {
        width,
        height,
        colorspace,
        has_smask: dict.get(b"SMask").is_ok(),
        data: data.0,
        jpeg: data.1,
        stream_len: stream.content.len() as u64,
    }))
}

fn resolved_name<'a>(doc: &'a Document, obj: Option<&'a Object>) -> Option<&'a [u8]> {
    let obj = obj?;
    let (_, obj) = doc.dereference(obj).ok()?;
    obj.as_name().ok()
}

fn scaled_dims(w: usize, h: usize, max_edge: u32, max_pixels: u64) -> (usize, usize) {
    if w == 0 || h == 0 {
        return (w.max(1), h.max(1));
    }
    let edge_scale = max_edge as f64 / w.max(h) as f64;
    let pixel_scale = (max_pixels as f64 / (w as f64 * h as f64)).sqrt();
    let scale = edge_scale.min(pixel_scale).min(1.0);
    if scale >= 0.999 {
        return (w, h);
    }
    let nw = ((w as f64 * scale).round() as usize).max(1);
    let nh = ((h as f64 * scale).round() as usize).max(1);
    (nw, nh)
}

/// Area-average resize (box filter), which is what a downscale wants:
/// every source pixel contributes, so fine detail does not alias.
fn resize(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, channels: usize) -> Vec<u8> {
    if (sw, sh) == (dw, dh) {
        return src.to_vec();
    }
    let mut out = vec![0u8; dw * dh * channels];
    for oy in 0..dh {
        let y0 = oy * sh / dh;
        let y1 = (((oy + 1) * sh) / dh).max(y0 + 1).min(sh);
        for ox in 0..dw {
            let x0 = ox * sw / dw;
            let x1 = (((ox + 1) * sw) / dw).max(x0 + 1).min(sw);
            let count = ((x1 - x0) * (y1 - y0)) as u32;
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                let row = sy * sw * channels;
                for sx in x0..x1 {
                    let px = row + sx * channels;
                    for c in 0..channels {
                        sum[c] += src[px + c] as u32;
                    }
                }
            }
            let dst = (oy * dw + ox) * channels;
            for c in 0..channels {
                out[dst + c] = (sum[c] / count) as u8;
            }
        }
    }
    out
}

// ── structural cleanup ───────────────────────────────────────────────

/// Re-deflate every single-filter Flate stream at level 9, keeping the
/// original bytes whenever the rewrite isn't smaller. Returns how many
/// streams improved.
fn recompress_flate_streams(doc: &mut Document, cancel: &AtomicBool) -> usize {
    let mut improved = 0usize;
    for obj in doc.objects.values_mut() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Object::Stream(stream) = obj else {
            continue;
        };
        if !stream.allows_compression || stream.content.len() < 256 {
            continue;
        }
        // These two are regenerated by the writer; recompressing them is
        // wasted work.
        if stream.dict.has_type(b"XRef") || stream.dict.has_type(b"ObjStm") {
            continue;
        }
        let filters = match stream.filters() {
            Ok(f) => f,
            Err(_) => continue,
        };
        if filters.len() != 1 || filters[0] != b"FlateDecode" {
            continue;
        }
        let original = stream.clone();
        let raw = match stream.decompressed_content_with_limit(MAX_FLATE_STREAM) {
            Ok(r) => r,
            Err(_) => continue,
        };
        stream.dict.remove(b"Filter");
        stream.dict.remove(b"DecodeParms");
        stream.set_content(raw);
        if stream.compress().is_err() || stream.content.len() + 16 >= original.content.len() {
            *stream = original;
        } else {
            improved += 1;
        }
    }
    improved
}

/// Output must parse, keep the page count, and every Flate/unfiltered page
/// content stream must decompress. This catches the realistic corruption
/// cases (a bad object stream or xref) without rejecting valid PDFs that
/// use filters lopdf doesn't implement elsewhere.
fn verify_pdf(path: &Path, expected_pages: usize) -> Result<(), String> {
    let doc = Document::load(path).map_err(|e| format!("output PDF does not parse: {e}"))?;
    let pages = doc.get_pages();
    if pages.len() != expected_pages {
        return Err(format!(
            "output has {} page(s), source had {expected_pages}",
            pages.len()
        ));
    }
    if doc.catalog().is_err() {
        return Err("output PDF has no usable catalog".into());
    }
    for page_id in pages.values() {
        let page = match doc.get_dictionary(*page_id) {
            Ok(p) => p,
            Err(_) => return Err("output page dictionary is unreadable".into()),
        };
        let Ok(contents) = page.get(b"Contents") else {
            continue;
        };
        let (_, obj) = doc
            .dereference(contents)
            .map_err(|_| "output page content reference is broken".to_string())?;
        match obj {
            Object::Stream(s) => verify_stream(s)?,
            Object::Array(items) => {
                for item in items {
                    if let Ok((_, Object::Stream(s))) = doc.dereference(item) {
                        verify_stream(s)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn verify_stream(stream: &Stream) -> Result<(), String> {
    let filters = match stream.filters() {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let simple = filters.is_empty() || (filters.len() == 1 && filters[0] == b"FlateDecode");
    if !simple {
        return Ok(());
    }
    stream
        .decompressed_content_with_limit(MAX_VERIFY_STREAM)
        .map(|_| ())
        .map_err(|e| format!("output page content does not decode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::content::{Content, Operation};
    use lopdf::dictionary;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "shrinkr-pdf-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn test_jpeg(w: usize, h: usize, quality: u8) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                px[i] = ((x * 7 + y * 3) & 0xff) as u8;
                px[i + 1] = ((x * 2 + y * 11) & 0xff) as u8;
                px[i + 2] = (((x / 9) ^ (y / 7)) * 23) as u8;
            }
        }
        libjpeg_turbo_rs::compress(
            &px,
            w,
            h,
            libjpeg_turbo_rs::PixelFormat::Rgb,
            quality,
            libjpeg_turbo_rs::Subsampling::S420,
        )
        .unwrap()
    }

    fn write_image_pdf(path: &Path, jpeg: &[u8], w: u32, h: u32) {
        let mut doc = Document::with_version("1.4");
        let pages_id = doc.new_object_id();
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => w as i64,
                "Height" => h as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            jpeg.to_vec(),
        ));
        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        (w as i64).into(),
                        0.into(),
                        0.into(),
                        (h as i64).into(),
                        0.into(),
                        0.into(),
                    ],
                ),
                Operation::new("Do", vec!["Im0".into()]),
                Operation::new("Q", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, {
            let mut bytes = content.encode().unwrap();
            // A large uncompressed comment gives lossless cleanup
            // something real to deflate, like the uncompressed
            // content streams real producers leave behind.
            bytes.extend_from_slice(b"\n%");
            bytes.extend(std::iter::repeat_n(b'x', 100_000));
            bytes.extend_from_slice(b"\n");
            bytes
        }));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), (w as i64).into(), (h as i64).into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im0" => image_id },
            },
            "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    fn first_image(doc: &Document) -> &Stream {
        doc.objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .find(|s| s.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image"))
            .unwrap()
    }

    #[test]
    fn lossless_preset_keeps_image_stream_bytes() {
        let d = temp_dir("lossless");
        let src = d.join("in.pdf");
        let out = d.join("out.pdf");
        let jpeg = test_jpeg(900, 700, 88);
        write_image_pdf(&src, &jpeg, 900, 700);
        let r = compress_pdf(&src, PdfPreset::Lossless, &out, &AtomicBool::new(false)).unwrap();
        let PdfOutcome::Done(res) = r else {
            panic!("a traditional PDF should shrink from object streams");
        };
        assert_eq!(res.images_recompressed, 0);
        let before = Document::load(&src).unwrap();
        let after = Document::load(&out).unwrap();
        assert_eq!(
            first_image(&before).content,
            first_image(&after).content,
            "lossless mode rewrote an image"
        );
        assert_eq!(after.get_pages().len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn balanced_preset_downsamples_and_replaces_the_image() {
        let d = temp_dir("balanced");
        let src = d.join("scan.pdf");
        let out = d.join("scan-out.pdf");
        let jpeg = test_jpeg(3000, 2000, 95);
        write_image_pdf(&src, &jpeg, 3000, 2000);
        let r = compress_pdf(&src, PdfPreset::Balanced, &out, &AtomicBool::new(false)).unwrap();
        let PdfOutcome::Done(res) = r else {
            panic!("a 6 MP JPEG scan should shrink");
        };
        assert_eq!(res.images_recompressed, 1);
        assert_eq!(res.images_downscaled, 1);
        assert!(res.output_bytes < std::fs::metadata(&src).unwrap().len());
        let after = Document::load(&out).unwrap();
        let img = first_image(&after);
        assert_eq!(
            img.dict.get(b"Filter").and_then(Object::as_name).unwrap(),
            b"DCTDecode"
        );
        let w = img.dict.get(b"Width").and_then(Object::as_i64).unwrap();
        assert!(w <= 2200, "image was not downsampled: {w}");
        // The new stream still decodes.
        let decoded = libjpeg_turbo_rs::decompress(&img.content).unwrap();
        assert_eq!(decoded.width as i64, w);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn signed_pdfs_are_refused() {
        let d = temp_dir("signed");
        let src = d.join("contract.pdf");
        let out = d.join("out.pdf");
        write_image_pdf(&src, &test_jpeg(400, 300, 85), 400, 300);
        let mut doc = Document::load(&src).unwrap();
        let acro = doc.add_object(dictionary! {
            "Fields" => Vec::<Object>::new(),
            "SigFlags" => 1i64,
        });
        doc.catalog_mut().unwrap().set("AcroForm", acro);
        doc.save(&src).unwrap();
        let err =
            compress_pdf(&src, PdfPreset::Lossless, &out, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("signed"), "{err}");
        assert!(!out.exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn refused_inputs_are_reported_not_written() {
        let d = temp_dir("bad");
        let src = d.join("not.pdf");
        let out = d.join("out.pdf");
        std::fs::write(&src, b"not a pdf at all").unwrap();
        let err =
            compress_pdf(&src, PdfPreset::Lossless, &out, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("PDF parse failed"), "{err}");
        assert!(!out.exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn scaled_dims_respects_edge_and_pixel_caps() {
        assert_eq!(scaled_dims(3000, 2000, 2200, 4_800_000), (2200, 1467));
        assert_eq!(scaled_dims(1000, 800, 2200, 4_800_000), (1000, 800));
        assert_eq!(scaled_dims(0, 0, 2200, 4_800_000), (1, 1));
    }
}
