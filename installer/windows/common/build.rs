use std::{env, fs, path::Path};

const VERSION_ENV: &str = "SHRINKR_VERSION";

fn main() {
    println!("cargo:rerun-if-env-changed={VERSION_ENV}");

    // The release script passes the tag's version; a plain cargo build falls
    // back to the GUI package's manifest so both stay in lockstep.
    let manifest =
        Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../../dioxus-app/Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest.display());

    let version = env::var(VERSION_ENV)
        .ok()
        .filter(|version| !version.is_empty())
        .or_else(|| {
            fs::read_to_string(&manifest)
                .ok()
                .and_then(|text| package_version(&text))
        })
        .unwrap_or_else(|| "0.0.0".to_owned());

    println!("cargo:rustc-env={VERSION_ENV}={version}");
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
        let rest = rest.strip_prefix('"')?.strip_suffix('"')?;
        return Some(rest.to_owned());
    }
    None
}
