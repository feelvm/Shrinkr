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

/// BtbN FFmpeg-Builds "latest" win64 GPL zip — the URL is permanent and
/// always points at the newest build; `ffmpeg.exe` / `ffprobe.exe` live
/// under `bin/` inside the archive.
const WIN64_URL: &str =
    "https://github.com/BtbN/FFmpeg-Builds/releases/latest/download/ffmpeg-master-latest-win64-gpl.zip";

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
    self_update::Download::from_url(WIN64_URL)
        .request_header("ACCEPT", "application/octet-stream")
        .progress_callback(progress)
        .download_to(file)
        .context("download ffmpeg build (github.com/BtbN/FFmpeg-Builds)")?;

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
}
