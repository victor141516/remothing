//! Linux providers: simulated displays (Xvfb + shared memory + XDamage) and
//! real X11 displays (RandR outputs, XShm + XDamage); XTest input, XFixes
//! cursor, X selections for the clipboard, sound through whatever sound
//! server there is. The encoder provider is injected by the binary.
//! Wayland sessions are not supported (would need PipeWire/portal or KMS).
#![cfg(target_os = "linux")]

mod audio;
mod clipboard;
mod cursor;
mod input;
mod microphone;
mod monitor;
mod x11;
mod xfb;
mod xvfb;
mod xvfb_session;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use remothing_core::encode::EncoderProvider;
use remothing_core::platform::{
    AudioSink, AudioSource, Clipboard, DisplayInfo, DisplayProvider, MicUnavailable, OpenedDisplay, Platform,
    SystemMonitor, ThreadRole, VirtualDisplays,
};

use crate::audio::SoundServer;
use crate::clipboard::X11Clipboard;
use crate::input::XTestInput;
use crate::monitor::LinuxMonitor;
use crate::xvfb::XvfbManager;
use crate::xvfb_session::XvfbSession;

/// `[platform]` options of the config file.
#[derive(Default)]
pub struct LinuxOptions {
    /// Existing X servers to expose as real displays (default: `$DISPLAY`).
    pub x11_displays: Vec<String>,
    /// Command started inside every new simulated display (`sh -c`).
    pub session_command: Option<String>,
    /// Simulated display created at startup, e.g. (1920, 1080).
    pub virtual_on_start: Option<(u32, u32)>,
}

impl LinuxOptions {
    pub fn from_table(t: &toml::Table) -> Result<Self> {
        let mut o = Self::default();
        match t.get("x11_displays") {
            Some(v) => {
                let list = v.as_array().context("platform.x11_displays must be a list")?;
                for d in list {
                    o.x11_displays.push(d.as_str().context("platform.x11_displays entries are strings")?.into());
                }
            }
            None => o.x11_displays.extend(std::env::var("DISPLAY").ok().filter(|d| !d.is_empty())),
        }
        o.session_command = t.get("session_command").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(Into::into);
        if let Some(size) = t.get("virtual_on_start").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            let (w, h) = size.split_once('x').context("platform.virtual_on_start must be WIDTHxHEIGHT")?;
            o.virtual_on_start = Some((w.parse()?, h.parse()?));
        }
        Ok(o)
    }
}

pub struct LinuxDisplays {
    xvfb: XvfbManager,
    x11_displays: Vec<String>,
}

impl DisplayProvider for LinuxDisplays {
    fn provider_name(&self) -> String {
        "xvfb (shm + xdamage) / x11 (xshm + xdamage)".into()
    }

    fn list(&self) -> Result<Vec<DisplayInfo>> {
        let mut out: Vec<DisplayInfo> = Vec::new();
        for d in &self.x11_displays {
            match x11::list(d) {
                Ok(list) => out.extend(list),
                Err(e) => eprintln!("x11 display {d}: {e:#}"),
            }
        }
        out.extend(self.xvfb.list().iter().map(|i| i.info()));
        Ok(out)
    }

    /// X11 has no HDR (docs/ARCHITECTURE.md, "HDR"): every picture is SDR.
    fn open(&self, id: &str, _hdr: bool) -> Result<OpenedDisplay> {
        // Streaming works without it (e.g. an X server without XFixes).
        let clipboard = |display: &str| {
            X11Clipboard::new(display)
                .inspect_err(|e| eprintln!("clipboard unavailable on {display}: {e:#}"))
                .ok()
                .map(|c| Arc::new(c) as Arc<dyn Clipboard>)
        };
        if let Some(instance) = self.xvfb.get(id) {
            let display = instance.display();
            let capture = XvfbSession::open(&display, instance.shmid, instance.clone())?;
            let info = instance.info();
            let input = Arc::new(XTestInput::new(&display, (0, 0), (info.width, info.height))?);
            let clipboard = clipboard(&display);
            return Ok(OpenedDisplay { info, capture: Box::new(capture), input, clipboard });
        }
        if let Some(rest) = id.strip_prefix("x11:") {
            // id = "x11:<display>:<output>"; the display itself may contain ':'.
            let display = self.x11_displays.iter().find(|d| rest.starts_with(&format!("{d}:"))).context("unknown X display")?;
            let info = x11::list(display)?.into_iter().find(|i| i.id == id).context("output no longer active")?;
            let capture = x11::X11Session::open(display, &info)?;
            let input = Arc::new(XTestInput::new(display, (info.x, info.y), (info.width, info.height))?);
            return Ok(OpenedDisplay { info, capture: Box::new(capture), input, clipboard: clipboard(display) });
        }
        bail!("unknown display {id}")
    }

    fn virtual_displays(&self) -> Option<&dyn VirtualDisplays> {
        Some(&self.xvfb)
    }
}

pub struct LinuxPlatform {
    displays: LinuxDisplays,
    encoders: Box<dyn EncoderProvider>,
    monitor: LinuxMonitor,
    sound: SoundServer,
}

impl LinuxPlatform {
    pub fn new(options: LinuxOptions, encoders: Box<dyn EncoderProvider>) -> Result<Self> {
        // Before the sessions: their programs are told where to play.
        let sound = SoundServer::start();
        let xvfb = XvfbManager::new(options.session_command, sound.address().map(Into::into));
        let p = Self {
            displays: LinuxDisplays { xvfb, x11_displays: options.x11_displays },
            encoders,
            monitor: LinuxMonitor::new(),
            sound,
        };
        if let Some((w, h)) = options.virtual_on_start {
            p.displays.xvfb.create_main(w, h)?;
        }
        Ok(p)
    }
}

impl Platform for LinuxPlatform {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn displays(&self) -> &dyn DisplayProvider {
        &self.displays
    }

    fn encoders(&self) -> &dyn EncoderProvider {
        self.encoders.as_ref()
    }

    fn monitor(&self) -> &dyn SystemMonitor {
        &self.monitor
    }

    /// A lower nice value for the thread (ARCHITECTURE, "Scheduling"). Needs
    /// CAP_SYS_NICE (root on a host; `--cap-add SYS_NICE` in a container);
    /// without it the threads keep the default, said once in the log.
    fn thread_priority(&self, role: ThreadRole) {
        static REFUSED: AtomicBool = AtomicBool::new(false);
        let nice = match role {
            ThreadRole::Capture => -10,
            ThreadRole::Stream => -5,
        };
        // SAFETY: plain system calls on the calling thread.
        let ok = unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, nice) } == 0;
        if !ok && !REFUSED.swap(true, Ordering::Relaxed) {
            eprintln!("thread priority: {} (needs CAP_SYS_NICE); threads keep the default", std::io::Error::last_os_error());
        }
    }

    fn open_audio(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioSource>> {
        self.sound.open(tried)
    }

    fn open_microphone(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioSink>, MicUnavailable> {
        microphone::open(&self.sound, tried)
    }
}
