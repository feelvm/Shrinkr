use anyhow::{Context, Result, bail};
use lzma_rust2::{Lzma2Options, Lzma2WriterMt};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{BufWriter, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
    thread,
};

/// Must match `common::PAYLOAD_NAMES`.
const PAYLOAD: &[&str] = &[
    "shrinkr.exe",
    "shrinkr-cli.exe",
    "ffmpeg.exe",
    "ffprobe.exe",
    "FFMPEG-NOTES.txt",
    "LICENSE",
];

fn main() -> Result<()> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let out = PathBuf::from(env::var("OUT_DIR")?);
    let repo = manifest.join("../../..");
    let version = shrinkr_version(&repo)?;
    let (numeric, numeric_text) = numeric_version(&version)?;

    let staging = env::var_os("SHRINKR_STAGING_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join("target/installer/staging"));
    println!("cargo:rerun-if-env-changed=SHRINKR_STAGING_DIR");

    // Debug/clippy builds only need a valid payload, not the smallest one.
    let preset = match env::var("PROFILE").as_deref() {
        Ok("release") => 9,
        _ => 1,
    };
    let mut options = Lzma2Options::with_preset(preset);
    // Fixed-size independent chunks let the encoder use every core. 64 MiB
    // matches the preset-9 dictionary, so the ratio stays close to the
    // single-threaded stream.
    let chunk_size = 64 * 1024 * 1024u64;
    options.set_chunk_size(NonZeroU64::new(chunk_size));
    let dict_size = options.lzma_options.dict_size;
    let workers = thread::available_parallelism()
        .map_or(1, |cores| cores.get())
        .min(16) as u32;

    // Compressing ~360 MB takes minutes; cache it by content so changing the
    // setup code does not re-run the encoder. The cache lives under the repo
    // target directory and is safe to delete at any time.
    let cache_dir = repo.join("target/installer/payload-cache");
    fs::create_dir_all(&cache_dir)?;

    let payload_dir = out.join("payload");
    fs::create_dir_all(&payload_dir)?;
    let mut generated = String::new();
    generated.push_str(&format!("pub const DICT_SIZE: u32 = {dict_size};\n"));
    generated.push_str(
        "pub struct PayloadFile {\n    pub name: &'static str,\n    pub size: u64,\n    pub sha256: [u8; 32],\n    pub data: &'static [u8],\n}\n",
    );
    generated.push_str("pub const PAYLOAD: &[PayloadFile] = &[\n");

    for name in PAYLOAD {
        let path = staging.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let data = fs::read(&path).with_context(|| {
            format!(
                "reading {}; run installer/windows/build-installer.ps1 so the payload is staged",
                path.display()
            )
        })?;

        let mut hasher = Sha256::new();
        hasher.update(&data);
        let digest = hasher.finalize();

        let digest_hex = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let cache_key = format!("{name}-{digest_hex}-p{preset}.lzma2");
        let cache_path = cache_dir.join(&cache_key);
        if !cache_path.is_file() {
            let temp_cache = cache_dir.join(format!(".tmp-{}-{name}.lzma2", std::process::id()));
            compress_into(&temp_cache, &data, &options, workers)?;
            fs::rename(&temp_cache, &cache_path)?;
        }

        let compressed = payload_dir.join(format!("{name}.lzma2"));
        let _ = fs::remove_file(&compressed);
        fs::hard_link(&cache_path, &compressed)
            .or_else(|_| fs::copy(&cache_path, &compressed).map(|_| ()))
            .with_context(|| format!("linking {}", compressed.display()))?;

        generated.push_str(&format!(
            "    PayloadFile {{ name: {name:?}, size: {}, sha256: [{}], data: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/payload/{name}.lzma2\")) }},\n",
            data.len(),
            digest
                .iter()
                .map(|byte| byte.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    generated.push_str("];\n");
    fs::write(out.join("payload.rs"), generated)?;

    let uninstaller = env::var_os("SHRINKR_UNINSTALLER")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../target/release/shrinkr-uninstaller.exe"));
    println!("cargo:rerun-if-env-changed=SHRINKR_UNINSTALLER");
    println!("cargo:rerun-if-changed={}", uninstaller.display());
    fs::copy(&uninstaller, out.join("uninstaller.exe")).with_context(|| {
        format!(
            "copying the uninstaller from {}; build it with \
             `cargo build --release -p shrinkr-uninstaller` first",
            uninstaller.display()
        )
    })?;

    let icon = repo.join("dioxus-app/assets/icons/icon.ico");
    let installer_manifest = manifest.join("../installer.manifest");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed={}", installer_manifest.display());

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
            VALUE \"FileDescription\", \"Shrinkr Setup\"\n\
            VALUE \"FileVersion\", \"{numeric_text}\"\n\
            VALUE \"InternalName\", \"shrinkr-setup\"\n\
            VALUE \"OriginalFilename\", \"shrinkr-setup.exe\"\n\
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
    let rc_path = out.join("setup.rc");
    fs::write(&rc_path, rc)?;
    if let Err(err) = embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional() {
        panic!("embedding the Windows resources failed: {err:?}");
    }

    Ok(())
}

fn forward(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn compress_into(path: &Path, data: &[u8], options: &Lzma2Options, workers: u32) -> Result<()> {
    let file = fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = Lzma2WriterMt::new(BufWriter::new(file), options.clone(), workers)?;
    encoder.write_all(data)?;
    let mut writer = encoder.finish()?;
    writer.flush()?;
    Ok(())
}

fn shrinkr_version(repo: &Path) -> Result<String> {
    println!("cargo:rerun-if-env-changed=SHRINKR_VERSION");
    let manifest = repo.join("dioxus-app/Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    if let Ok(version) = env::var("SHRINKR_VERSION")
        && !version.is_empty()
    {
        return Ok(version);
    }
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    package_version(&text).context("finding the shrinkr package version")
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

/// Four-part numeric form for `FILEVERSION` and the version resource.
fn numeric_version(version: &str) -> Result<(String, String)> {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = Vec::new();
    for part in core.split('.') {
        let number: u32 = part
            .parse()
            .with_context(|| format!("version '{version}' has a non-numeric component"))?;
        if number > 65535 {
            bail!("version '{version}' has an out-of-range component");
        }
        parts.push(number);
    }
    if parts.is_empty() || parts.len() > 4 {
        bail!("version '{version}' must have one to four numeric components");
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
    Ok((numeric, text))
}
