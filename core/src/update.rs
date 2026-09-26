//! Self-update via GitHub Releases (`self_update` crate).
//!
//! Setup (once your public repo exists):
//! 1. Set `REPO_OWNER` / `REPO_NAME` below to your `owner/repo`.
//! 2. Tag releases as `v0.x.y` (e.g. `v0.2.0`) — the tag MUST be valid
//!    semver with a leading `v`; the updater compares it against the
//!    running binary's `CARGO_PKG_VERSION`.
//! 3. The release workflow (`.github/workflows/release.yml`) uploads a
//!    zip named `shrinkr-<target>.zip` containing
//!    `shrinkr.exe`. `self_update` picks the asset matching
//!    the current target triple automatically.
//!
//! GUI rules: never block the UI thread (callers run these on worker
//! threads and report back via `JobMsg`), never prompt on stdin
//! (`no_confirm(true)` + `show_output(false)`), and never install while
//! an encode/convert batch is running (checked in the UI layer).

use anyhow::{bail, Context, Result};

/// GitHub owner of the public repo (github.com/feelvm/Shrinkr).
pub const REPO_OWNER: &str = "feelvm";
/// GitHub repo name.
pub const REPO_NAME: &str = "Shrinkr";
/// Binary name as shipped inside the release zip.
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

/// Download + install the latest release in place. Returns the new version.
/// The running process keeps executing the OLD code until it exits — the
/// caller must prompt for restart (see [`restart_now`]).
pub fn install_update(current_version: &str) -> Result<String> {
    let upd = updater(current_version)?;
    let status = upd.update().context("download/install update")?;
    Ok(status.version().to_string())
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
