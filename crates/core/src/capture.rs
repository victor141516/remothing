//! Capture thread: drives the selected display's `CaptureSession`, feeds the
//! tile state and handles hot display switching.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::geom::{MovedRect, Rect};
use crate::pixels::Differ;
use crate::platform::{CaptureSession, DisplayInfo, InputProvider, OpenedDisplay, Platform};
use crate::proto;
use crate::shared::Shared;
use crate::telemetry::Telemetry;

/// Also bounds how late skipped CPU copies are refreshed after motion stops.
const FRAME_WAIT: Duration = Duration::from_millis(50);
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
        // GPU-based sessions can skip CPU readback of tiles shown only from video.
        let gpu = capture.gpu_surface().is_some();
        let pixels = self.shared.reconfigure(width, height, capture.gpu_surface());
        *self.control.input.write().unwrap() = Some(input);
        *self.control.current.write().unwrap() = Some(info);

        let stride = width as usize * 4;
        let mut diff = Differ::new(pixels.geometry);
        // Only for sessions without directly mapped pixels.
        let mut staging = Vec::new();
        let mut rects: Vec<Rect> = Vec::new();
        let mut read_and_diff = |capture: &mut Box<dyn CaptureSession>,
                                 diff: &mut Differ,
                                 tiles: &[Rect],
                                 moved: &[MovedRect],
                                 find_scroll: bool|
         -> Result<()> {
            match capture.mapped() {
                Some((src, src_stride)) => diff.diff(&pixels, src, src_stride, tiles, moved, find_scroll),
                None => {
                    staging.resize(stride * height as usize, 0);
                    capture.read(tiles, &mut staging, stride)?;
                    diff.diff(&pixels, &staging, stride, tiles, moved, find_scroll);
                }
            }
            Ok(())
        };
        while !self.control.has_request() {
            if let Some(state) = capture.cursor_change()? {
                self.shared.set_cursor(proto::cursor(&state));
            }
            let frame = capture.next_frame(FRAME_WAIT)?;
            let hints = gpu.then(|| self.shared.capture_hints(proto::now_us()));
            if let Some(h) = &hints {
                // CPU copies skipped while a tile was video-only catch up once it
                // settles or is shown losslessly again, before its lossless refine.
                let stale = diff.stale_tiles(|i| !h.skip[i] || h.settled[i]);
                if !stale.is_empty() {
                    diff.changed.clear();
                    read_and_diff(&mut capture, &mut diff, &stale, &[], false)?;
                    self.shared.commit(&diff, proto::now_us(), true);
                }
            }
            let Some(frame) = frame else { continue };
            rects.clear();
            rects.extend(frame.dirty.iter().filter_map(|r| r.clip(width, height)));
            rects.extend(frame.moved.iter().filter_map(|m| m.dst.clip(width, height)));
            if rects.is_empty() {
                continue;
            }
            // `mark_rects` lists the skipped tiles as changed; `diff` appends the read ones.
            let tiles = diff.mark_rects(&rects, hints.as_ref().map_or(&[][..], |h| &h.skip));
            read_and_diff(&mut capture, &mut diff, &tiles, &frame.moved, true)?;
            self.shared.commit(&diff, frame.capture_us, false);
        }
        Ok(())
    }
}
