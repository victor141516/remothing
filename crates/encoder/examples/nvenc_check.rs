//! NVENC check on Linux, where the core feeds the encoder from system memory (`load_cpu`)
//! and NVENC reads a CUDA copy: encodes synthetic full-screen motion (rt-testapp's plasma)
//! through the `EncoderProvider`, decodes it with FFmpeg and checks it, as `d3d11_bench`
//! does on Windows. Run it where NVENC is: a container with `--gpus all` and
//! `NVIDIA_DRIVER_CAPABILITIES` including `video`.
//!
//! `cargo run --release -p remothing-encoder --example nvenc_check -- [flags]`
//!
//! - `--codec C`: `h264`, `hevc`, `av1`; repeatable (default all three)
//! - `--size WxH` (default 1920x1080), `--frames N` (default 600)
//! - `--enc sdk|ffmpeg`: NVENC through the SDK (default) or FFmpeg's
//! - `--rate-steps N`: changes the bitrate cap every N frames (5, 12, 30, 80, 20 Mbit/s in
//!   turn) and reports the keyframes after the first
//! - `--lose N`: drops N frames from the decoder's input once a period (60 frames, longer for
//!   long runs), then calls `invalidate_from` (a keyframe where it says no); a second decoder
//!   gets every frame and each picture the client's decoder shows is compared with it.
//!   Frames are acknowledged 8 frames late.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nvenc_check is the Linux check (d3d11_bench is the Windows one)");
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

// The quality scores are `d3d11_bench`'s.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "common/check.rs"]
mod check;

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{bail, Context, Result};
    use remothing_core::encode::{Codec, EncoderProvider, EncoderRequest};
    use remothing_core::geom::Rect;
    use remothing_encoder::FfmpegEncoders;

    use super::check::Check;

    /// Pre-rendered frames, played forth and back.
    const CYCLE: usize = 16;
    /// Caps `--rate-steps` cycles through, Mbit/s at 60 fps.
    const RATE_STEPS: [u64; 5] = [5, 12, 30, 80, 20];
    /// `--lose`: frames are acknowledged this many frames after they are coded.
    const ACK_DELAY: u64 = 8;

    struct Args {
        codecs: Vec<Codec>,
        size: (u32, u32),
        frames: u32,
        native: bool,
        rate_steps: Option<u32>,
        lose: Option<u32>,
    }

    fn args() -> Result<Args> {
        let mut a = Args { codecs: Vec::new(), size: (1920, 1080), frames: 600, native: true, rate_steps: None, lose: None };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--codec" => {
                    let v = value()?;
                    a.codecs.push(Codec::from_name(&v).with_context(|| format!("unknown codec {v}"))?);
                }
                "--size" => {
                    let v = value()?;
                    let (w, h) = v.split_once('x').with_context(|| format!("size {v}: expected WxH"))?;
                    a.size = (w.parse()?, h.parse()?);
                }
                "--frames" => a.frames = value()?.parse()?,
                "--enc" => {
                    a.native = match value()?.as_str() {
                        "sdk" => true,
                        "ffmpeg" => false,
                        e => bail!("unknown encoder {e}"),
                    }
                }
                "--rate-steps" => a.rate_steps = Some(value()?.parse()?),
                "--lose" => a.lose = Some(value()?.parse()?),
                _ => bail!("unknown argument {arg} (see the doc comment of examples/nvenc_check.rs)"),
            }
        }
        if a.codecs.is_empty() {
            a.codecs = Codec::ALL.to_vec();
        }
        Ok(a)
    }

    /// rt-testapp's plasma at time `t`, BGRX.
    fn plasma(w: u32, h: u32, t: usize) -> Vec<u8> {
        let table = |period: f64| -> Vec<u32> {
            (0..4096).map(|i| ((((i as f64) * std::f64::consts::TAU / period).sin() + 1.0) * 63.0) as u32).collect()
        };
        let (sx, sy) = (table(700.0), table(500.0));
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            let (vy, vy2) = (sy[(y + t * 3) & 4095], sx[(y * 2 + t) & 4095]);
            for x in 0..w as usize {
                let a = sx[(x + t * 5) & 4095] + vy;
                let b = sy[(x * 2 + 4096 - ((t * 2) & 4095)) & 4095] + vy2;
                px.extend_from_slice(&[((a + b) >> 1) as u8, b as u8, a as u8, 255]);
            }
        }
        px
    }

    pub fn run() -> Result<()> {
        let a = args()?;
        let (w, h) = a.size;
        let mut provider = FfmpegEncoders::new()?;
        provider.set_native_nvenc(a.native);
        let cycle: Vec<Vec<u8>> = (0..CYCLE).map(|t| plasma(w, h, t + 1)).collect();
        println!("| encoder | size | frames | KiB/frame | keys | decoded | MAE | rate changes | losses | recovered | keys after first | recovery KiB p50 | client pictures | differing | worst MAE | refused |");
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for &codec in &a.codecs {
            let req = EncoderRequest {
                codec,
                width: w,
                height: h,
                output: (w, h),
                preferred: Some("nvenc".into()),
                surface: None,
                max_bitrate: u64::MAX,
                fps: 60,
                two_pass: false,
                bt709: false,
            };
            let mut enc = provider.create(&req)?;
            if a.native && !enc.name().contains("sdk") {
                println!("(SDK path did not open: {:?})", enc.fallback_reason());
            }
            let mut check = Check::new(codec, a.lose.is_some(), false)?;
            let period = a.lose.map(|n| (n as u64 + 40).max(60));
            let (mut bytes, mut keys, mut losses, mut recovered, mut rate_changes) = (0usize, 0u32, 0u32, 0u32, 0u32);
            let mut recovery_bytes = Vec::new();
            let mut reached = Vec::new();
            let mut lost_from = None;
            let mut current = 0;
            for n in 0..a.frames {
                // Forth and back, as d3d11_bench's full-screen scenes.
                let i = n as usize % (2 * CYCLE - 2);
                current = if i < CYCLE { i } else { 2 * CYCLE - 2 - i };
                enc.load_cpu(&cycle[current], w as usize * 4, 0, &[Rect::new(0, 0, w, h)])?;
                if let Some(step) = a.rate_steps.filter(|&s| n > 0 && n % s == 0) {
                    enc.set_max_bitrate(RATE_STEPS[(n / step) as usize % RATE_STEPS.len()] * 1_000_000, 60);
                    rate_changes += 1;
                }
                let i = n as u64;
                let lost = match (a.lose, period) {
                    (Some(len), Some(p)) => {
                        let start = i - i % p + p / 2;
                        i >= p && (start..start + len as u64).contains(&i) && start + len as u64 + 1 < a.frames as u64
                    }
                    _ => false,
                };
                let mut force_key = n == 0;
                if let (false, Some(first)) = (lost, lost_from) {
                    lost_from = None;
                    losses += 1;
                    force_key |= !enc.invalidate_from(first);
                }
                let out = enc.encode(force_key)?;
                bytes += out.data.len();
                keys += out.key as u32;
                if out.recovers.is_some() {
                    recovered += 1;
                    recovery_bytes.push(out.data.len());
                }
                if lost && lost_from.is_none() {
                    lost_from = Some(out.index);
                }
                reached.push(!lost);
                if a.lose.is_some() && out.index >= ACK_DELAY && reached[(out.index - ACK_DELAY) as usize] {
                    enc.acknowledge(out.index - ACK_DELAY);
                }
                check.feed(out.index, &out.data, !lost, None)?;
            }
            let d = check.finish(&cycle[current], w, h)?;
            recovery_bytes.sort_unstable();
            let recovery = recovery_bytes.get(recovery_bytes.len() / 2).map_or(f64::NAN, |&b| b as f64 / 1024.0);
            println!(
                "| {} | {w}x{h} | {} | {:.1} | {keys} | {} | {:.2} | {rate_changes} | {losses} | {recovered} | {} | {recovery:.1} | {} | {} | {:.3} | {} |",
                enc.name(),
                a.frames,
                bytes as f64 / a.frames as f64 / 1024.0,
                d.decoded,
                d.mae,
                keys.saturating_sub(1),
                d.shown,
                d.differing,
                d.worst,
                d.errors,
            );
        }
        Ok(())
    }
}
