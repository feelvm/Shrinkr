# Shrinkr — technical deep dive

This is the engineering companion to the [README](README.md): how Shrinkr is
built, how each pipeline makes decisions, and what runs before a file is kept
or replaced.

Shrinkr is a Rust Cargo workspace with a UI-agnostic core, a Dioxus desktop
GUI, and a headless CLI. FFmpeg does the media I/O and encoding; the decision
logic, verification, PDF handling, and lossless image work are native Rust.

## Design principles

- **One engine, two frontends.** The GUI and `shrinkr-cli` call the same
  `shrinkr-core` functions. Anything the GUI can do is scriptable.
- **Capability-driven, not model-driven.** Encoder availability is detected by
  asking FFmpeg and by running trial encodes with the exact flag set the real
  encode would use. Model names are never hard-coded.
- **Measure the file, don't trust a table.** Target-size mode and the
  preserve-quality search encode short samples from the actual source.
  Benchmarks can replace the generic size model with measured correction
  factors.
- **Fail closed.** Every output is verified before placement. A failed check
  discards the temporary file and keeps the original.
- **Native where FFmpeg is the wrong tool.** PDFs use `lopdf` + libjpeg-turbo;
  PNG optimization uses `oxipng`; JPEG optimization uses a coefficient-domain
  rewrite with optimized Huffman tables.

## Repository map

| Crate | Role |
| --- | --- |
| `core` (`shrinkr-core`) | Probing, planning, encoder selection, FFmpeg backend, verification, images, PDF, convert, benchmark, calibration, self-update |
| `dioxus-app` (`shrinkr`) | Dioxus 0.7 desktop GUI, vendored shadcn-style `rust-ui` components, Tailwind |
| `cli` (`shrinkr-cli`) | Argument parsing and headless orchestration over `core` |

Important core modules:

| Module | Responsibility |
| --- | --- |
| `media.rs` | `ffprobe` JSON → `MediaFile` (codecs, resolution, fps, bit depth, color tags, tracks, duration) |
| `pipeline.rs` | Estimate, preflight, backend ladder, stream planning, scale policy |
| `ffmpeg.rs` | Argument builder, fallback loop, progress, verification, placement |
| `hw.rs` | Runtime hardware-capability detection and GPU sampling |
| `quality.rs` | Sample search, SSIM checks, audio-bitrate protection |
| `target.rs` | Target-size parsing and per-file CRF solver |
| `grain.rs` | Frame analysis and adaptive SVT-AV1 film-grain strength |
| `images.rs` | Still-image planning, shrinking, format preservation |
| `convert.rs` | Format conversion matrix, copy-first remuxing, output verification |
| `optimize.rs` | Lossless PNG/JPEG optimization |
| `pdf.rs` | Native PDF shrinking and verification |
| `bench.rs`, `calib.rs` | Encoder matrix and persisted estimate calibration |
| `process.rs`, `pause.rs`, `log.rs` | Child processes, pause/cancel, log classification |
| `update.rs`, `ffmpeg_fetch.rs` | Self-update and one-click FFmpeg acquisition |

## Runtime architecture

The GUI owns an `AppState` in a Dioxus `Signal`. Worker threads never touch UI
state: they send `JobMsg` values over a futures channel to a UI-thread
coroutine that applies them. Dioxus 0.7 signals use unsync storage, so this
separation is required, not just tidy.

- **Cancellation** is an `AtomicBool` polled by the process runner. A stalled
  child can still be killed because output pipes are read on separate threads
  and the process is polled independently.
- **Pause** is a process-global flag plus one watcher thread per ffmpeg child.
  The watcher suspends the child (`NtSuspendProcess` on Windows, `SIGSTOP` on
  Unix) while paused and resumes it afterwards. Cancel still wins over pause.
- **Child processes** go through `process::cmd`, which resolves bundled
  `ffmpeg`/`ffprobe` next to the running executable before falling back to
  `PATH`, and sets `CREATE_NO_WINDOW` on Windows so the GUI never flashes
  console windows.
- **Work directories** are tried under the process temp directory first, then
  app data, so a locked or wiped `%TEMP%` does not fail a batch.

## The Shrink pipeline

### Probe

`media.rs` runs `ffprobe -show_streams -show_format -of json` and builds a
`MediaFile`. Besides the obvious fields it records:

- codec, dimensions, average/real frame rate, pixel format, bit depth;
- color primaries/transfer/space/range, used to detect HDR and re-assert tags;
- duration and stream bitrates (with conservative fallbacks when the container
  omits them, as MKV commonly does);
- every audio track (codec, bitrate, channels, sample rate, layout);
- subtitle, attachment, and chapter counts.

Embedded fonts and image streams without dimensions are counted as attachments,
not video.

### Estimate and preflight

The size model is deliberately conservative. At shared CQ 28 the expected
output/input ratio comes from per-codec tables, then:

```text
density       = clamp(typical_bits_per_pixel / actual_bits_per_pixel, 0.35, 1.15)
base_ratio    = 1 - (1 - table_ratio) * density
cq_factor     = 0.93 ^ (cq - 28)          // ≈7% per quality point
preset_factor = 1.12 P3 / 1.04 P4 / 1.00 P5 / 0.96 P6 / 0.94 P7+
                SVT: 1.22 p10 / 1.18 p9 / 1.12 p8 / 1.06 p7 / 1.00 p6 / 0.97 p5 / 0.94 p4
scale_factor  = (output_pixels / input_pixels) ^ 0.85
video_bytes   = source_video_bytes * base_ratio * cq_factor * preset_factor * scale_factor
output_bytes  = video_bytes + planned_audio_bytes
```

`density` corrects for grainy or high-bitrate sources that barely shrink.
Film-grain synthesis raises the effective density floor to 0.8 because the
denoiser removes much of that cost. Audio is added separately, using the
planned copy/Opus/AAC decision.

A lean modern source — HEVC, AV1 or VP9 at or below typical bits per pixel —
re-encoded by SVT-AV1 at its native resolution gets a floor:

```text
video_factor >= preset_factor * cq_factor(cq) / cq_factor(24)   // SVT CRF 36 = shared CQ 24
```

SVT's CRF scale is stricter than the hardware CQ scale, so an already-efficient
file from an earlier pass sits near CRF 36 equivalent. Estimating below that
quality promises savings the second-generation encode cannot deliver (a
0.9 Mbps 720p HEVC file estimated at 83 MB encoded to 184 MB). On the shared
slider, Smallest's CQ 32 maps to CRF 44 — above that frontier — so it can
still promise and deliver a saving. Older codecs and downscaling are exempt:
both create real headroom.

Preflight classifies each file:

- **Skip** — the estimate is above the source size or below the minimum-saving
  threshold (default 10%).
- **Warn** — same, but the user chose “run anyway”; the file is queued and its
  output is still subject to the real no-gain guard.
- **Shrink** — proceed.

For already-efficient codecs (AV1/HEVC/VP9), a bitrate tier check can short-
circuit obvious no-gain files, but the estimator is allowed to overrule it
where real headroom exists (a lower resolution or a materially higher quality
setting). A lean modern source at native resolution is left as a skip instead
of being promised a saving it cannot deliver.

A max-shrink SVT-AV1 encode that comes out no smaller than its source retries
at CRF +4, up to three attempts and capped at CRF 40; every retry is logged and
the final report names the CRF that won. Other backends keep the quality the
user selected and report no gain instead of silently degrading the picture.

### Stream planning

The default plan keeps:

- the primary video stream;
- every audio track (`--primary-audio` keeps only the first);
- all subtitle streams when subtitles are enabled;
- no attachments, data streams, chapters, or global metadata.

Audio policy per track:

- `--audio off` copies everything.
- Otherwise a track is copied when its bitrate is already at or below the
  target (within 8 kbps). Re-encoding a smaller track could only grow it.
- Tracks that need work become Opus at the selected bitrate, capped at 256 kbps
  per channel (512 kbps stereo).
- Channel layouts libopus rejects (side/ambisonic/22.2) fall back to AAC at
  roughly twice the Opus bitrate.
- In an audio-only pass (`--backend copy`), the selected bitrate is an
  instruction, not a ceiling: tracks are re-encoded even when already smaller,
  which is how “upscale audio” works.

HDR color tags are re-asserted on every encode because `-map_metadata -1`
strips container metadata and encoders default to BT.709.

### Encoding and fallback

`run_with_fallback` walks the ordered attempt list for the selected backend.
Each attempt is a full ffmpeg invocation with `-progress pipe:1`. An attempt
fails over when it exits non-zero, leaves a suspicious stub, produces the
wrong codec, or looks truncated. Two targeted retries avoid burning the whole
ladder on identical mistakes:

- a subtitle-mapping error retries once with subtitles dropped;
- an Opus layout error retries once with AAC.

Decode strategy:

- NVENC zero-copy levels use `-hwaccel cuda -hwaccel_output_format cuda
  -extra_hw_frames 3`, so NVDEC frames stay on the GPU and `scale_cuda` /
  NVENC consume them without a round trip.
- CPU encoders and the VideoToolbox/QSV/AMF families get `-hwaccel auto`: any
  listed decoder feeds the encoder, frames return to system RAM, and FFmpeg
  silently falls back to software decode when needed.

Encoder flags by family:

| Level | Encoder and quality flags |
| --- | --- |
| HEVC NVENC | `hevc_nvenc -preset pN -tune hq -rc vbr -cq N -b:v 0`, lookahead 32, spatial/temporal AQ, `-b_ref_mode middle` when trial-gated on |
| H.264 NVENC | Same NVENC quality block, `-profile:v high` |
| AV1 NVENC | Same block plus `-b_ref_mode middle`, `-profile:v main` |
| VideoToolbox | `hevc_videotoolbox`/`h264_videotoolbox`, quality via `-q:v`, `-allow_sw 1` |
| Quick Sync | `hevc_qsv`/`h264_qsv`/`av1_qsv`, `-global_quality N`, mapped named preset, lookahead on H.264/HEVC |
| AMF | `hevc_amf`/`h264_amf`/`av1_amf`, `-rc cqp`, `-qp_i N-2 -qp_p N`, mapped quality mode, VBAQ on H.264/HEVC |
| x264 | `libx264 -crf N -preset medium` |
| x265 | `libx265 -crf N -preset slow` |
| SVT-AV1 | `libsvtav1 -crf (CQ+12) -preset 4-10`, always `yuv420p10le`, optional `film-grain=N:film-grain-denoise=1` |
| Copy video | `-c:v copy`, no video filters or encoder flags |

The shared preset knob maps to each family: P3-P6 are NVENC presets; SVT uses
P3→10, P4→9, P5→8 (default), P6→7, P7→6, P8→5, P9→4; Quick Sync uses named
presets; AMF uses speed/balanced/quality. Hardware encoders clamp P7-P9 to
their slowest available preset. The shared quality slider also maps to SVT's
stricter CRF scale: libsvtav1 receives `CQ + 12` (CQ 28 ≈ CRF 40, CQ 32 = CRF
44), so the same slider number means a comparable quality and size across
encoder families.

### Verification and placement

After an encode, the attempt-level check confirms:

- ffmpeg exited successfully;
- the output is not a stub (≥32 KiB);
- the coded codec is HEVC/H.264/AV1 (or byte-identical codec for a copy);
- the output is not more than 5% shorter than a longer source, unless the
  source's own last packet ends where the output does.

Placement then depends on mode:

- **Keep both** — output is `<stem>.shrunk.<ext>`, then
  `<stem>.shrunk-1.<ext>`, and so on. A same-extension output never overwrites
  an unrelated file.
- **Replace** — for same-name outputs, the original is renamed to
  `.orig.bak`, the new file is moved in, and the backup is removed; a failed
  move restores the backup. Cross-extension replacement picks a collision-free
  destination first and deletes the original only after the move succeeds.
- **Cross-volume moves** (temp directory on another drive) fall back to
  copy-then-delete.

## Hardware capability detection

`hw.rs` runs once per process and caches `HwCaps`. Detection has three layers:

1. `ffmpeg -hwaccels` — which decode accelerators exist.
2. `ffmpeg -encoders` — which encoder names are compiled in.
3. **Trial encodes** — does the encoder initialize on this GPU with the exact
   flags Shrinkr sends? Each trial is a 0.4-second 320×240 synthetic clip with
   a 20-second deadline. This catches the cases where the encoder is listed but
   unusable: AV1 NVENC on RTX 20/30, AV1 QSV/AMF on older GPU generations, and
   `-b_ref_mode` on Pascal.

`ffmpeg -decoders` also records available `*_cuvid` decoders. Before a
zero-copy attempt, `hw_decode_viable` rejects 4:4:4 chroma and codecs with no
matching CUVID decoder, avoiding a wasted encode that would fail immediately.

The GPU label comes from `nvidia-smi` when present. During encodes and
benchmarks, a sampler polls `nvidia-smi` every 750 ms for average/maximum GPU
utilization; non-NVIDIA machines report zeros.

Backends that this machine cannot run are hidden from the GUI choices rather
than offered and broken. The CLI accepts every backend name and falls back
gracefully, so an explicit `--backend av1-nvenc` on an unsupported GPU still
encodes via the next best level.

## Encoder ladder

`Auto` is HEVC-first for playback compatibility:

```text
HEVC NVENC zero-copy → HEVC NVENC CPU-decode → H.264 NVENC zero-copy
→ H.264 NVENC CPU-decode → VideoToolbox HEVC/H.264 → Quick Sync HEVC/H.264
→ AMF HEVC/H.264 → x264
```

Explicit modes keep the same spirit:

| Backend | Attempts |
| --- | --- |
| `av1-nvenc` | AV1 NVENC zero-copy → AV1 NVENC CPU-decode → HEVC NVENC ladder → x264 |
| `hevc` / `h264` | NVENC zero-copy → NVENC CPU-decode → x264 |
| `vt-hevc` | VideoToolbox HEVC → VideoToolbox H.264 → x264 |
| `qsv-hevc` | Quick Sync HEVC → Quick Sync H.264 → x264 |
| `qsv-av1` | Quick Sync AV1 (if trial passed) → QSV HEVC → QSV H.264 → x264 |
| `amf-hevc` | AMF HEVC → AMF H.264 → x264 |
| `amf-av1` | AMF AV1 (if trial passed) → AMF HEVC → AMF H.264 → x264 |
| `x265` | x265 slow → x264 |
| `av1` | SVT-AV1 (or x264 if the build lacks it) |
| `copy` | Video stream copy only |

Only genuinely hardware-related errors advance the ladder early. Other errors
still try the next level, but a terminal CPU level (x264, x265, or SVT-AV1) is
treated as authoritative: if it fails, the file is genuinely broken.

## Goals, quality and target size

### Goals

The GUI's Fastest / Balanced / Smallest control resolves to a concrete
backend, preset, and CQ on the current machine:

| Goal | Resolution |
| --- | --- |
| Fastest | `auto`, P4, CQ 30 - least shrink, quickest encode |
| Balanced | AV1 NVENC P5 CQ 30 when the trial passed; otherwise `auto` P5 CQ 32 |
| Smallest | SVT-AV1 preset 6 (P7) at shared CQ 32 (= CRF 44); then AV1 NVENC P6 CQ 34, HEVC NVENC P6 CQ 34, VideoToolbox/QSV/AMF HEVC P5 CQ 34, then `auto` P6 CQ 34 |

The CQ values are shared-slider values calibrated against measured encodes,
not nominal encoder numbers: SVT's CRF scale is stricter, so libsvtav1
receives `CQ + 12` (`SVT_CRF_OFFSET` in `core/src/pipeline.rs`). Fastest
and Balanced therefore deliver a real, visible shrink instead of the 2-5%
that CQ 28 produced on already-efficient sources.

Once files are loaded, each segment shows the measured trade-off for that
batch: predicted shrink percentage and wall time from the size model. Editing
any knob clears the highlighted segment, so the presets never fight manual
settings. Under Preserve quality, the CQ value is pinned by the mode, so
segments match on encoder and preset only.

Smallest has a second safety net: when its first full encode is not smaller
than the source, it retries at shared CQ +4 (up to three attempts, capped at
CQ 40, i.e. SVT CRF 44 → 48 → 52) before discarding the result. The run report
names the final CRF.

### Preserve quality

This is the GUI default. It is a conservative perceptual policy, not
mathematical losslessness:

1. **Sample.** For sources of at least 4 seconds, cut up to three short
   windows across the timeline. Window length is roughly `duration / 12`,
   clamped to 1.5–3 seconds. Files at least four window lengths long use 15%,
   50%, and 85%; shorter ones use the middle.
2. **Search.** Binary-search CQ/CRF from 18 to 40, at most five probes. Each
   probe encodes every window with the real encoder, preset, scale policy, and
   no film grain, then compares it to the lossless window at the output
   resolution.
3. **Sample gate.** A probe passes only when SSIM average is ≥ 0.992 and the
   worst frame is ≥ 0.98. The strongest passing CQ is chosen. If no probe
   passes, Shrinkr keeps the conservative CRF 18 fallback.
4. **Full check.** The complete file must pass a slightly looser gate: SSIM
   average ≥ 0.99 and every frame ≥ 0.97. The output must also be smaller, and
   its duration, audio/subtitle track counts, frame rate, bit depth, HDR color
   tags, audio channels, and sample rates must match.
5. **Retry.** If the full encode fails only the SSIM check and the searched CQ
   is above 18, Shrinkr retries once at CRF 18. If that also fails, the output
   is discarded and the original kept.

Audio is protected separately. A requested target is used only if every track
can retain at least 90% of its source bitrate; lossless codecs and tracks with
unknown bitrate are copied. The final check also compares actual audio packet
totals, so an encoder that undershoots its nominal bitrate cannot slip past
the 10% limit.

Under Preserve quality, all audio tracks and subtitles are kept, synthetic
film grain is disabled, and advanced extra FFmpeg flags are ignored.

The full-file SSIM comparison decodes both files and compares every output
frame to the source resized to the output resolution. That allows deliberate
downscaling while still rejecting visible damage. SSIM is a useful rejection
gate, not a guarantee of invisible differences.

### Target-size solver

Target mode measures the file's own size curve instead of guessing:

- Accepted forms: `400`, `400MB`, `1.2GB`, `800KB`, `2x`, `1.5x`, `50%`.
  Bare numbers mean MB. A ratio must be over 1x; a percentage must be
  1–100%.
- Absolute and relative targets both reduce to an output/input byte ratio.
- Shrinkr cuts up to three 45-second samples across the source (stream-copy,
  no re-encode), encodes them with the exact pipeline settings at
  `min(cq0, 36)` and four points above (capped at 40), and fits
  `ln(ratio) = a + b * crf` by least squares.
- It solves for the CRF that hits the target. If the solution lies more than
  two points outside the probed interval, it encodes one refinement sample at
  the solution and re-fits.
- If even CRF 40 cannot reach the target, the closest reachable result is
  reported instead. Copy-video and targets larger than the source are
  rejected.

The final encode is still a normal encode: it goes through the fallback
ladder, placement guard, and all relevant checks.

### Film grain

Bitrate density is a poor grain detector: clean high-bitrate footage and grainy
footage can look identical in probe numbers. `grain.rs` reads actual frames:

- Analysis runs at 480 px wide and 6 fps, using nearest-neighbour scaling so
  grain amplitude is not averaged away.
- It measures 8×8 blocks. A block is “flat” when its contrast is below 7
  (8-bit levels); grain is estimated only in flat areas, where real detail
  cannot be mistaken for noise.
- At least 4% of blocks must be flat and at least 6 frames must be measured.
  The median sigma across windows determines a starting strength:
  `<1.0` off, `<1.8` 4, `<2.8` 6, `<4.5` 8, `<6.5` 11, otherwise 14,
  clamped to 4–16.

For a real SVT-AV1 encode, the suggested strength and `strength + 4` are
tested against a plain encode on the “hottest” (grainiest) 2-second window. A
candidate is kept only when:

- it is at most 95% of the grain-off size;
- regenerated grain energy is between 0.5× and 1.6× the source's;
- textured-area detail retains at least 55% of the source's energy.

The smallest passing candidate wins. If none passes, grain stays off rather
than smoothing the picture. SVT is fed
`film-grain=N:film-grain-denoise=1` and always encodes 10-bit, which keeps
gradients clean and lets the quality search spend fewer bits for the same SSIM.

## Still images

Images ride the same probe → preflight → temp encode → verify → place flow with
a format-specific decision layer:

| Source | Default behavior |
| --- | --- |
| JPEG | Re-encode with `mjpeg` at the image quality slider |
| WebP (static) | Re-encode with `libwebp`, gated on encoder presence |
| PNG/BMP/TIFF, no alpha | Convert to JPEG — usually the biggest win on near-uncompressed sources |
| PNG/BMP/TIFF with alpha | Convert to WebP (`yuva420p`), keeping transparency |
| ICO | Every size entry is kept and re-encoded as a PNG entry; alpha survives |
| Animated GIF/WebP | Skipped |

Additional rules:

- Files under 16 KiB are left alone.
- A JPEG re-encode that cannot beat its source gets one measured WebP fallback
  at the same quality, kept only if truly smaller.
- An output must be at least ~2% smaller than its source; otherwise it is
  discarded and the original kept.
- EXIF orientation is parsed in Rust and baked into the pixels. If the ffmpeg
  build already rotates on decode (tested with an embedded orientation-6
  probe), no extra filter is added; otherwise explicit transpose/flip filters
  are applied.

JPEG quality maps CQ 18–40 to `-q:v` 2–18 (CQ 28 ≈ q9, roughly JPEG quality
88). WebP maps the same slider to `-quality` 60–95.

**Format preservation** (GUI checkbox, `--image-preserve-format`) keeps the
source format and suppresses conversions and the WebP fallback:

- JPEG: coefficient-domain lossless rewrite with optimized Huffman tables,
  progressive output, and all metadata (including EXIF and ICC) copied.
- PNG: a constant fully-opaque alpha plane is dropped; images with at most 256
  distinct colors become palette PNGs with dithering off (near-lossless,
  measured PSNR above 60 dB); 16-bit depth may drop to 8-bit, disclosed in the
  plan note; otherwise a plain same-format re-encode is attempted and usually
  skips at the default threshold.
- TIFF: deflate re-encode, lossless, often 80–95% off raw scans.
- BMP: honestly skipped — it stores pixels raw and has no compression hook.

ICO keeps its format in both modes.

## Convert

Convert changes the file type rather than optimizing quality. The strategy is
copy-first:

- If the source codecs are legal in the target container, remux with
  `-c copy`. This takes seconds and preserves bytes.
- Otherwise re-encode with sensible defaults. Re-encoding retains all audio
  tracks and maps subtitles to the target container (text subtitles become
  `mov_text` in MP4).
- Video-to-audio extraction intentionally keeps only the primary audio stream
  (`0:a:0`).

Available targets:

| Kind | Targets |
| --- | --- |
| Video | `mp4`, `mkv`, `webm`, `mov`, `avi` |
| Video → audio | `mp3`, `m4a`, `opus`, `ogg`, `flac`, `wav` |
| Audio | `mp3`, `m4a`, `opus`, `ogg`, `flac`, `wav` |
| Image | `jpg`, `png`, `webp`, `gif` |
| Subtitle | `srt`, `vtt`, `ass` |
| Documents | Word → `pdf`/`docx`/`odt`/`txt`/`html`; Spreadsheet → `pdf`/`xlsx`/`ods`/`csv`; Presentation → `pdf`/`pptx`/`odp` |

A target is offered only when this machine can actually produce it: the ffmpeg
muxer and encoder must be present, or LibreOffice must be installed for
documents. The source extension must also differ.

Verification before placement:

- duration must match within 250 ms;
- expected video/audio/subtitle stream counts must remain;
- attachments must survive for video targets;
- image dimensions must match (allowing a 90° orientation swap);
- every output video/audio stream must decode without errors.

If the target cannot retain the selected content or verification fails, the
temporary output is discarded and the original kept. Documents use their
separate LibreOffice checks.

## Optimize

### PDF

PDF shrinking is native, with no external tools:

1. **Images.** Embedded JPEGs (`DCTDecode`) and raw 8-bit RGB/gray images
   (`FlateDecode`) are decoded, downsampled to a display-appropriate long edge,
   re-encoded as JPEG, and written back as `DCTDecode`. An image stream is only
   replaced when the new stream is smaller.
2. **Structure.** Every Flate stream is re-deflated at level 9 when that wins,
   unused objects are pruned, and the document is rewritten with object
   streams and a cross-reference stream. Text, fonts, links, forms,
   annotations, and metadata stay in place.

Presets:

| Preset | Image policy |
| --- | --- |
| Lossless | No image pass; structure/stream cleanup only |
| Balanced | Long edge up to 2200 px, JPEG q80 |
| Small | Long edge up to 1500 px, JPEG q66 |

PDF is treated as hostile input: encrypted or digitally signed files are
refused; masked images, CMYK/ICC, JBIG2/JPX, unsupported filters, absurd
dimensions, and malformed image dictionaries are skipped individually instead
of failing the document. Before placement the output is re-parsed, its page
count is compared with the source, and page content streams are decompressed
to verify them. A rewrite that is not smaller is discarded.

### PNG and JPEG

This is the opposite contract to Shrink: the output must decode to exactly the
same pixels.

- **PNG/APNG** goes through `oxipng` with filter search, palette and bit-depth
  reduction, and deflate. Balanced uses preset 3 with a 20-second budget; Max
  uses preset 6 with a 120-second budget and zopfli (15 iterations). Metadata,
  interlacing, and transparent-pixel values are preserved.
- **JPEG** is re-emitted in the coefficient domain with optimized Huffman
  tables and progressive output, equivalent to `jpegtran -optimize`. All
  markers, EXIF orientation, and ICC profiles are copied.

A structural check confirms valid headers and unchanged dimensions (plus APNG
animation retention). A rewrite that does not beat the source by at least 64
bytes is reported as no-gain and never placed.

## Benchmark and calibration

The benchmark matrix is capability-gated:

| Row | Encoder |
| --- | --- |
| A | x264 CRF 28 baseline |
| B–E | HEVC NVENC CQ 28 at P3, P4, P5, P6 |
| F | AV1 NVENC P5 (if the trial passed) |
| G | SVT-AV1 preset 8 (if available) |

Each row records output size, encode time, average fps, realtime factor, and
average/maximum GPU utilization. Rows can be exported as CSV. The
recommendation picks the smallest NVENC output whose speed is still at least
80% of the fastest NVENC row.

Every successful row also feeds calibration. For each measured
`(settings, ratio)` pair, the model's prediction for the same file and
settings is computed; the per-encoder factor is the mean of
`measured / modeled`. Factors exist for `hevc_nvenc`, `av1_nvenc`, `x264`, and
`svt_av1`, are clamped to `[0.5, 2.0]`, and persist as JSON in the app-data
folder. With no benchmark, all factors are 1.0 and the model behaves exactly
as before.

## GUI, CLI and process control

The GUI has four tabs:

- **Shrink** — videos and stills, quality slider, backend, scale, target size.
- **Convert** — the format matrix, filtered to what the machine can produce.
- **Optimize** — PDF presets and lossless PNG/JPEG.
- **Benchmark** — run the matrix and save CSV.

Long work runs on worker threads. The UI receives progress, logs, per-file
outcomes, and benchmark results as messages; it never blocks on an encode.
Pause holds new files and suspends children already in flight. Cancel kills
the child and reaps it.

### CLI reference

`shrinkr-cli` drives the same core headlessly.

| Flag | Meaning |
| --- | --- |
| `--shrink <file\|dir>` | Run the shrink pipeline (videos + still images) |
| `--optimize <file\|dir>` | Shrink PDFs and losslessly optimize PNG/JPEG |
| `--pdf-level <lossless\|balanced\|small>` | PDF preset (default balanced) |
| `--opt-level <balanced\|max>` | PNG/JPEG effort (default balanced) |
| `--replace` | Delete originals after a verified encode |
| `--images` / `--no-images` | Include still images in `--shrink` (default on) |
| `--image-preserve-format` | Keep image formats; conversions and the WebP fallback are off |
| `--target <400MB\|2x\|50%>` | Solve per-file quality for a size target (video only) |
| `--backend <auto\|av1-nvenc\|hevc\|h264\|vt-hevc\|qsv-hevc\|qsv-av1\|amf-hevc\|amf-av1\|x264\|x265\|av1\|copy>` | Encoder selection (default auto) |
| `--cq <18-40>` | NVENC CQ / x264/x265 CRF / AV1 CRF (default 28) |
| `--scale <preserve\|1080p\|720p\|480p\|WxH>` | Resolution policy (default preserve) |
| `--image-cq <18-40>` | Image quality (default: the `--cq` value) |
| `--image-scale <policy>` | Image resolution (default: the `--scale` value) |
| `--film-grain` / `--no-film-grain` | Measure and synthesize AV1 film grain (default on; SVT-AV1 only) |
| `--audio <off\|8-512>` | Opus bitrate in kbps (default 64; 512 is the stereo max) |
| `--all-audio` / `--primary-audio` | Keep every audio track (default) or only the primary |
| `--keep-subs` / `--no-subs` | Subtitle copy (default keep) |
| `--min-saving <pct>` | Preflight skip threshold (default 10) |
| `--benchmark <file\|dir>` | Run the encoder matrix |
| `--bench-out <dir>` | CSV + output directory (default `./bench-out`) |
| `--extra-args "<flags>"` | Extra ffmpeg output flags, e.g. `-tune grain` |
| `--dry-run` | Print ffmpeg commands without encoding |
| `--probe <file>` | Dump media inspection |
| `--caps` | Print hardware-capability detection |
| `--help` | Full help text |

`--extra-args` is appended after all built-in output flags, so the last
occurrence wins in ffmpeg. Structural flags that would break bookkeeping
(`-i`, `-map`, seeking, progress, output format, …) are rejected. Estimates
intentionally ignore extra flags because they describe a different encode.

## Packaging and self-update

The release workflow builds:

- Windows x64, macOS Apple Silicon, and Linux x64 release binaries (GUI + CLI);
- `shrinkr-<target-triple>.zip` archives containing both binaries, `ffmpeg`,
  `ffprobe`, and `FFMPEG-NOTES.txt`;
- `Shrinkr-Setup-<version>-x64.exe` (Inno Setup) for Windows;
- `shrinkr-<target-triple>.dmg` for macOS.

The Windows installer is per-user (`%LOCALAPPDATA%\Programs\Shrinkr`), needs no
admin prompt, and optionally adds the install directory to `PATH` for
`shrinkr-cli`. The macOS app bundle is ad-hoc signed but not notarized. Linux
builds target Ubuntu 22.04 so their glibc requirement stays older-distro
friendly.

Self-update deliberately does not use the updater crate's single-file install:
portable installs must keep `shrinkr-cli`, `ffmpeg`, and `ffprobe` in sync. It
downloads the archive matching the running binary's target triple, extracts it,
and transactionally moves every file into the executable's directory
(`MoveAll` rolls back if any move fails). Download, extraction, and stash
directories live inside that directory so no rename crosses a filesystem
boundary. The GUI refuses to install while a batch is running.

Every release archive carries an embedded zipsign signature (ed25519, made by
the release workflow with the key whose public half is committed as
`updater-key.pub`), and the updater verifies it against the key embedded in
the binary before a single archive byte is unpacked — a corrupt, replaced, or
unsigned archive fails closed. `SHA256SUMS.txt` on each release covers the
artifacts that carry no embedded signature (DMG, installer). The bundled
ffmpeg/ffprobe sidecars are pinned per build and checksum-verified against
`FFMPEG-PINS.sha256` at release time and for the in-app Windows download;
bumping a pin is documented in that file's header and `FFMPEG-NOTES.txt`.

The Windows installer exists as a separate asset whose name omits the target
triple, so the updater can never select it by mistake.

## GUI shell

The window refuses to close silently while anything runs (shrink, convert,
optimize, update install): Windows asks in a native box before forwarding the
close; macOS/Linux hide on the close attempt and resolve a native dialog
through the pump. User settings (pipeline knobs, codec mutes, convert/optimize
prefs, log filter) persist to `settings.json` in the app-data folder,
debounced in the background and applied before the first render. GPU and
converter capability detection (ffmpeg trial encodes, `soffice --version`)
runs on a warmup thread so the first paint never blocks on subprocesses, and
plan computations (eligible files, goal segments, estimates) are cached behind
a settings+library fingerprint so live progress ticks don't re-run the
preflight math.

Rotated sources (phone clips that store landscape-coded frames plus a display
matrix) scale in display space: the short-side caps, custom boxes, SSIM
references, and the `res_label` the UI shows all describe the picture as a
player presents it, matching ffmpeg's default autorotate that transposes
frames before user filters.

## Tests and CI

CI runs on every push to `master` and every pull request, across:

- Windows x64;
- macOS Apple Silicon;
- macOS Intel (cross-compiled check only — no hosted Intel runner can execute
  the test binaries);
- Linux x64 on Ubuntu 22.04.

Each job installs FFmpeg, runs `cargo check --workspace --all-targets`, then
`cargo test --workspace` (except check-only jobs). Linux jobs also install the
WebKitGTK, GTK, and libxdo development packages needed to link the GUI.

Tests cover estimate math, audio policy, preflight thresholds, sample-window
placement, SSIM gates, solver fitting, grain policy, image plans, PDF
rewrites, JPEG/PNG losslessness, conversion verification, capability helpers,
and process cancellation.

## Troubleshooting

**Windows: `shrinkr.exe` appears in Task Manager but no window appears.**
WebView2 could not open its profile, usually a stale or locked
`%LOCALAPPDATA%\shrinkr` left by an older runtime, an elevated run, or a
sandboxed run. Shrinkr creates a fresh, runtime-versioned profile per launch
under `%LOCALAPPDATA%\shrinkr-webview-*` and falls back to `%TEMP%` or the
app folder. If it still hangs, delete `%LOCALAPPDATA%\shrinkr\EBWebView` and
any `%LOCALAPPDATA%\shrinkr-webview-*` folders, then relaunch.

**macOS blocks the first launch.** The builds are not notarized. Right-click the
app and choose *Open*, or run `xattr -cr .` inside the unzipped folder.

**Linux GUI fails to start.** Install WebKitGTK 4.1 and GTK 3
(`sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0` on Debian/Ubuntu).
`shrinkr-cli` does not need them.

**FFmpeg is missing.** Release archives bundle it. The Windows GUI also offers
a one-click Download FFmpeg button that installs a static build next to the
executable. On macOS/Linux, use a package manager. A source build needs
`ffmpeg` and `ffprobe` on `PATH` or next to the binary.

## Honest limits

- Preserve quality is a perceptual SSIM gate, not mathematical losslessness
  and not a guarantee of zero visible differences. It also adds sample-encode
  time before the real encode.
- Film-grain synthesis regenerates statistically similar grain, not identical
  grain. That is why it remains a visible switch.
- PNG format preservation with a palette is near-lossless: a few palette
  entries can quantize by ±1, though measured PSNR stays above 60 dB.
- PDF shrinking refuses encrypted and digitally signed files, and leaves
  exotic image types untouched rather than risk corrupting them.
- Hardware encoder availability and speed depend on the driver, GPU
  generation, and FFmpeg build. Detection is runtime-based for that reason.
- Size and time estimates are models. Benchmark calibration makes them
  machine-specific, but the real encode remains the source of truth.
