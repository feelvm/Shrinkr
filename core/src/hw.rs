//! Runtime hardware-capability detection.
//!
//! We deliberately avoid hard-coding GPU model names. Encoder support is
//! determined by asking FFmpeg/NVIDIA what actually works:
//!
//! * `ffmpeg -hwaccels` → is `cuda` hwaccel present?
//! * `ffmpeg -encoders` → are `hevc_nvenc` / `h264_nvenc` / `av1_nvenc` listed?
//! * trial encode → can `av1_nvenc` actually initialise on this GPU?
//!   (RTX 3060 lists `av1_nvenc` but has no AV1 HW encoder, so the trial
//!   fails and we correctly avoid AV1 there while a future RTX 40/50 box
//!   would pass and unlock the AV1 path without code changes.)

use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct HwCaps {
    pub cuda_hwaccel: bool,
    pub hevc_nvenc: bool,
    pub h264_nvenc: bool,
    /// Encoder listed by ffmpeg (may still lack HW on this GPU).
    pub av1_nvenc_listed: bool,
    /// True only if a real trial encode with av1_nvenc initialises.
    pub av1_nvenc_usable: bool,
    /// SVT-AV1 software encoder present. No trial needed: if listed it
    /// runs (no GPU involved — this is the Ryzen path on RTX 30 boxes).
    pub svt_av1: bool,
    pub cuvid_decoders: Vec<String>,
    pub gpu_label: String,
}

impl HwCaps {
    pub fn any_nvenc(&self) -> bool {
        self.hevc_nvenc || self.h264_nvenc
    }

    pub fn summary(&self) -> String {
        format!(
            "cuda={} hevc_nvenc={} h264_nvenc={} av1_nvenc={}({}) svt_av1={} gpu={}",
            flag(self.cuda_hwaccel),
            flag(self.hevc_nvenc),
            flag(self.h264_nvenc),
            flag(self.av1_nvenc_usable),
            if self.av1_nvenc_listed {
                "listed"
            } else {
                "absent"
            },
            flag(self.svt_av1),
            self.gpu_label
        )
    }
}

fn flag(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

static CAPS: OnceLock<HwCaps> = OnceLock::new();

/// Cached detection (runs once per process).
pub fn caps() -> &'static HwCaps {
    CAPS.get_or_init(detect)
}

pub fn detect() -> HwCaps {
    let hwaccels = cmd_out("ffmpeg", &["-hide_banner", "-hwaccels"]);
    let cuda_hwaccel = hwaccels.to_lowercase().contains("cuda");

    let encoders = cmd_out("ffmpeg", &["-hide_banner", "-encoders"]);
    let enc_lower = encoders.to_lowercase();
    let hevc_nvenc = enc_lower.contains("hevc_nvenc");
    let h264_nvenc = enc_lower.contains("h264_nvenc");
    let av1_nvenc_listed = enc_lower.contains("av1_nvenc");
    let svt_av1 = enc_lower.contains("libsvtav1");

    let decoders = cmd_out("ffmpeg", &["-hide_banner", "-decoders"]).to_lowercase();
    let mut cuvid_decoders = vec![];
    for name in [
        "h264_cuvid",
        "hevc_cuvid",
        "mpeg4_cuvid",
        "mpeg2_cuvid",
        "vp9_cuvid",
        "av1_cuvid",
        "vc1_cuvid",
    ] {
        if decoders.contains(name) {
            cuvid_decoders.push(name.to_string());
        }
    }

    // Trial encode decides AV1 usability (RTX 20/30 → fails, RTX 40/50 → passes).
    let av1_nvenc_usable = if av1_nvenc_listed {
        trial_encoder_works("av1_nvenc")
    } else {
        false
    };

    let gpu_label = gpu_label();

    HwCaps {
        cuda_hwaccel,
        hevc_nvenc,
        h264_nvenc,
        av1_nvenc_listed,
        av1_nvenc_usable,
        svt_av1,
        cuvid_decoders,
        gpu_label,
    }
}

fn cmd_out(prog: &str, args: &[&str]) -> String {
    crate::process::cmd(prog)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Try to initialise `encoder` on a 6-frame synthetic clip.
/// Returns true only if ffmpeg exits 0.
fn trial_encoder_works(encoder: &str) -> bool {
    let mut child = match crate::process::cmd("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=128x128:rate=15:duration=0.4",
            "-c:v",
            encoder,
            "-preset",
            "p1",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    // Bound the trial so a hung driver can't hang the app.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

fn gpu_label() -> String {
    let out = crate::process::cmd("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "unknown".into()
            } else {
                s.lines().next().unwrap_or("unknown").trim().to_string()
            }
        }
        _ => "none".into(),
    }
}

/// Sample average/max GPU utilisation (%) while `running` is true.
/// Returns (avg, max, samples). Yields (0,0,0) when nvidia-smi is missing.
pub fn sample_gpu_util(
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (u32, u32, usize) {
    let mut vals: Vec<u32> = vec![];
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        if let Ok(o) = crate::process::cmd("nvidia-smi")
            .args([
                "--query-gpu=utilization.gpu",
                "--format=csv,noheader,nounits",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout);
                if let Some(line) = s.lines().next() {
                    if let Ok(v) = line.trim().parse::<u32>() {
                        vals.push(v.min(100));
                    }
                }
            } else {
                break; // no NVIDIA driver
            }
        } else {
            break;
        }
        std::thread::sleep(Duration::from_millis(750));
    }
    if vals.is_empty() {
        (0, 0, 0)
    } else {
        let sum: u64 = vals.iter().map(|&v| v as u64).sum();
        let avg = (sum / vals.len() as u64) as u32;
        let max = *vals.iter().max().unwrap();
        (avg, max, vals.len())
    }
}
