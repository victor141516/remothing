//! Capture thread: drives the selected display's `CaptureSession`, feeds the
//! tile state and handles hot display switching.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::geom::Rect;
use crate::platform::{DisplayInfo, InputProvider, OpenedDisplay, Platform};
use crate::proto;
use crate::shared::{Shared, TileDiff};
use crate::telemetry::Telemetry;

const FRAME_WAIT: Duration = Duration::from_millis(100);
const REOPEN_DELAY: Duration = Duration::from_millis(250);

/// Selected display, shared between the capture thread, the input socket and the admin API.
#[derive(Default)]
pub struct DisplayControl {
    /// Display id the capture thread should switch to.
    request: Mutex<Option<String>>,
    current: RwLock<Option<DisplayInfo>>,
    input: RwLock<Option<Arc<dyn InputProvider>>>,
}

impl DisplayControl {
    pub fn select(&self, id: &str) {
        *self.request.lock().unwrap() = Some(id.to_string());
    }

    pub fn current(&self) -> Option<DisplayInfo> {
        self.current.read().unwrap().clone()
    }

    pub fn input(&self) -> Option<Arc<dyn InputProvider>> {
        self.input.read().unwrap().clone()
    }

    fn take_request(&self) -> Option<String> {
        self.request.lock().unwrap().take()
    }

    fn has_request(&self) -> bool {
        self.request.lock().unwrap().is_some()
    }
}

pub struct CaptureLoop {
    pub platform: Arc<dyn Platform>,
    pub shared: Arc<Shared>,
    pub control: Arc<DisplayControl>,
    pub telemetry: Arc<Telemetry>,
    /// Applied to each newly opened display's input provider.
    pub keyboard_layout: Arc<RwLock<String>>,
}

impl CaptureLoop {
    /// Runs forever. `initial` empty = primary display, else first listed.
    pub fn run(self, initial: String) {
        let mut wanted = initial;
        loop {
            if let Some(id) = self.control.take_request() {
                wanted = id;
            }
            let opened = match self.open(&wanted) {
                Ok(o) => o,
                Err(e) => {
                    self.telemetry.error("capture: open display", e);
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };
            wanted = opened.info.id.clone();
            if let Err(e) = self.stream(opened) {
                self.telemetry.error("capture", e);
                std::thread::sleep(REOPEN_DELAY);
            }
        }
    }

    fn open(&self, wanted: &str) -> Result<OpenedDisplay> {
        let displays = self.platform.displays();
        let list = displays.list().context("list displays")?;
        let id = list
            .iter()
            .find(|d| d.id == wanted)
            .or_else(|| list.iter().find(|d| d.primary))
            .or(list.first())
            .map(|d| d.id.clone())
            .context("no display available")?;
        displays.open(&id).with_context(|| format!("open display {id}"))
    }

    /// Streams one display until a switch is requested (Ok) or the session fails (Err).
    fn stream(&self, opened: OpenedDisplay) -> Result<()> {
        let OpenedDisplay { info, mut capture, input } = opened;
        let layout = self.keyboard_layout.read().unwrap().clone();
        if let Err(e) = input.set_keyboard_layout(&layout) {
            eprintln!("keyboard layout {layout}: {e:#}");
        }
        let (width, height) = capture.size();
        eprintln!("streaming display {} ({}x{})", info.id, width, height);
        self.shared.reconfigure(width, height, capture.gpu_surface());
        *self.control.input.write().unwrap() = Some(input);
        *self.control.current.write().unwrap() = Some(info);

        let stride = width as usize * 4;
        let mut diff = TileDiff::new(self.shared.geometry());
        // Only for sessions without directly mapped pixels.
        let mut staging = Vec::new();
        let mut rects: Vec<Rect> = Vec::new();
        while !self.control.has_request() {
            if let Some(state) = capture.cursor_change()? {
                self.shared.set_cursor(proto::cursor(&state));
            }
            let Some(frame) = capture.next_frame(FRAME_WAIT)? else { continue };
            rects.clear();
            rects.extend(frame.dirty.iter().filter_map(|r| r.clip(width, height)));
            rects.extend(frame.moved.iter().filter_map(|m| m.dst.clip(width, height)));
            if rects.is_empty() {
                continue;
            }
            match capture.mapped() {
                Some((pixels, src_stride)) => diff.diff(pixels, src_stride, &rects, &frame.moved),
                None => {
                    staging.resize(stride * height as usize, 0);
                    capture.read(&rects, &mut staging, stride)?;
                    diff.diff(&staging, stride, &rects, &frame.moved)
                }
            };
            self.shared.commit(&diff, frame.capture_us);
        }
        Ok(())
    }
}
