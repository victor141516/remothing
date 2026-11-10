mod capture;
mod cuda;
mod input;
mod net;
mod proto;
mod shared;
mod video;
mod xfb;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

struct Args {
    display: String,
    shmid: i32,
    listen: SocketAddr,
    web: PathBuf,
    tile: u32,
    tuning: net::Tuning,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        display: std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()),
        shmid: -1,
        listen: "0.0.0.0:8080".parse()?,
        web: "/opt/remothing/web".into(),
        tile: 64,
        tuning: net::Tuning { bandwidth_mbps: 200.0, max_lossless_frame_kb: 96 },
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--display" => a.display = val()?,
            "--shmid" => a.shmid = val()?.parse()?,
            "--listen" => a.listen = val()?.parse()?,
            "--web" => a.web = val()?.into(),
            "--tile" => a.tile = val()?.parse()?,
            "--bandwidth-mbps" => a.tuning.bandwidth_mbps = val()?.parse()?,
            "--max-lossless-frame-kb" => a.tuning.max_lossless_frame_kb = val()?.parse()?,
            _ => bail!("unknown flag {flag}"),
        }
    }
    if a.shmid < 0 {
        bail!("--shmid is required (printed by `Xvfb -shmem`)");
    }
    Ok(a)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let fb = xfb::XvfbFramebuffer::attach(args.shmid)?;
    eprintln!("framebuffer {}x{} stride {}", fb.width, fb.height, fb.stride);
    let shared = Arc::new(shared::Shared::new(fb.width, fb.height, args.tile));
    let injector = input::Injector::new(&args.display)?;

    let cap_shared = shared.clone();
    let display = args.display.clone();
    std::thread::Builder::new().name("capture".into()).spawn(move || {
        if let Err(e) = capture::run(&display, fb, cap_shared) {
            eprintln!("capture failed: {e:#}");
            std::process::exit(1);
        }
    })?;

    let app = Arc::new(net::App { shared, injector, tuning: args.tuning });
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(net::serve(args.listen, args.web, app))
}
