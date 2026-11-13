# Shrinkr

**Shrink video. Shrink images. Convert anything.** Shrinkr is a fast, batch-friendly media compressor and converter for Windows, macOS and Linux with a clean desktop GUI *and* a headless CLI. It re-encodes video to dramatically smaller files using your GPU when available — NVIDIA NVENC (zero-copy NVDEC → CUDA → NVENC path), Apple VideoToolbox, Intel Quick Sync or AMD AMF, with AV1 hardware encoders on RTX 40+, Intel Arc and RX 7000+ — and falls back to CPU encoders (x264 / x265 / SVT-AV1) automatically. Still images ride the same pipeline: JPEG/WebP re-encode or PNG/BMP/TIFF → JPEG conversion. Built in Rust on top of FFmpeg.

## Features

### Shrink (batch video & image compression)

- **Preserve quality (GUI default)** — aims for a similar-looking video at a smaller size. Resolution stays adjustable; video encodes at conservative CQ/CRF 18, all audio and subtitle tracks are kept, and synthetic grain and advanced overrides are disabled. Audio is copied unless the requested target retains at least 90% of every track's bitrate; unknown-bitrate and lossless tracks stay untouched. Before placement, Shrinkr verifies duration, tracks, bit depth and HDR color tags, checks actual audio packet sizes against the 10% limit, and compares the complete video with the source at the chosen resolution (SSIM average ≥0.99, every frame ≥0.97). Larger outputs or failed checks are discarded, keeping the original. Checking adds time beyond the encode estimate. This is a conservative perceptual check, not mathematical losslessness or a guarantee of zero visible differences. Images use separate settings. Turn this mode off for target-size fitting or unrestricted manual quality/audio settings.
- **Auto backend with fallback ladder** — picks the best encoder your machine can actually run, across every vendor: HEVC NVENC (NVIDIA, zero-copy NVDEC → CUDA → NVENC) → H.264 NVENC → VideoToolbox HEVC/H.264 (Apple) → Quick Sync HEVC/H.264 (Intel) → AMF HEVC/H.264 (AMD) → CPU x264. AV1 NVENC (RTX 40+, auto-detected via a trial encode that uses the real flag set), SVT-AV1, x264 and x265 are also selectable; every hardware backend that this machine can't run is hidden from the options instead of shown-and-disabled. CPU encodes (and the VideoToolbox/QSV/AMF families) get a `-hwaccel auto` decode assist, so NVDEC/QSV/VideoToolbox feed the encoder when available — identical pixels, faster wall clock.
- **Goal presets on one speed↔size axis** — a segmented control — **Fastest** · **Balanced** (default) · **Smallest** — where position on the axis *is* the trade-off, with the ends labeled (faster·bigger ← → smaller·slower). Each goal resolves to the best encoder *this machine* has. Once videos are loaded, each segment shows the measured trade-off for *your* batch: shrink % and estimated wall time from the size model. Fastest = Auto, NVENC P4, CQ 28 (Balanced's quality, encoded quicker); Balanced = AV1 NVENC CQ 28 on RTX 40/50 (the same speed class as HEVC NVENC with ~20–30% better compression — free savings), Auto/P5 anywhere else; Smallest = SVT-AV1 CRF 32 — the most compression-efficient encoder available, CPU-slow by design — falling back to AV1 NVENC, then the machine's best hardware HEVC, only when the ffmpeg build lacks SVT-AV1. Hand-tuning any knob clears the segment highlight — the presets never fight manual settings.
- **Quality control** — one CQ/CRF slider (18–40, default 28), an encoder preset knob (NVENC P3–P6; Quick Sync and SVT-AV1 presets follow the same control), scale policy (preserve / 1080p / 720p / 480p / fit-in-box), and **AV1 film-grain synthesis** (default on): grainy sources — the ones the size model flags as dense — are denoised before encoding and the decoder regenerates statistically-similar grain, typically 30–50% smaller on exactly the files nothing else helps. The grain is synthetic, which is why it stays a visible switch.
- **Target-size mode** — instead of guessing a quality value, Shrinkr measures the file's own size curve (short sample encodes + a log-linear fit) and solves for the quality that hits your target: `400MB`, `2x` smaller, `50%`, …
- **Audio-only mode (upscale audio)** — the **Audio only** checkbox (beside the audio selector, or the *Audio only* backend) stream-copies the video untouched and re-encodes just the audio to the chosen Opus target (default 64 kbps, selectable up to 512 kbps, Opus's stereo ceiling). There the target is an instruction, not a ceiling: tracks **below** the target are re-encoded up (upscale), and the result is kept even when the file grows — size thresholds and no-gain discards do not apply when Preserve quality is off. Files without an audio track skip.
- **Smart preflight** — probes every file first and skips files that wouldn't save enough (configurable minimum-saving threshold, default 10%). Uncheck **Skip already-efficient files** to keep low-gain files in the run: the minimum becomes advisory and the plan warns (`⚠`) instead of skipping them. (Unprotected audio-only passes bypass the threshold.) A no-gain output skips only its own file; the remaining files and queued batches continue.
- **Images** — still images shrink in the same batch with the same quality slider and preflight threshold: JPEG and WebP are re-encoded at the slider's quality; PNG, BMP and TIFF convert to JPEG when that's smaller (60–90% off near-uncompressed sources); PNGs with transparency convert to WebP instead, keeping the alpha channel; ICO containers keep every resolution entry and their alpha but re-encode them as PNG entries (legacy BMP-entry icons shrink ~90%, already-PNG icons are left alone). JPEGs that can't win a same-format re-encode (already-crushed files) get one measured WebP fallback at the same slider quality, kept only if it's actually smaller; animated GIFs/WebPs are skipped, files under 16 KB are left alone, and an output that isn't at least ~2% smaller than its source is never placed (no wasted JPEG generations). EXIF orientation is detected and baked into the pixels — rotated phone photos come out upright regardless of the ffmpeg build. With format preservation enabled (GUI checkbox or `--image-preserve-format`), outputs keep their source format instead: JPEGs are rewritten losslessly (optimized Huffman tables, progressive, metadata kept), a PNG with a constant fully-opaque alpha plane drops that plane, images with at most 256 distinct colors re-encode as palette PNGs with dithering off (visually identical, measured PSNR above 60 dB), and TIFFs re-encode with deflate (lossless, often 80–95% off raw scans) — while BMP, which stores pixels raw with no compression hook, honestly skips instead of converting.
- **Safe by default** — encodes to a temp file, re-probes the result to verify it, then places it next to the original (`movie.mkv`, `photo.jpg`). Originals are kept unless you explicitly choose *replace*, and replacement only happens after a verified encode.
- **Subtitles & audio tracks** — subtitles are kept by default, as is every audio track; uncheck *Keep all audio tracks* (or pass `--primary-audio`) to keep only the primary one. Extra ffmpeg flags can be passed through (e.g. `-tune grain`).

### Convert (change the file type)

Copy-first strategy: remuxes (`-c copy`) when the source codecs are legal in the target container — seconds instead of minutes — and only re-encodes when necessary. Targets are only offered if your machine can actually produce them.

Media conversion checks the output against the source before placement: duration must match within 250 ms, expected video/audio/subtitle streams must remain, and every output video/audio stream must decode without errors. Re-encoding retains all audio tracks and maps subtitles to the target container (for example, text subtitles become mov_text in MP4). Video-to-audio extraction intentionally keeps only the primary audio track. If the target cannot retain the selected content, or verification fails, the temporary output is discarded and the original is kept. Document conversion uses its separate LibreOffice checks.

- **Video** → mp4, mkv, webm, mov, avi
- **Video → audio extraction** → mp3, m4a, opus, ogg, flac, wav
- **Audio** → mp3, m4a, opus, ogg, flac, wav
- **Images** → jpg, png, webp, gif
- **Subtitles** → srt, vtt, ass
- **Documents** (via LibreOffice headless, if installed) → pdf, docx, odt, txt, html / xlsx, ods, csv / pptx, odp

### Optimize (PDF + lossless images)

- **PDF shrinking** — no external tools. **Lossless** re-deflates every Flate stream, prunes unused objects, and rewrites the object/xref streams; text, fonts, links, forms, annotations and metadata stay in place. **Balanced** and **Small** additionally re-encode embedded JPEG and 8-bit RGB/gray Flate images, downsampling them to a display-appropriate long edge (2200 px / q80 and 1500 px / q66) and only when the new stream is actually smaller. Encrypted or digitally signed documents are refused; masked, CMYK/ICC, JBIG2/JPX and unsupported-filter images are left untouched. Before placement the output is re-parsed, its page count is compared with the source, and page content streams are decompressed to verify them; a rewrite that isn't smaller is discarded and the original kept. Outputs land next to the original as `<name>.shrunk.pdf` unless replacement is enabled.
- **Lossless images** — PNG/APNG through oxipng (filter search, palette and bit-depth reduction, zopfli in Maximum mode) and JPEG through a coefficient-domain rewrite with optimized Huffman tables and progressive output. Both preserve every pixel and keep metadata, including EXIF orientation and ICC profiles. An image that doesn't get smaller is never placed; outputs land as `<name>.shrunk.<ext>` unless replacement is enabled. Maximum mode spends more CPU on PNG and is selectable from the CLI (`--opt-level max`) and the GUI.

### Benchmark

Runs the same source through a capability-gated matrix — x264 CRF 28 baseline, HEVC NVENC CQ 28 at presets P3–P6, plus AV1 NVENC P5 and SVT-AV1 preset 8 when this machine can run them — recording output size, encode time, fps, realtime factor and GPU load, then recommends a preset and can export the table as CSV. Every successful row also **calibrates the size model**: measured output vs the model's prediction becomes a per-encoder correction factor, persisted in the app-data folder and applied to every future estimate, so the Goal-segment and batch numbers become *measured on your GPU* instead of from a generic table. Great for finding your GPU's sweet spot before a big batch.

### Everything else

- **Hardware capability detection** — Shrinkr asks FFmpeg what actually works on your GPU instead of hard-coding model lists (e.g. it correctly avoids AV1 NVENC on GPUs that list it but can't run it). The AV1 variants of every vendor family and NVENC's B-frames-as-reference mode are verified with a short trial encode using the real flag set; anything that fails is hidden from the options rather than offered and broken.
- **Desktop GUI** — dark, shadcn-style interface (Dioxus + Tailwind) with drag & drop, per-file progress, and a full in-app log panel.
- **Self-update** — the GUI can check GitHub Releases and update itself (and the bundled FFmpeg) in place.

## Installation

### Requirements

Nothing, for core functionality: the release archives bundle `ffmpeg` and `ffprobe` next to the binary, so Shrinkr runs with zero system dependencies. Optional extras:

- **GPU vendor driver** (optional) — enables the hardware encoder path for your GPU: NVIDIA (NVENC), Apple (VideoToolbox), Intel (Quick Sync) or AMD (AMF). Without one, Shrinkr uses CPU encoders (x264 / x265 / SVT-AV1).
- **LibreOffice** (optional) — only needed for document conversion.

### Prebuilt binaries

**Windows** — download `Shrinkr-Setup-<version>-x64.exe` from the latest [release](../../releases) and run it. The installer puts the GUI, the headless CLI and the bundled `ffmpeg` / `ffprobe` sidecars in `%LOCALAPPDATA%\Programs\Shrinkr`, adds Start Menu (and optionally desktop) shortcuts, and registers an uninstaller in Settings → Apps. It installs per-user without an admin prompt, which also lets the in-app self-update replace files in place. Builds are unsigned, so SmartScreen may ask for confirmation on first run (*More info → Run anyway*).

**Portable** — every platform also has a zip: grab the archive, unzip, and run the `shrinkr` binary inside — no installer, nothing else to install or configure. Each archive contains everything: the GUI (`shrinkr`), the headless CLI (`shrinkr-cli`), and static `ffmpeg` / `ffprobe` builds (see `FFMPEG-NOTES.txt` in the archive for sources and licensing).

| Archive | Platform |
| --- | --- |
| `shrinkr-x86_64-pc-windows-msvc.zip` | Windows x64 |
| `shrinkr-aarch64-apple-darwin.zip` | macOS (Apple Silicon / M-series) |
| `shrinkr-x86_64-unknown-linux-gnu.zip` | Linux x64 |

Shrinkr prefers the bundled FFmpeg over any system-wide installation, so behavior is identical on every machine. If you lost the sidecars (bare binary, source build) on Windows, the GUI header offers a one-click **Download FFmpeg** button that fetches a static build into the app's folder; on macOS/Linux, use a package manager (`brew install ffmpeg`, `apt install ffmpeg`, …).

- **macOS** — the binaries are unsigned, so Gatekeeper blocks the first launch: right-click `shrinkr` and choose *Open*, or clear the quarantine flag on the whole folder with `xattr -cr .` from inside it.
- **Linux** — the GUI needs WebKitGTK 4.1 and GTK 3 at runtime (`sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0` on Debian/Ubuntu, or your distro's equivalent); `shrinkr-cli` has no such requirement.

### Build from source

Building yourself is a developer path — at runtime you still need `ffmpeg` on `PATH` (or copied next to the binary). Requires a stable [Rust](https://rustup.rs) toolchain.

```sh
git clone https://github.com/feelvm/shrinkr.git
cd shrinkr

# GUI app
cargo build --release -p shrinkr

# Headless CLI
cargo build --release -p shrinkr-cli
```

Binaries end up in `target/release/` (`shrinkr.exe`, `shrinkr-cli.exe`).

### Build the Windows installer

Requires [Inno Setup 6](https://jrsoftware.org/isinfo.php) (`winget install JRSoftware.InnoSetup`). From the repo root:

```pwsh
./installer/windows/build-installer.ps1
```

The script builds the release binaries, downloads the same FFmpeg sidecars the release workflow bundles (cached under `target/installer/`), and writes `dist/Shrinkr-Setup-<version>-x64.exe`. Pass `-SkipBuild` to reuse existing `target/release` binaries, or `-FfmpegDir <dir>` to reuse sidecars already on disk.

### Troubleshooting

- **Windows: `shrinkr.exe` shows up in Task Manager but no window appears** — WebView2 could not open its profile (usually a stale or locked `%LOCALAPPDATA%\shrinkr` left by an older runtime, an elevated run, or a sandboxed run). Shrinkr now creates a fresh, runtime-versioned profile per launch under `%LOCALAPPDATA%\shrinkr-webview-*` and falls back to `%TEMP%` or the app's folder if that location isn't writable. If it still hangs, delete the old `%LOCALAPPDATA%\shrinkr\EBWebView` and any `%LOCALAPPDATA%\shrinkr-webview-*` folders, then relaunch.

## Usage

### GUI

Launch `shrinkr.exe`, drag files or a whole folder onto the window, and pick a tab:

- **Shrink** — compress videos and images (quality slider, backend, scale, target size for video; images re-encode at the slider's quality).
- **Convert** — change file types (see the matrix above).
- **Optimize** — shrink PDFs (lossless / balanced / small) and losslessly optimize PNG/JPEG.
- **Benchmark** — run the encoder matrix on an eligible file.

### CLI

`shrinkr-cli` drives the exact same pipeline headlessly — useful for big batches, scripting, and reproducing GUI results.

```sh
# Shrink a folder in place, deleting originals after verified encodes
shrinkr-cli --shrink D:\Videos --replace

# Solve quality per file to hit a size target
shrinkr-cli --shrink movie.mkv --target 400MB

# Force a backend, quality and resolution
shrinkr-cli --shrink . --backend av1 --cq 30 --scale 1080p

# Audio-only pass: keep video as-is, re-encode audio to Opus 64k
shrinkr-cli --shrink . --backend copy

# Shrink PDFs and losslessly optimize PNG/JPEG in one pass
shrinkr-cli --optimize D:\Docs --pdf-level balanced

# PNG/JPEG only, maximum effort
shrinkr-cli --optimize D:\Photos --opt-level max

# Shrink a photo folder too (images are on by default; JPEG/WebP
# re-encode, PNG/BMP/TIFF convert to JPEG when smaller)
shrinkr-cli --shrink D:\Photos --cq 30

# Videos only, as before 1.x behavior
shrinkr-cli --shrink D:\Videos --no-images

# Find the best NVENC preset for your GPU
shrinkr-cli --benchmark clip.mkv --bench-out bench-out

# Inspect without encoding
shrinkr-cli --probe file.mkv
shrinkr-cli --caps
shrinkr-cli --shrink D:\Videos --dry-run
```

Key flags (run `shrinkr-cli` with no arguments for the full list):

| Flag | Meaning |
| --- | --- |
| `--shrink <file\|dir>` | Run the shrink pipeline (videos + still images) |
| `--optimize <file\|dir>` | Shrink PDFs and losslessly optimize PNG/JPEG |
| `--pdf-level <lossless\|balanced\|small>` | PDF preset (default balanced) |
| `--opt-level <balanced\|max>` | PNG/JPEG effort (default balanced) |
| `--images / --no-images` | Include still images in `--shrink` (default on) |
| `--image-preserve-format` | Keep image formats: jpeg→jpeg (lossless Huffman rewrite), png→png (constant opaque alpha planes dropped, near-lossless 256-color palettes, lossless re-encode), tiff→tiff (deflate); conversions and the WebP fallback are off (default off) |
| `--target <400MB\|2x\|50%>` | Solve per-file quality for a size target (video only) |
| `--backend <auto\|av1-nvenc\|hevc\|h264\|vt-hevc\|qsv-hevc\|qsv-av1\|amf-hevc\|amf-av1\|x264\|x265\|av1\|copy>` | Encoder selection (default `auto`; hardware backends this machine can't run fall back gracefully) |
| `--cq <18-40>` | Quality: NVENC CQ / x264/x265 CRF / AV1 CRF (default 28) |
| `--film-grain / --no-film-grain` | AV1 film-grain synthesis on grainy sources (default on; SVT-AV1 backend only) |
| `--scale <preserve\|1080p\|720p\|480p>` | Resolution policy (default preserve) |
| `--audio <off\|8-512>` | Opus bitrate in kbps (default 64; 512 is the stereo max) |
| `--all-audio / --primary-audio` | Keep every audio track (default) or only the primary one |
| `--keep-subs / --no-subs` | Subtitle copy (default keep) |
| `--replace` | Delete originals after a verified encode (default: keep both) |
| `--min-saving <pct>` | Skip files that wouldn't save this much (default 10) |
| `--benchmark <file\|dir>` | Run the x264 + NVENC P3–P6 matrix |
| `--dry-run` | Print the exact ffmpeg commands without encoding |

## How it works

1. **Probe** — `ffprobe` inspects each file (codecs, resolution, streams, size).
2. **Preflight** — estimates the expected saving; files below the threshold are skipped untouched.
3. **Encode** — the selected backend runs with a runtime-driven fallback hierarchy, so a failed GPU attempt drops to the next level instead of failing the job.
4. **Verify & place** — the output is re-probed to confirm it's valid, then moved next to the original as `<name>.mkv` (video) or `<name>.jpg`/`<name>.webp` (images), or replaces the original only if you asked for it. An image output that isn't actually smaller than its source is dropped instead of placed.

## Development

The repo is a Cargo workspace:

| Crate | Purpose |
| --- | --- |
| `core` (`shrinkr-core`) | UI-agnostic engine: probing, pipeline selection, FFmpeg backend, benchmark, target solver, conversion, self-update |
| `dioxus-app` (`shrinkr`) | Dioxus desktop GUI |
| `cli` (`shrinkr-cli`) | Headless CLI driving the same core |

Releases are cut by tagging (`v0.x.y`); the GitHub Actions workflow builds the GUI for Windows x64, macOS (Apple Silicon) and Linux x64, packages each binary into a `shrinkr-<target-triple>.zip`, builds the Windows setup (`Shrinkr-Setup-<version>-x64.exe`), and publishes them to Releases — which is also what the in-app updater checks (each platform picks the archive matching its own target triple; the setup exe is not an update asset).
