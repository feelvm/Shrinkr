//! The per-user `PATH` entry the setup can add for `shrinkr-cli`. The value
//! is normally `REG_EXPAND_SZ`, so it is edited through the raw registry value
//! to preserve its type and any `%VAR%` expansions.

use crate::broadcast_environment_change;
use anyhow::{Context, Result};
use std::path::Path;
use winreg::RegKey;
use winreg::RegValue;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RegType};

const ENVIRONMENT_KEY: &str = "Environment";
const PATH_VALUE: &str = "Path";

pub fn path_contains(dir: &Path) -> bool {
    read_path()
        .map(|(text, _)| contains(&text, dir))
        .unwrap_or(false)
}

/// Adds `dir` to the user PATH. Returns whether the value changed.
pub fn path_add(dir: &Path) -> Result<bool> {
    let (text, vtype) = read_path()?;
    if contains(&text, dir) {
        return Ok(false);
    }

    let mut updated = text;
    if !updated.is_empty() && !updated.ends_with(';') {
        updated.push(';');
    }
    updated.push_str(&dir.display().to_string());
    write_path(&updated, vtype)?;
    broadcast_environment_change();
    Ok(true)
}

/// Removes `dir` from the user PATH. Returns whether the value changed.
pub fn path_remove(dir: &Path) -> Result<bool> {
    let (text, vtype) = read_path()?;
    let entries: Vec<&str> = text.split(';').collect();
    let kept: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|entry| !entry.is_empty() && !matches_dir(entry, dir))
        .collect();
    if kept.len() == entries.iter().filter(|entry| !entry.is_empty()).count() {
        return Ok(false);
    }

    let updated = kept.join(";");
    if updated.is_empty() {
        let hive = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(environment) = hive.open_subkey_with_flags(ENVIRONMENT_KEY, KEY_WRITE) {
            let _ = environment.delete_value(PATH_VALUE);
        }
    } else {
        write_path(&updated, vtype)?;
    }
    broadcast_environment_change();
    Ok(true)
}

fn read_path() -> Result<(String, RegType)> {
    let hive = RegKey::predef(HKEY_CURRENT_USER);
    let environment = hive
        .open_subkey_with_flags(ENVIRONMENT_KEY, KEY_READ)
        .unwrap_or_else(|_| RegKey::predef(HKEY_CURRENT_USER));
    match environment.get_raw_value(PATH_VALUE) {
        Ok(value) => Ok((decode(&value.bytes), value.vtype)),
        Err(_) => Ok((String::new(), RegType::REG_EXPAND_SZ)),
    }
}

fn write_path(text: &str, vtype: RegType) -> Result<()> {
    let hive = RegKey::predef(HKEY_CURRENT_USER);
    let (environment, _) = hive
        .create_subkey(ENVIRONMENT_KEY)
        .context("opening the user Environment registry key")?;
    let vtype = match vtype {
        RegType::REG_SZ => RegType::REG_SZ,
        _ => RegType::REG_EXPAND_SZ,
    };
    let bytes = text
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<u8>>();
    environment
        .set_raw_value(
            PATH_VALUE,
            &RegValue {
                bytes: bytes.into(),
                vtype,
            },
        )
        .context("writing the user PATH")
}

fn contains(text: &str, dir: &Path) -> bool {
    text.split(';').any(|entry| matches_dir(entry, dir))
}

fn matches_dir(entry: &str, dir: &Path) -> bool {
    normalize(entry.trim().trim_matches('"')) == normalize(&dir.display().to_string())
}

fn normalize(value: &str) -> String {
    value.trim_end_matches(['\\', '/']).to_ascii_lowercase()
}

fn decode(bytes: &[u8]) -> String {
    let words: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[lo, hi]| u16::from_le_bytes([*lo, *hi]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&words)
}
