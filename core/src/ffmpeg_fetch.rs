//! One-click FFmpeg acquisition for installs that lack it (e.g. a bare
//! `shrinkr` binary passed around without its sidecars, or a source
//! build). Downloads a static build into the running executable's
//! directory, where [`crate::process::cmd`] looks first.
//!
//! Windows only: it is the one platform with a stable zip-packaged static
//! build URL (BtbN's GitHub-hosted FFmpeg builds). macOS and Linux
//! releases bundle ffmpeg next to the binary, and Linux users have
//! package managers, so those platforms point at the release archive
//! instead of pulling in a tar.xz decoder for a rare path.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// True where [`fetch`] can run.
pub fn supported() -> bool {
    cfg!(target_os = "windows")
}

/// The pinned FFmpeg-Builds artifact this install fetches, as
/// `<tag>/<asset>` under github.com/FFmpeg-Builds/FFmpeg-Builds. A dated
/// autobuild tag is immutable (its asset names are versioned and never
/// replaced) — unlike the floating "latest" tag this URL used to point at.
/// Bumping means a new entry in FFMPEG-PINS.sha256 plus this constant.
const WIN64_TAG: &str = "autobuild-2026-11-13-12-43";
const WIN64_ASSET: &str = "ffmpeg-N-128889-g88c46fce27-win64-gpl.zip";

/// Expected SHA-256 of [`WIN64_ASSET`], read from the repo's pins file —
/// the same source the release workflow verifies its sidecar downloads
/// against, so runtime fetches and release builds can never drift apart.
const PINS: &str = include_str!("../../FFMPEG-PINS.sha256");

/// The SHA-256 (lowercase hex) pinned for `asset`, from `FFMPEG-PINS.sha256`.
fn pinned_sha256(asset: &str) -> Option<String> {
    PINS.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            return None;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        if parts.next()? == asset {
            Some(hash.to_ascii_lowercase())
        } else {
            None
        }
    })
}

/// Streaming SHA-256 of a file, lowercase hex.
fn file_sha256(path: &std::path::Path) -> Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).context("read download for hashing")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Download FFmpeg + ffprobe into the directory of the running
/// executable. `progress` receives `(bytes_downloaded, Some(total))`
/// while streaming. Returns the placed ffmpeg path.
pub fn fetch(progress: impl Fn(u64, Option<u64>) + Send + Sync + 'static) -> Result<PathBuf> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = progress;
        bail!(
            "automatic download is Windows-only — the release archive bundles ffmpeg, \
             or install it with your package manager (macOS: homebrew)"
        );
    }

    #[cfg(target_os = "windows")]
    {
        let exe = std::env::current_exe().context("locate running executable")?;
        let dir = exe
            .parent()
            .context("executable has no parent directory")?
            .to_path_buf();
        // Scratch inside the destination dir: same filesystem, so placing
        // the extracted binaries is a plain rename.
        let scratch = dir.join(format!(".shrinkr-ffmpeg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)
            .with_context(|| format!("create {}", scratch.display()))?;
        let res = fetch_into(&scratch, &dir, progress);
        let _ = std::fs::remove_dir_all(&scratch);
        res
    }
}

#[cfg(target_os = "windows")]
fn fetch_into(
    scratch: &std::path::Path,
    dir: &std::path::Path,
    progress: impl Fn(u64, Option<u64>) + Send + Sync + 'static,
) -> Result<PathBuf> {
    let zip_path = scratch.join("ffmpeg.zip");
    let file = std::fs::File::create(&zip_path).context("create download file")?;
    let url = format!(
        "https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/download/{WIN64_TAG}/{WIN64_ASSET}"
    );
    self_update::Download::from_url(&url)
        .request_header("ACCEPT", "application/octet-stream")
        .progress_callback(progress)
        .download_to(file)
        .context("download ffmpeg build (github.com/FFmpeg-Builds/FFmpeg-Builds)")?;

    // Fail closed before anything from the archive is unpacked, let alone
    // executed: the downloaded bytes must match the pinned checksum.
    let expected = pinned_sha256(WIN64_ASSET).context(
        "FFMPEG-PINS.sha256 has no entry for the pinned ffmpeg build — \
         this is a broken install",
    )?;
    let actual = file_sha256(&zip_path)?;
    if actual != expected {
        bail!(
            "downloaded ffmpeg build does not match its pinned checksum \
             (got {actual}, expected {expected}) — it was not installed. \
             This can mean a corrupted download or a tampered upstream build."
        );
    }

    let staging = scratch.join("files");
    self_update::Extract::from_source(&zip_path)
        .extract_into(&staging)
        .context("unpack ffmpeg build")?;

    // Don't rely on the archive's internal layout: find both binaries
    // wherever they sit.
    let mut ffmpeg: Option<PathBuf> = None;
    let mut ffprobe: Option<PathBuf> = None;
    for entry in walkdir::WalkDir::new(&staging) {
        let entry = entry.map_err(|e| anyhow::anyhow!("scan archive: {e}"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        match entry.file_name().to_string_lossy().to_lowercase().as_str() {
            "ffmpeg.exe" if ffmpeg.is_none() => ffmpeg = Some(entry.path().to_path_buf()),
            "ffprobe.exe" if ffprobe.is_none() => ffprobe = Some(entry.path().to_path_buf()),
            _ => {}
        }
    }
    let ffmpeg = ffmpeg.context("ffmpeg.exe not found in the downloaded build")?;
    let ffprobe = ffprobe.context("ffprobe.exe not found in the downloaded build")?;

    let dest_ffmpeg = dir.join("ffmpeg.exe");
    let dest_ffprobe = dir.join("ffprobe.exe");
    let _ = std::fs::remove_file(&dest_ffmpeg);
    let _ = std::fs::remove_file(&dest_ffprobe);
    std::fs::rename(&ffmpeg, &dest_ffmpeg).context("place ffmpeg.exe")?;
    if let Err(e) = std::fs::rename(&ffprobe, &dest_ffprobe) {
        let _ = std::fs::remove_file(&dest_ffmpeg);
        return Err(e).context("place ffprobe.exe");
    }

    // Sanity-check the freshly placed sidecar before declaring success.
    let runs = crate::process::cmd(&dest_ffmpeg.to_string_lossy())
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !runs {
        let _ = std::fs::remove_file(&dest_ffmpeg);
        let _ = std::fs::remove_file(&dest_ffprobe);
        bail!("the downloaded ffmpeg would not run — it was removed again");
    }
    Ok(dest_ffmpeg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_only_on_windows() {
        assert_eq!(supported(), cfg!(target_os = "windows"));
    }

    #[test]
    fn the_pinned_windows_build_is_in_the_pins_file() {
        let hash = pinned_sha256(WIN64_ASSET).expect("pin exists for the runtime fetch asset");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn pin_lookup_matches_sha256sum_format_and_skips_comments() {
        assert_eq!(pinned_sha256("definitely-not-pinned.zip"), None);
        // The pins file follows `sha256sum -c` format: "<hash>  <name>".
        let first = PINS
            .lines()
            .find(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
            .expect("at least one pin entry");
        let mut parts = first.trim().split_whitespace();
        let (hash, name) = (parts.next().unwrap(), parts.next().unwrap());
        assert_eq!(pinned_sha256(name).as_deref(), Some(hash));
    }
}
