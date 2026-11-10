//! Latency probe app. Draws a scenario plus a 64-bit barcode at its top-left
//! corner: 64 blocks of 16x16 px, block i = bit i (white = 1).
//!   bits 0..8   magic 0xA5
//!   bits 8..48  wall-clock microseconds since the UNIX epoch (low 40 bits),
//!               taken right before the frame is presented
//!   bits 48..64 x coordinate of the last pointer motion, relative to the
//!               window (= display) origin
//! The client decodes it to measure app-draw -> client-display latency and
//! input -> app -> client round trips. Pointer motion is redrawn immediately.
//!
//! Usage: rt-testapp [--mode idle|box|full|scroll] [--fps N]
//! Windows only: [--monitor N | --display \\.\DISPLAYn] [--present dxgi|gdi]

mod scenario;
#[cfg(windows)]
mod win32;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("rt-testapp supports Linux (X11) and Windows only");

use anyhow::{bail, Context, Result};
use scenario::Mode;

pub struct Args {
    pub mode: Mode,
    pub fps: f64,
    #[cfg(windows)]
    pub monitor: win32::MonitorSel,
    #[cfg(windows)]
    pub present: win32::PresentKind,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        mode: Mode::Idle,
        fps: 165.0,
        #[cfg(windows)]
        monitor: win32::MonitorSel::Primary,
        #[cfg(windows)]
        present: win32::PresentKind::Dxgi,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let val = it.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--mode" => {
                a.mode = match val.as_str() {
                    "idle" => Mode::Idle,
                    "box" => Mode::Boxed,
                    "full" => Mode::Full,
                    "scroll" => Mode::Scroll,
                    m => bail!("unknown mode {m}"),
                }
            }
            "--fps" => a.fps = val.parse()?,
            #[cfg(windows)]
            "--monitor" => a.monitor = win32::MonitorSel::Index(val.parse()?),
            #[cfg(windows)]
            "--display" => a.monitor = win32::MonitorSel::Device(val),
            #[cfg(windows)]
            "--present" => {
                a.present = match val.as_str() {
                    "dxgi" => win32::PresentKind::Dxgi,
                    "gdi" => win32::PresentKind::Gdi,
                    p => bail!("unknown present method {p}"),
                }
            }
            _ => bail!("unknown flag {flag}"),
        }
    }
    Ok(a)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    #[cfg(windows)]
    return win32::run(args);
    #[cfg(target_os = "linux")]
    return x11::run(args);
}
