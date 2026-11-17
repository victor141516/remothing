//! macOS providers: ScreenCaptureKit capture (changed areas from the frame
//! info, IOSurface frames handed to the VideoToolbox encoder without a
//! copy), Quartz event injection, the system cursor, the general pasteboard,
//! ScreenCaptureKit's system sound and CGVirtualDisplay virtual displays.
//! Needs the Screen Recording and Accessibility permissions. Not here: a
//! virtual microphone (macOS has none without an audio driver), HDR pictures.
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
mod virtual_display;

use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult};
use remothing_core::encode::EncoderProvider;
use remothing_core::platform::{
    AudioSink, AudioSource, DisplayInfo, DisplayProvider, MicUnavailable, OpenedDisplay, Platform, SystemMonitor,
    ThreadRole, VirtualDisplays,
};

use crate::display::Geometry;

/// Runs `work` on a thread of its own while this thread, the main one, runs
/// the main run loop until `work` returns. CoreGraphics learns of display
/// changes through the main run loop: without it a display added after the
/// start (a virtual one, a monitor plugged in) has no mode in this process
/// and a mode change is never seen (measured on the CI runner).
pub fn with_main_run_loop<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("server".into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .expect("spawning the server thread");
    // SAFETY: reading an immutable CoreFoundation constant.
    let mode = unsafe { kCFRunLoopDefaultMode };
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(mpsc::TryRecvError::Disconnected) => panic!("the server thread ended without a result"),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        // With nothing to run the loop returns at once; nothing to miss then.
        if CFRunLoop::run_in_mode(mode, 0.25, false) == CFRunLoopRunResult::Finished {
            thread::sleep(Duration::from_millis(250));
        }
    }
}

pub struct MacDisplays {
    virtuals: virtual_display::MacVirtualDisplays,
}

impl DisplayProvider for MacDisplays {
    fn provider_name(&self) -> String {
        "screencapturekit".into()
    }

    fn list(&self) -> Result<Vec<DisplayInfo>> {
        let mut list = display::list()?;
        list.iter_mut().for_each(|info| self.virtuals.annotate(info));
        Ok(list)
    }

    fn virtual_displays(&self) -> Option<&dyn VirtualDisplays> {
        Some(&self.virtuals)
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
            info: {
                let mut info = geometry.info();
                self.virtuals.annotate(&mut info);
                info
            },
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
        let displays = MacDisplays { virtuals: virtual_display::MacVirtualDisplays::new() };
        Ok(Self { displays, encoders, monitor: monitor::MacMonitor::new(), awake: Mutex::new(None) })
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
