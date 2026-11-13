# Shrinkr

**Shrink video. Shrink images. Convert anything.**

Shrinkr is a fast, batch-friendly media compressor and converter for Windows,
macOS and Linux, with a desktop GUI and a headless CLI. It encodes with your
GPU when available — NVIDIA NVENC, Apple VideoToolbox, Intel Quick Sync or AMD
AMF, including AV1 on supported hardware — and falls back to CPU encoders
automatically. Built in Rust on FFmpeg.

## What it does

- **Shrink** videos and still images in batches. One quality slider, automatic
  hardware selection, resolution policies, target-size mode (`400MB`, `2x`),
  and audio-only passes.
- **Preserve quality** (GUI default): samples the source, picks the strongest
  quality setting that clears a strict SSIM gate, then verifies the complete
  output before keeping it. If it fails, the original stays.
- **Convert** video, audio, images and subtitles, plus documents through
  LibreOffice. Copy-first: remux when possible, re-encode only when needed.
- **Optimize** PDFs (lossless / balanced / small) and losslessly recompress
  PNG/JPEG without touching a pixel.
- **Benchmark** the encoders on your machine and calibrate future size
  estimates to your GPU.
- **Safe by default**: encode to a temp file, re-probe and verify, then place.
  Originals are kept unless you ask to replace them.

## Install

Grab the latest [release](https://github.com/feelvm/Shrinkr/releases). No
dependencies are required for core functionality: the archives bundle `ffmpeg`
and `ffprobe`. A GPU driver enables hardware encoding; LibreOffice is only
needed for document conversion.

| Platform | Download |
| --- | --- |
| Windows x64 | `Shrinkr-Setup-<version>-x64.exe` (installer) or `shrinkr-x86_64-pc-windows-msvc.zip` (portable) |
| macOS (Apple Silicon) | `shrinkr-aarch64-apple-darwin.dmg` or `.zip` |
| Linux x64 | `shrinkr-x86_64-unknown-linux-gnu.zip` |

- **Windows**: the installer is per-user (no admin prompt) and unsigned, so
  SmartScreen may ask for confirmation (*More info → Run anyway*).
- **macOS**: builds are not notarized. Right-click the app and choose *Open*,
  or run `xattr -cr .` inside the unzipped folder.
- **Linux**: the GUI needs WebKitGTK 4.1 and GTK 3 at runtime
  (`sudo apt install libwebkitgtk-4.1-0 libgtk-3-0` on Debian/Ubuntu).
  `shrinkr-cli` has no such requirement.

## Quick start

Launch Shrinkr, drop files or folders onto the window, and pick a tab:

| Tab | Purpose |
| --- | --- |
| **Shrink** | Compress videos and images |
| **Convert** | Change file types |
| **Optimize** | Shrink PDFs, losslessly optimize PNG/JPEG |
| **Benchmark** | Measure encoder speed and size on your machine |

The CLI drives the same pipeline:

```sh
# Shrink a folder in place; originals are deleted only after verified encodes
shrinkr-cli --shrink D:\Videos --replace

# Hit a size target
shrinkr-cli --shrink movie.mkv --target 400MB

# Force encoder, quality and resolution
shrinkr-cli --shrink . --backend av1 --cq 30 --scale 1080p

# Shrink PDFs and losslessly optimize PNG/JPEG in one pass
shrinkr-cli --optimize D:\Docs --pdf-level balanced

# Inspect without encoding
shrinkr-cli --probe file.mkv
shrinkr-cli --caps
shrinkr-cli --shrink D:\Videos --dry-run
```

Run `shrinkr-cli --help` for every flag.

## Documentation

- **[Technical deep dive](TECHNICAL.md)** — architecture, encoder ladder,
  capability detection, quality search and verification, target solver, film
  grain, image and PDF internals, benchmark calibration, and release
  engineering.
- [Full CLI reference](TECHNICAL.md#cli-reference)
- [Troubleshooting](TECHNICAL.md#troubleshooting)

## Build from source

A stable [Rust](https://rustup.rs) toolchain is required. At runtime you also
need `ffmpeg` on `PATH` or next to the binary.

```sh
git clone https://github.com/feelvm/shrinkr.git
cd shrinkr
cargo build --release -p shrinkr
cargo build --release -p shrinkr-cli
```

Binaries land in `target/release/`. For the Windows installer, install
[Inno Setup 6](https://jrsoftware.org/isinfo.php) and run
`./installer/windows/build-installer.ps1`.

## Project layout

| Crate | Purpose |
| --- | --- |
| `core` (`shrinkr-core`) | UI-agnostic engine |
| `dioxus-app` (`shrinkr`) | Dioxus desktop GUI |
| `cli` (`shrinkr-cli`) | Headless CLI |

Releases are cut by tagging `v0.x.y`; GitHub Actions builds and publishes the
platform archives, the Windows installer and the macOS DMG.
