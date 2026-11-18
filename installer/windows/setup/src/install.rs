use anyhow::{Context, Result, bail};
use common::{APP_NAME, UNINSTALLER_EXE};
use lzma_rust2::Lzma2Reader;
use sha2::{Digest, Sha256};
use std::{
    fmt, fs,
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use crate::progress::Progress;

mod payload {
    include!(concat!(env!("OUT_DIR"), "/payload.rs"));
}

const UNINSTALLER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/uninstaller.exe"));
const CHUNK: usize = 1024 * 1024;

/// The app executable is locked by a running instance, so the swap has to
/// wait for it to close.
#[derive(Debug)]
pub struct AppRunning;

impl fmt::Display for AppRunning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Shrinkr is running")
    }
}

impl std::error::Error for AppRunning {}

/// The user pressed Cancel in the progress window.
#[derive(Debug)]
pub struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("setup was canceled")
    }
}

impl std::error::Error for Canceled {}

pub fn install(
    dir: &Path,
    desktop: bool,
    add_path: bool,
    had_desktop: bool,
    show_progress: bool,
) -> Result<()> {
    let created = !dir.exists();
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    // Fail before unpacking ~360 MB if any target is running. The rename
    // phase checks again, but this avoids doing the work just to fail.
    for target in targets(dir) {
        if is_running(&target) {
            return Err(AppRunning.into());
        }
    }

    let total =
        payload::PAYLOAD.iter().map(|file| file.size).sum::<u64>() + UNINSTALLER.len() as u64;
    let mut progress = Progress::start(&format!("Installing {APP_NAME}"), total, show_progress);

    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = extract_all(dir, &mut progress, &mut staged);

    if let Err(err) = result {
        // Nothing is committed until every `.new` file is complete, so a
        // failure or cancellation leaves the current install untouched.
        for (temp, _) in &staged {
            let _ = fs::remove_file(temp);
        }
        if created {
            let _ = fs::remove_dir(dir);
        }
        return Err(err);
    }

    // Commit. A running binary shows up as access denied or a sharing
    // violation here; the caller offers Retry.
    for (temp, target) in &staged {
        replace(temp, target)?;
    }

    crate::shortcut::create(dir, desktop, had_desktop)?;

    let path_present = if add_path {
        common::path_add(dir)?;
        true
    } else {
        common::path_contains(dir)
    };

    let size_kb = total / 1024;
    common::write_uninstall_entry(dir, size_kb as u32, desktop, path_present)?;

    if let Some(old) = common::legacy_install_dir()
        && same_dir(&old, dir)
    {
        for name in ["unins000.exe", "unins000.dat", "unins000.msg"] {
            let _ = fs::remove_file(old.join(name));
        }
        common::remove_legacy_uninstall_entry();
    }

    Ok(())
}

/// Windows paths compare case-insensitively, and Inno records its install
/// path with a trailing backslash.
fn same_dir(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

fn extract_all(
    dir: &Path,
    progress: &mut Progress,
    staged: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    for file in payload::PAYLOAD {
        let temp = dir.join(format!("{}.new", file.name));
        let target = dir.join(file.name);
        staged.push((temp.clone(), target));

        progress.set_line(file.name);
        let handle = File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
        let mut writer = BufWriter::new(handle);
        let mut reader = Lzma2Reader::new(file.data, payload::DICT_SIZE, None);
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut buffer = vec![0u8; CHUNK];

        loop {
            let read = reader
                .read(&mut buffer)
                .with_context(|| format!("unpacking {}", file.name))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            writer.write_all(&buffer[..read])?;
            written += read as u64;
            progress.advance(read as u64);
            if progress.cancelled() {
                return Err(Canceled.into());
            }
        }
        writer.flush()?;
        drop(writer);

        if written != file.size || hasher.finalize().as_slice() != &file.sha256[..] {
            bail!("the installer payload for {} is corrupt", file.name);
        }
    }

    let temp = dir.join(format!("{UNINSTALLER_EXE}.new"));
    let target = dir.join(UNINSTALLER_EXE);
    staged.push((temp.clone(), target));
    progress.set_line(UNINSTALLER_EXE);
    fs::write(&temp, UNINSTALLER).with_context(|| format!("writing {}", temp.display()))?;
    progress.advance(UNINSTALLER.len() as u64);

    Ok(())
}

fn targets(dir: &Path) -> Vec<PathBuf> {
    payload::PAYLOAD
        .iter()
        .map(|file| dir.join(file.name))
        .chain(std::iter::once(dir.join(UNINSTALLER_EXE)))
        .collect()
}

fn is_running(exe: &Path) -> bool {
    match OpenOptions::new().write(true).open(exe) {
        Ok(_) => false,
        // Windows reports a file held open without write sharing as a sharing
        // or lock violation; `ErrorKind` maps both to `Uncategorized`.
        Err(err) => matches!(err.raw_os_error(), Some(32) | Some(33)),
    }
}

fn replace(temp: &Path, target: &Path) -> Result<()> {
    if let Err(err) = fs::rename(temp, target) {
        let _ = fs::remove_file(temp);
        // Replacing a running image fails with access denied; a sharing or
        // lock violation is the other way Windows reports it.
        if matches!(err.raw_os_error(), Some(5) | Some(32) | Some(33)) {
            return Err(AppRunning.into());
        }
        return Err(err).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}
