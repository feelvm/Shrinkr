//! Runtime hardware-capability detection.
//!
//! We deliberately avoid hard-coding GPU model names. Encoder support is
//! determined by asking FFmpeg / the GPU vendor stack what actually works:
//!
//! * `ffmpeg -hwaccels` → which decode accelerators exist (cuda, qsv,
//!   d3d11va, videotoolbox, vaapi, …). CPU encode levels use this for a
//!   decode assist (`-hwaccel auto`).
//! * `ffmpeg -encoders` → are the NVENC / VideoToolbox / Quick Sync / AMF
//!   encoders listed?
//! * trial encodes → do the AV1 variants (and NVENC `-b_ref_mode`) really
//!   initialise on this GPU *with the flags Shrinkr passes*? `-encoders`
//!   lists `av1_nvenc` on an RTX 3060 that has no AV1 encoder, and AMF
//!   rejects `cqp` modes it can't run — the trial fails and the backend
//!   is hidden rather than offered and broken.

use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct HwCaps {
    // ── NVIDIA NVENC (+ NVDEC/CUDA) ──
    pub cuda_hwaccel: bool,
    pub hevc_nvenc: bool,
    pub h264_nvenc: bool,
    /// Encoder listed by ffmpeg (may still lack HW on this GPU).
    pub av1_nvenc_listed: bool,
    /// True only if a real trial encode with av1_nvenc (and Shrinkr's
    /// full flag set, incl. `-b_ref_mode middle`) initialises.
    pub av1_nvenc_usable: bool,
    /// NVENC accepts `-b_ref_mode middle` (B-frames as references,
    /// Turing+). Trial-gated: Pascal hard-errors on the flag, which would
    /// otherwise burn a full encode before the fallback fires.
    pub nvenc_b_ref_mode: bool,
    // ── Apple VideoToolbox (macOS builds; `-q:v` quality mode is
    //    Apple-Silicon-only in ffmpeg, which matches the release target) ──
    pub vt_h264: bool,
    pub vt_hevc: bool,
    // ── Intel Quick Sync ──
    pub qsv_h264: bool,
    pub qsv_hevc: bool,
    pub qsv_av1_listed: bool,
    /// Trial-gated with the real ICQ flags (`-global_quality`).
    pub qsv_av1_usable: bool,
    // ── AMD AMF ──
    pub amf_h264: bool,
    pub amf_hevc: bool,
    pub amf_av1_listed: bool,
    /// Trial-gated with the real CQP flags (`-rc cqp -qp_i/-qp_p`).
    pub amf_av1_usable: bool,
    // ── CPU ──
    /// SVT-AV1 software encoder present. No trial needed: if listed it
    /// runs (no GPU involved — this is the Ryzen path on RTX 30 boxes).
    pub svt_av1: bool,
    pub cuvid_decoders: Vec<String>,
    pub gpu_label: String,
    /// Every hwaccel ffmpeg lists. Drives the CPU-level decode assist:
    /// `-hwaccel auto` picks the best one per codec and silently falls
    /// back to software decode when none applies.
    pub hwaccels: Vec<String>,
}

impl HwCaps {
    pub fn any_nvenc(&self) -> bool {
        self.hevc_nvenc || self.h264_nvenc
    }

    /// Any hardware decode path listed → the decode assist is worth its flag.
    pub fn any_hw_decode(&self) -> bool {
        !self.hwaccels.is_empty()
    }

    /// Any hardware HEVC encoder across the three vendor families.
    pub fn any_hevc_hw(&self) -> bool {
        self.hevc_nvenc || self.vt_hevc || self.qsv_hevc || self.amf_hevc
    }

    /// Any hardware AV1 encoder that passed its trial.
    pub fn any_av1_hw(&self) -> bool {
        self.av1_nvenc_usable || self.qsv_av1_usable || self.amf_av1_usable
    }

    pub fn summary(&self) -> String {
        let fam = |on: bool| if on { "usable" } else { "no" };
        format!(
            "cuda={} hevc_nvenc={} h264_nvenc={} av1_hw={} \
             (nvenc:{} qsv:{} amf:{}) b_ref_mode={} videotoolbox={}/{} qsv={}/{} \
             amf={}/{} svt_av1={} hwaccels={} gpu={}",
            flag(self.cuda_hwaccel),
            flag(self.hevc_nvenc),
            flag(self.h264_nvenc),
            flag(self.any_av1_hw()),
            fam(self.av1_nvenc_usable),
            fam(self.qsv_av1_usable),
            fam(self.amf_av1_usable),
            flag(self.nvenc_b_ref_mode),
            flag(self.vt_h264),
            flag(self.vt_hevc),
            flag(self.qsv_h264),
            flag(self.qsv_hevc),
            flag(self.amf_h264),
            flag(self.amf_hevc),
            flag(self.svt_av1),
            self.hwaccels.join(","),
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
    let hwaccels_raw = cmd_out("ffmpeg", &["-hide_banner", "-hwaccels"]);
    let hwaccels: Vec<String> = hwaccels_raw
        .lines()
        .skip_while(|l| !l.contains("Hardware acceleration methods"))
        .skip(1)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let cuda_hwaccel = hwaccels.iter().any(|h| h == "cuda");

    let encoders = cmd_out("ffmpeg", &["-hide_banner", "-encoders"]);
    let enc_lower = encoders.to_lowercase();
    let has = |name: &str| enc_lower.contains(name);
    let hevc_nvenc = has("hevc_nvenc");
    let h264_nvenc = has("h264_nvenc");
    let av1_nvenc_listed = has("av1_nvenc");
    let svt_av1 = has("libsvtav1");
    let vt_h264 = has("h264_videotoolbox");
    let vt_hevc = has("hevc_videotoolbox");
    let qsv_h264 = has("h264_qsv");
    let qsv_hevc = has("hevc_qsv");
    let qsv_av1_listed = has("av1_qsv");
    let amf_h264 = has("h264_amf");
    let amf_hevc = has("hevc_amf");
    let amf_av1_listed = has("av1_amf");

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

    // Trial encodes decide usability. `-encoders` listing is not enough:
    // RTX 20/30 list av1_nvenc without an AV1 unit, and QSV/AMF AV1
    // support depends on the GPU generation behind the same encoder name.
    // Each trial uses the exact flags the real encode would carry, so
    // "usable" means "works with our arguments", not just "loads".
    let av1_nvenc_usable = av1_nvenc_listed
        && trial_encoder_works(
            "av1_nvenc",
            &[
                "-preset",
                "p4",
                "-tune",
                "hq",
                "-rc",
                "vbr",
                "-cq",
                "28",
                "-b:v",
                "0",
                "-b_ref_mode",
                "middle",
            ],
        );
    let qsv_av1_usable = qsv_av1_listed
        && trial_encoder_works("av1_qsv", &["-global_quality", "28", "-preset", "medium"]);
    let amf_av1_usable = amf_av1_listed
        && trial_encoder_works(
            "av1_amf",
            &[
                "-quality", "balanced", "-rc", "cqp", "-qp_i", "28", "-qp_p", "28",
            ],
        );
    // B-frames-as-reference: a few % smaller HEVC/AV1 files at the same CQ
    // on Turing+; Pascal rejects the flag outright, so gate it here.
    let nvenc_b_ref_mode = hevc_nvenc
        && trial_encoder_works(
            "hevc_nvenc",
            &[
                "-preset",
                "p1",
                "-tune",
                "hq",
                "-rc",
                "vbr",
                "-cq",
                "28",
                "-b:v",
                "0",
                "-b_ref_mode",
                "middle",
            ],
        );

    let gpu_label = gpu_label();

    HwCaps {
        cuda_hwaccel,
        hevc_nvenc,
        h264_nvenc,
        av1_nvenc_listed,
        av1_nvenc_usable,
        nvenc_b_ref_mode,
        vt_h264,
        vt_hevc,
        qsv_h264,
        qsv_hevc,
        qsv_av1_listed,
        qsv_av1_usable,
        amf_h264,
        amf_hevc,
        amf_av1_listed,
        amf_av1_usable,
        svt_av1,
        cuvid_decoders,
        gpu_label,
        hwaccels,
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

/// Try to initialise `encoder` with `extra` encoder flags on a short
/// synthetic clip. Returns true only if ffmpeg exits 0. 320x240 keeps the
/// clip tiny while staying above NVENC's minimum frame dimensions — the
/// old 128x128 trial failed every encoder for that reason alone, which
/// made the AV1 gate reject capable GPUs (and hid `-b_ref_mode`).
fn trial_encoder_works(encoder: &str, extra: &[&str]) -> bool {
    let mut cmd = crate::process::cmd("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=15:duration=0.4",
        "-c:v",
        encoder,
    ])
    .args(extra)
    .args(["-f", "null", "-"]);
    let mut child = match cmd
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

#[cfg(test)]
mod tests {
    use super::*;

    fn full_caps() -> HwCaps {
        HwCaps {
            cuda_hwaccel: true,
            hevc_nvenc: true,
            h264_nvenc: true,
            av1_nvenc_listed: true,
            av1_nvenc_usable: true,
            nvenc_b_ref_mode: true,
            vt_h264: false,
            vt_hevc: false,
            qsv_h264: true,
            qsv_hevc: true,
            qsv_av1_listed: false,
            qsv_av1_usable: false,
            amf_h264: false,
            amf_hevc: false,
            amf_av1_listed: false,
            amf_av1_usable: false,
            svt_av1: true,
            cuvid_decoders: vec![],
            gpu_label: "test".into(),
            hwaccels: vec!["cuda".into(), "d3d11va".into()],
        }
    }

    #[test]
    fn family_helpers() {
        let c = full_caps();
        assert!(c.any_nvenc());
        assert!(c.any_hw_decode());
        assert!(c.any_hevc_hw());
        assert!(c.any_av1_hw());
        // A Mac-shaped box: no NVENC, VideoToolbox present, no AV1 anywhere.
        let mac = HwCaps {
            cuda_hwaccel: false,
            hevc_nvenc: false,
            h264_nvenc: false,
            av1_nvenc_listed: false,
            av1_nvenc_usable: false,
            nvenc_b_ref_mode: false,
            vt_h264: true,
            vt_hevc: true,
            qsv_h264: false,
            qsv_hevc: false,
            qsv_av1_listed: false,
            qsv_av1_usable: false,
            amf_h264: false,
            amf_hevc: false,
            amf_av1_listed: false,
            amf_av1_usable: false,
            svt_av1: true,
            cuvid_decoders: vec![],
            gpu_label: "none".into(),
            hwaccels: vec!["videotoolbox".into()],
        };
        assert!(mac.any_hevc_hw());
        assert!(!mac.any_av1_hw());
        assert!(mac.any_hw_decode());
        // CPU-only machine: no hwaccels → decode assist disabled.
        let cpu = HwCaps {
            hwaccels: vec![],
            ..mac
        };
        assert!(!cpu.any_hw_decode());
    }

    #[test]
    fn summary_names_the_families() {
        let s = full_caps().summary();
        assert!(s.contains("hevc_nvenc=yes"), "{s}");
        assert!(s.contains("qsv=yes"), "{s}");
        assert!(s.contains("b_ref_mode=yes"), "{s}");
    }
}
