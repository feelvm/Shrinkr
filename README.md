# Shrinkr

**Shrink video. Convert anything.** Shrinkr is a fast, batch-friendly media compressor and converter for Windows with a clean desktop GUI *and* a headless CLI. It re-encodes video to dramatically smaller files using your NVIDIA GPU when available (NVENC, zero-copy NVDEC → CUDA → NVENC path) and falls back to CPU encoders automatically. Built in Rust on top of FFmpeg.

## Features

### Shrink (batch video compression)

- **Auto backend with fallback ladder** — picks the best encoder your machine can actually run: HEVC NVENC (GPU decode + encode) → H.264 NVENC → CPU-decode + NVENC → CPU x264. x265, SVT-AV1 and AV1 NVENC (auto-detected via a trial encode) are also selectable.
- **Quality control** — one CQ/CRF slider (18–40, default 28), NVENC presets P3–P6, scale policy (preserve / 1080p / 720p / 480p).
- **Target-size mode** — instead of guessing a quality value, Shrinkr measures the file's own size curve (short sample encodes + a log-linear fit) and solves for the quality that hits your target: `400MB`, `2x` smaller, `50%`, …
- **Audio-only mode** — stream-copies the video untouched and re-encodes just the audio to Opus (default 64 kbps) for remux-speed savings.
- **Smart preflight** — probes every file first and skips files that wouldn't save enough (configurable minimum-saving threshold, default 10%).
- **Safe by default** — encodes to a temp file, re-probes the result to verify it, then places it next to the original (`movie.mkv`). Originals are kept unless you explicitly choose *replace*, and replacement only happens after a verified encode.
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

### Prebuilt binary (Windows x64)

1. Grab the latest `shrinkr-x86_64-pc-windows-msvc.zip` from [Releases](../../releases).
2. Unzip and run `shrinkr.exe` — no installer, nothing else to configure.

### Build from source

Requires a stable [Rust](https://rustup.rs) toolchain.

```sh
git clone https://github.com/<owner>/shrinkr.git
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

- **Shrink** — compress videos (quality slider, backend, scale, target size).
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
| `--shrink <file\|dir>` | Run the shrink pipeline |
| `--target <400MB\|2x\|50%>` | Solve per-file quality for a size target |
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
4. **Verify & place** — the output is re-probed to confirm it's valid, then moved next to the original as `<name>.mkv` (or replaces the original only if you asked for it).

## Development

The repo is a Cargo workspace:

| Crate | Purpose |
| --- | --- |
| `core` (`shrinkr-core`) | UI-agnostic engine: probing, pipeline selection, FFmpeg backend, benchmark, target solver, conversion, self-update |
| `dioxus-app` (`shrinkr`) | Dioxus desktop GUI |
| `cli` (`shrinkr-cli`) | Headless CLI driving the same core |

Releases are cut by tagging (`v0.x.y`); the GitHub Actions workflow builds the Windows GUI, packages `shrinkr.exe` into a zip, and publishes it to Releases — which is also what the in-app updater checks.
