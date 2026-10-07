//! Self-update via GitHub Releases (`self_update` crate).
//!
//! Setup (once your public repo exists):
//! 1. Set `REPO_OWNER` / `REPO_NAME` below to your `owner/repo`.
//! 2. Tag releases as `v0.x.y` (e.g. `v0.2.0`) — the tag MUST be valid
//!    semver with a leading `v`; the updater compares it against the
//!    running binary's `CARGO_PKG_VERSION`.
//! 3. Attach one archive per release target, named `shrinkr-<triple>.zip`
//!    (e.g. `shrinkr-x86_64-pc-windows-msvc.zip`), each with all shipped
//!    files at its root — the release workflow does this: the GUI binary,
//!    `shrinkr-cli`, and the bundled `ffmpeg` / `ffprobe` sidecars.
//!
//! Matching works without an `asset_identifier`: the crate requires the
//! asset name to contain the running binary's compile-time target triple,
//! so every platform downloads exactly its own archive and a missing one
//! fails as "no release found for target" instead of installing another
//! platform's file.
//!
//! Installation is multi-file on purpose: `Update::update()` would
//! replace only the GUI binary, silently stripping the bundled ffmpeg
//! sidecars and the CLI from portable installs. Instead we download and
//! extract the archive ourselves and transactionally move *every* file
//! into the running executable's directory (`MoveAll` rolls back
//! completely if any move fails). The download, extraction, and stash
//! dirs all live inside that directory so no rename can cross a
//! filesystem boundary.
//!
//! GUI rules: never block the UI thread (callers run these on worker
//! threads and report back via `JobMsg`), never prompt on stdin, and
//! never install while an encode/convert batch is running (checked in the
//! UI layer).

use anyhow::{bail, Context, Result};
use std::path::Path;

/// GitHub owner of the public repo (github.com/feelvm/Shrinkr).
pub const REPO_OWNER: &str = "feelvm";
/// GitHub repo name.
pub const REPO_NAME: &str = "Shrinkr";
/// Binary name inside the release archives (`shrinkr.exe` on Windows,
/// `shrinkr` elsewhere — the platform exe suffix is appended by the crate).
pub const BIN_NAME: &str = "shrinkr";

/// True when the constants above were left as placeholders.
pub fn is_configured() -> bool {
    REPO_OWNER != "YOUR_GITHUB_USER"
}

/// Info about an available update (check-only, nothing downloaded).
#[derive(Clone, Debug)]
pub struct AvailableUpdate {
    pub version: String,
    pub notes: String,
}

fn updater(current_version: &str) -> Result<self_update::backends::github::Update> {
    if !is_configured() {
        bail!("updates not configured yet — set REPO_OWNER in core/src/update.rs");
    }
    self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(current_version)
        // No `asset_identifier` (crate default `None`): matching then
        // requires the asset name to contain the compile-time target
        // triple, so each platform picks its own `shrinkr-<triple>.zip`
        // and nothing else. Setting an identifier would enable a plain
        // substring fallback that, with multi-platform releases, could
        // select another platform's asset when this one is missing.
        // The update check reports the newest release; without this the
        // default "compatible" strategy could install an older same-major
        // one instead of what the UI just showed the user.
        .update_strategy(self_update::UpdateStrategy::Latest)
        // GUI: no stdout spam, no interactive yes/no prompt (1.x defaults
        // are interactive and would stall without a terminal).
        .show_output(false)
        .no_confirm(true)
        .show_download_progress(false)
        .build()
        .context("build updater")
}

/// Check GitHub Releases for a strictly-newer version. Returns `Ok(None)`
/// when up to date.
pub fn check_for_update(current_version: &str) -> Result<Option<AvailableUpdate>> {
    let upd = updater(current_version)?;
    match upd.is_update_available().context("check releases")? {
        Some(release) => Ok(Some(AvailableUpdate {
            version: release.version().to_string(),
            notes: release.body().unwrap_or_default().chars().take(500).collect(),
        })),
        None => Ok(None),
    }
}

/// Newest release carrying an asset for this platform.
fn latest_release() -> Result<self_update::update::Release> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        // Same match rule the update check uses: only releases with an
        // asset named for this target triple, and only one API walk.
        .filter_target(self_update::get_target())
        .build()
        .context("build release list")?
        .fetch()
        .context("list releases")?;
    releases
        .latest()
        .cloned()
        .context("no release with an archive for this platform")
}

/// Download + install the latest release in place: every file in the
/// archive (GUI binary, CLI, ffmpeg/ffprobe sidecars) lands next to the
/// running executable. Returns the new version.
/// The running process keeps executing the OLD code until it exits — the
/// caller must prompt for restart (see [`restart_now`]).
pub fn install_update(current_version: &str) -> Result<String> {
    if !is_configured() {
        bail!("updates not configured yet — set REPO_OWNER in core/src/update.rs");
    }
    let release = latest_release()?;
    let new_version = release.version().to_string();
    match self_update::version::bump_is_greater(current_version, &new_version) {
        Ok(true) => {}
        Ok(false) => bail!("already up to date (v{current_version})"),
        Err(e) => return Err(anyhow::Error::new(e).context("compare versions")),
    }
    let asset = release
        .asset_for(self_update::get_target(), None)
        .context("release has no archive for this platform")?
        .download_url()
        .to_string();

    let exe = std::env::current_exe().context("locate running executable")?;
    let install_dir = exe
        .parent()
        .context("executable has no parent directory")?
        .to_path_buf();

    // Everything happens inside one scratch dir next to the install
    // target: same filesystem for the extraction, the MoveAll stash, and
    // the final renames.
    // Sweep scratch dirs left by update attempts from other runs (their
    // pid suffix means they never collide with ours). On Windows the dir
    // of a *running* updater keeps its stashed copy of the then-running
    // binary; once that process is gone the deletion succeeds.
    if let Ok(entries) = std::fs::read_dir(&install_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let stale = name.to_string_lossy().starts_with(".shrinkr-update-");
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    let scratch = install_dir.join(format!(".shrinkr-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)
            .with_context(|| format!("create {}", scratch.display()))?;
    let result = install_from_asset(&asset, &scratch, &install_dir);
    // Best-effort: on Windows the stashed copy of the *running* binary
    // cannot be deleted until this process exits, so the dir can linger
    // until the next update sweeps it.
    let _ = std::fs::remove_dir_all(&scratch);
    result.map(|_| new_version)
}

fn install_from_asset(url: &str, scratch: &Path, install_dir: &Path) -> Result<()> {
    let zip_path = scratch.join("update.zip");
    let file = std::fs::File::create(&zip_path).context("create download file")?;
    self_update::Download::from_url(url)
        .request_header("ACCEPT", "application/octet-stream")
        .download_to(file)
        .context("download release archive")?;
    install_from_archive(&zip_path, scratch, install_dir)
}

/// Extract an already-downloaded archive and transactionally install
/// every file it contains into `install_dir`.
fn install_from_archive(zip_path: &Path, scratch: &Path, install_dir: &Path) -> Result<()> {
    let staging = scratch.join("files");
    self_update::Extract::from_source(zip_path)
        .extract_into(&staging)
        .context("unpack release archive")?;

    // Install whatever the archive ships (flat layout: the GUI binary,
    // the CLI, the ffmpeg sidecars) — nothing hardcoded, so the archive
    // can grow files without the updater learning new names.
    // MoveAll stashes displaced files under this dir (same filesystem by
    // construction) and expects it to exist.
    let stash = scratch.join("stash");
    std::fs::create_dir_all(&stash).with_context(|| format!("create {}", stash.display()))?;
    let mut move_all = self_update::MoveAll::from_temp(&stash);
    let mut installed: Vec<std::path::PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&staging).context("read unpacked archive")? {
        let entry = entry.context("read unpacked archive")?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let dest = install_dir.join(entry.file_name());
        move_all.add(entry.path(), dest.clone());
        installed.push(dest);
    }
    if installed.is_empty() {
        bail!("release archive is empty");
    }
    move_all
        .commit()
        .context("install update files (nothing was changed)")?;

    // zip entries keep their unix mode through extraction, but don't bet
    // the app on the archive having been built with +x set.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in &installed {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        }
    }
    Ok(())
}

/// Relaunch the (newly replaced) executable and exit this process.
/// On unix this execs (same PID); on Windows it spawns + exits.
pub fn restart_now() -> Result<()> {
    // `restart()` returns `Result<Infallible>` (Ok never happens: unix
    // execs, Windows spawns + exits). The `if let` is irrefutable by
    // construction — keep it explicit rather than unwrap.
    #[allow(irrefutable_let_patterns)]
    if let Err(e) = self_update::restart::restart() {
        bail!("restart failed: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_is_configured() {
        assert!(is_configured());
    }

    /// Build a flat zip with the given (name, contents) files.
    fn write_zip(path: &Path, files: &[(&str, &str)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, contents) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut zip, contents.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn installs_every_archive_file_and_replaces_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let install_dir = tmp.path().join("install");
        std::fs::create_dir_all(&install_dir).unwrap();
        // A previous version of one file already sits in the install
        // dir; the install must replace it cleanly.
        std::fs::write(install_dir.join("shrinkr"), "old binary").unwrap();

        let zip_path = tmp.path().join("update.zip");
        write_zip(&zip_path, &[("shrinkr", "new binary"), ("ffmpeg", "ff")]);

        let scratch = tmp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        install_from_archive(&zip_path, &scratch, &install_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(install_dir.join("shrinkr")).unwrap(),
            "new binary"
        );
        assert_eq!(
            std::fs::read_to_string(install_dir.join("ffmpeg")).unwrap(),
            "ff"
        );
    }

    #[test]
    fn empty_archive_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let install_dir = tmp.path().join("install");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("shrinkr"), "old binary").unwrap();

        let zip_path = tmp.path().join("update.zip");
        write_zip(&zip_path, &[]);

        let scratch = tmp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        assert!(install_from_archive(&zip_path, &scratch, &install_dir).is_err());
        // The failed install must not have touched the existing file.
        assert_eq!(
            std::fs::read_to_string(install_dir.join("shrinkr")).unwrap(),
            "old binary"
        );
    }
}
