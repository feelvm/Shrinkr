# Shrinkr

**Shrink video. Shrink images. Convert anything.** Shrinkr is a fast, batch-friendly media compressor and converter for Windows, macOS and Linux with a clean desktop GUI *and* a headless CLI. It re-encodes video to dramatically smaller files using your NVIDIA GPU when available (NVENC, zero-copy NVDEC → CUDA → NVENC path) and falls back to CPU encoders automatically. Still images ride the same pipeline: JPEG/WebP re-encode or PNG/BMP/TIFF → JPEG conversion. Built in Rust on top of FFmpeg.

## Features

### Shrink (batch video & image compression)

- **Auto backend with fallback ladder** — picks the best encoder your machine can actually run: HEVC NVENC (GPU decode + encode) → H.264 NVENC → CPU-decode + NVENC → CPU x264. x265, SVT-AV1 and AV1 NVENC (auto-detected via a trial encode) are also selectable.
- **Quality control** — one CQ/CRF slider (18–40, default 28), NVENC presets P3–P6, scale policy (preserve / 1080p / 720p / 480p).
- **Target-size mode** — instead of guessing a quality value, Shrinkr measures the file's own size curve (short sample encodes + a log-linear fit) and solves for the quality that hits your target: `400MB`, `2x` smaller, `50%`, …
- **Audio-only mode** — stream-copies the video untouched and re-encodes just the audio to Opus (default 64 kbps) for remux-speed savings.
- **Smart preflight** — probes every file first and skips files that wouldn't save enough (configurable minimum-saving threshold, default 10%).
- **Images** — still images shrink in the same batch with the same quality slider and preflight threshold: JPEG and WebP are re-encoded at the slider's quality; PNG, BMP and TIFF convert to JPEG when that's smaller (60–90% off near-uncompressed sources); PNGs with transparency convert to WebP instead, keeping the alpha channel; ICO containers keep every resolution entry and their alpha but re-encode them as PNG entries (legacy BMP-entry icons shrink ~90%, already-PNG icons are left alone). JPEGs that can't win a same-format re-encode (already-crushed files) get one measured WebP fallback at the same slider quality, kept only if it's actually smaller; animated GIFs/WebPs are skipped, files under 16 KB are left alone, and an output that isn't at least ~2% smaller than its source is never placed (no wasted JPEG generations). EXIF orientation is detected and baked into the pixels — rotated phone photos come out upright regardless of the ffmpeg build. With format preservation enabled (GUI checkbox or `--image-preserve-format`), outputs keep their source format instead: a PNG with a constant fully-opaque alpha plane drops that plane, images with at most 256 distinct colors re-encode as palette PNGs with dithering off (visually identical, measured PSNR above 60 dB), and TIFFs re-encode with deflate (lossless, often 80–95% off raw scans) — while BMP, which stores pixels raw with no compression hook, honestly skips instead of converting.
- **Safe by default** — encodes to a temp file, re-probes the result to verify it, then places it next to the original (`movie.mkv`, `photo.jpg`). Originals are kept unless you explicitly choose *replace*, and replacement only happens after a verified encode.
- **Subtitles & audio tracks** — subtitles are kept by default; keep every audio track or just the primary one. Extra ffmpeg flags can be passed through (e.g. `-tune grain`).

### Convert (change the file type)

Copy-first strategy: remuxes (`-c copy`) when the source codecs are legal in the target container — seconds instead of minutes — and only re-encodes when necessary. Targets are only offered if your machine can actually produce them.

- **Video** → mp4, mkv, webm, mov, avi
- **Video → audio extraction** → mp3, m4a, opus, ogg, flac, wav
- **Audio** → mp3, m4a, opus, ogg, flac, wav
- **Images** → jpg, png, webp, gif
- **Subtitles** → srt, vtt, ass
- **Documents** (via LibreOffice headless, if installed) → pdf, docx, odt, txt, html / xlsx, ods, csv / pptx, odp

### Benchmark

Runs the same source through a matrix — x264 CRF 28 baseline plus HEVC NVENC CQ 28 at presets P3–P6 — recording output size, encode time, fps, realtime factor and GPU load, then recommends a preset and can export the table as CSV. Great for finding your GPU's sweet spot before a big batch.

### Everything else

- **Hardware capability detection** — Shrinkr asks FFmpeg what actually works on your GPU instead of hard-coding model lists (e.g. it correctly avoids AV1 NVENC on GPUs that list it but can't run it).
- **Desktop GUI** — dark, shadcn-style interface (Dioxus + Tailwind) with drag & drop, per-file progress, and a full in-app log panel.
- **Self-update** — the GUI can check GitHub Releases and update itself in place.

## Installation

### Requirements

- **FFmpeg + ffprobe on `PATH`** (required). Any recent build works; NVENC encoders are used only if present.
- **NVIDIA GPU** (optional) — enables the NVENC hardware path. Without it, Shrinkr uses CPU encoders (x264 / x265 / SVT-AV1).
- **LibreOffice** (optional) — only needed for document conversion.

### Prebuilt binaries

Grab the archive for your platform from the latest [release](../../releases), unzip, and run the `shrinkr` binary inside — no installer, nothing else to configure.

| Archive | Platform |
| --- | --- |
| `shrinkr-x86_64-pc-windows-msvc.zip` | Windows x64 |
| `shrinkr-aarch64-apple-darwin.zip` | macOS (Apple Silicon / M-series) |
| `shrinkr-x86_64-unknown-linux-gnu.zip` | Linux x64 |

- **macOS** — the binary is unsigned, so Gatekeeper blocks the first launch: right-click it and choose *Open*, or clear the quarantine flag with `xattr -cr shrinkr`.
- **Linux** — the GUI needs WebKitGTK 4.1 and GTK 3 at runtime (`sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0` on Debian/Ubuntu, or your distro's equivalent); `shrinkr-cli` has no such requirement.

### Build from source

Requires a stable [Rust](https://rustup.rs) toolchain.

```sh
git clone https://github.com/feelvm/shrinkr.git
cd shrinkr

# GUI app
cargo build --release -p shrinkr

# Headless CLI
cargo build --release -p shrinkr-cli
```

Binaries end up in `target/release/` (`shrinkr.exe`, `shrinkr-cli.exe`).

## Usage

### GUI

Launch `shrinkr.exe`, drag files or a whole folder onto the window, and pick a tab:

- **Shrink** — compress videos and images (quality slider, backend, scale, target size for video; images re-encode at the slider's quality).
- **Convert** — change file types (see the matrix above).
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
| `--images / --no-images` | Include still images in `--shrink` (default on) |
| `--image-preserve-format` | Keep image formats: png→png (constant opaque alpha planes dropped, near-lossless 256-color palettes, lossless re-encode), tiff→tiff (deflate); conversions and the WebP fallback are off (default off) |
| `--target <400MB\|2x\|50%>` | Solve per-file quality for a size target (video only) |
| `--backend <auto\|hevc\|h264\|x264\|x265\|av1\|copy>` | Encoder selection (default `auto`) |
| `--cq <18-40>` | Quality: NVENC CQ / CRF (default 28) |
| `--scale <preserve\|1080p\|720p\|480p>` | Resolution policy (default preserve) |
| `--audio <off\|8-512>` | Opus bitrate in kbps (default 64) |
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

Releases are cut by tagging (`v0.x.y`); the GitHub Actions workflow builds the GUI for Windows x64, macOS (Apple Silicon) and Linux x64, packages each binary into a `shrinkr-<target-triple>.zip`, and publishes all three to Releases — which is also what the in-app updater checks (each platform picks the archive matching its own target triple).
