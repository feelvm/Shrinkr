use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest.join("../../..");
    let version = shrinkr_version(&repo);
    let (numeric, numeric_text) = numeric_version(&version);

    let icon = repo.join("dioxus-app/assets/icons/icon.ico");
    let installer_manifest = manifest.join("../installer.manifest");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed={}", installer_manifest.display());
    println!(
        "cargo:rerun-if-changed={}",
        repo.join("dioxus-app/Cargo.toml").display()
    );
    println!("cargo:rerun-if-env-changed=SHRINKR_VERSION");

    let rc = format!(
        "1 ICON \"{}\"\n\
         1 24 \"{}\"\n\
         1 VERSIONINFO\n\
          FILEVERSION {numeric}\n\
          PRODUCTVERSION {numeric}\n\
          FILEOS 0x40004\n\
          FILETYPE 0x1\n\
         BEGIN\n\
          BLOCK \"StringFileInfo\"\n\
          BEGIN\n\
           BLOCK \"040904B0\"\n\
           BEGIN\n\
            VALUE \"CompanyName\", \"Shrinkr\"\n\
            VALUE \"FileDescription\", \"Shrinkr Uninstaller\"\n\
            VALUE \"FileVersion\", \"{numeric_text}\"\n\
            VALUE \"InternalName\", \"shrinkr-uninstaller\"\n\
            VALUE \"OriginalFilename\", \"shrinkr-uninstaller.exe\"\n\
            VALUE \"ProductName\", \"Shrinkr\"\n\
            VALUE \"ProductVersion\", \"{numeric_text}\"\n\
            VALUE \"LegalCopyright\", \"Copyright (C) 2026 feelvm\"\n\
           END\n\
          END\n\
          BLOCK \"VarFileInfo\"\n\
          BEGIN\n\
           VALUE \"Translation\", 0x409, 1200\n\
          END\n\
         END\n",
        forward(&icon),
        forward(&installer_manifest),
    );
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc_path = out.join("uninstaller.rc");
    fs::write(&rc_path, rc).expect("writing the uninstaller resource script");
    if let Err(err) = embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional() {
        panic!("embedding the Windows resources failed: {err:?}");
    }
}

fn forward(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn shrinkr_version(repo: &Path) -> String {
    if let Ok(version) = env::var("SHRINKR_VERSION")
        && !version.is_empty()
    {
        return version;
    }
    let manifest = repo.join("dioxus-app/Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|err| panic!("reading {}: {err}", manifest.display()));
    package_version(&text).unwrap_or_else(|| "0.0.0".to_owned())
}

/// Pulls the first `version = "..."` out of the `[package]` table.
fn package_version(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("version") else {
            continue;
        };
        let rest = rest.trim_start().strip_prefix('=')?.trim();
        return Some(rest.strip_prefix('"')?.strip_suffix('"')?.to_owned());
    }
    None
}

fn numeric_version(version: &str) -> (String, String) {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts: Vec<u32> = core
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    if parts.is_empty() || parts.len() > 4 {
        parts = vec![0, 0, 0, 0];
    }
    while parts.len() < 4 {
        parts.push(0);
    }
    let text = parts
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".");
    let numeric = parts
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    (numeric, text)
}
