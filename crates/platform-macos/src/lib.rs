//! macOS providers: ScreenCaptureKit capture (changed areas from the frame
//! info, IOSurface frames handed to the VideoToolbox encoder without a
//! copy), Quartz event injection, the system cursor, the general pasteboard
//! and ScreenCaptureKit's system sound. Needs the Screen Recording and
//! Accessibility permissions. Not here: virtual displays, a virtual
//! microphone (macOS has none without an audio driver), HDR pictures.
#![cfg(target_os = "macos")]

mod audio;
mod clipboard;
mod cursor;
mod display;
mod input;
mod keymap;
mod monitor;
mod permissions;
mod power;
mod sck;
pub mod service;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use remothing_core::encode::EncoderProvider;
use remothing_core::platform::{
    AudioSink, AudioSource, DisplayInfo, DisplayProvider, MicUnavailable, OpenedDisplay, Platform, SystemMonitor,
    ThreadRole,
};

use crate::display::Geometry;

pub struct MacDisplays;

impl DisplayProvider for MacDisplays {
    fn provider_name(&self) -> String {
        "screencapturekit".into()
    }

    fn list(&self) -> Result<Vec<DisplayInfo>> {
        display::list()
    }

    /// No HDR picture on macOS (docs/ARCHITECTURE.md, "macOS"): ScreenCaptureKit
    /// is asked for 8-bit sRGB.
    fn open(&self, id: &str, _hdr: bool) -> Result<OpenedDisplay> {
        let geometry = Geometry::of(display::parse_id(id)?)?;
        let capture = sck::SckSession::open(geometry)?;
        if !permissions::accessibility(false) {
            eprintln!("input: no Accessibility permission, macOS drops the remote input");
        }
        Ok(OpenedDisplay {
            info: geometry.info(),
            capture: Box::new(capture),
            input: Arc::new(input::QuartzInput::new(geometry)),
            clipboard: Some(Arc::new(clipboard::Pasteboard::new())),
        })
    }
}

pub struct MacPlatform {
    displays: MacDisplays,
    encoders: Box<dyn EncoderProvider>,
    monitor: monitor::MacMonitor,
    awake: Mutex<Option<power::Awake>>,
}

impl MacPlatform {
    pub fn new(encoders: Box<dyn EncoderProvider>) -> Result<Self> {
        permissions::check();
        Ok(Self { displays: MacDisplays, encoders, monitor: monitor::MacMonitor::new(), awake: Mutex::new(None) })
    }
}

impl Platform for MacPlatform {
    fn name(&self) -> &'static str {
        "macos"
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

    /// The scheduler's interactive class for every streaming thread: what
    /// macOS gives the threads that draw what the user looks at.
    fn thread_priority(&self, _role: ThreadRole) {
        // SAFETY: applies to the calling thread only.
        let err = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) };
        if err != 0 {
            eprintln!("thread priority: pthread_set_qos_class_self_np failed ({err})");
        }
    }

    fn streaming(&self, active: bool) -> Vec<String> {
        let mut report = Vec::new();
        let mut awake = self.awake.lock().unwrap();
        match (active, awake.is_some()) {
            (true, false) => *awake = Some(power::Awake::start(&mut report)),
            (false, true) => {
                *awake = None;
                report.push("display sleep and background throttling allowed again".into());
            }
            _ => {}
        }
        report
    }

    fn open_audio(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioSource>> {
        Ok(Box::new(audio::SckAudio::open(tried)?))
    }

    fn open_microphone(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioSink>, MicUnavailable> {
        tried.push("macOS has no virtual microphone without a third-party audio driver; not supported".into());
        Err(MicUnavailable::Failed)
    }
}
