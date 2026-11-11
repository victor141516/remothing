//! Capture thread: drives the selected display's `CaptureSession`, feeds the
//! tile state and handles hot display switching.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::watch;

use crate::geom::{MovedRect, Rect};
use crate::pixels::Differ;
use crate::platform::{CaptureSession, Clipboard, DisplayInfo, InputProvider, OpenedDisplay, Platform, ThreadRole};
use crate::proto;
use crate::shared::Shared;
use crate::telemetry::Telemetry;

/// Also bounds how late skipped CPU copies are refreshed after motion stops.
const FRAME_WAIT: Duration = Duration::from_millis(50);
const REOPEN_DELAY: Duration = Duration::from_millis(250);
/// Longest clipboard text relayed between the session and the clients.
const MAX_CLIPBOARD: usize = 1 << 20;

/// Clipboard text as last seen, and the client that set it (None: the session itself).
pub type ClipboardText = (Option<u64>, Arc<str>);

/// Selected display, shared between the capture thread, the input socket and the admin API.
pub struct DisplayControl {
    /// Display id the capture thread should switch to.
    request: Mutex<Option<String>>,
    current: RwLock<Option<DisplayInfo>>,
    input: RwLock<Option<Arc<dyn InputProvider>>>,
    clipboard: RwLock<Option<Arc<dyn Clipboard>>>,
    clipboard_text: watch::Sender<ClipboardText>,
}

impl Default for DisplayControl {
    fn default() -> Self {
        Self {
            request: Mutex::default(),
            current: RwLock::default(),
            input: RwLock::default(),
            clipboard: RwLock::default(),
            clipboard_text: watch::channel((None, Arc::from(""))).0,
        }
    }
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

    /// The session's clipboard text, followed across changes.
    pub fn clipboard_text(&self) -> watch::Receiver<ClipboardText> {
        self.clipboard_text.subscribe()
    }

    /// Client `client` put `text` on the session's clipboard.
    pub fn set_clipboard(&self, client: u64, text: &str) -> Result<()> {
        if text.len() > MAX_CLIPBOARD {
            bail!("clipboard text of {} bytes is too long", text.len());
        }
        let clipboard = self.clipboard.read().unwrap().clone().context("no clipboard access on this display")?;
        clipboard.set_text(text)?;
        self.clipboard_changed(Some(client), text);
        Ok(())
    }

    /// Publishes the text unless it is what was last seen (the session
    /// reports a client's own change back).
    fn clipboard_changed(&self, client: Option<u64>, text: &str) {
        if text.len() > MAX_CLIPBOARD {
            return;
        }
        self.clipboard_text.send_if_modified(|(by, last)| {
            let changed = **last != *text;
            if changed {
                (*by, *last) = (client, Arc::from(text));
            }
            changed
        });
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
        // Every client waits for this thread.
        self.platform.thread_priority(ThreadRole::Capture);
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
        let OpenedDisplay { info, mut capture, input, clipboard } = opened;
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
        if let Some(c) = &clipboard {
            let control = self.control.clone();
            c.watch(Box::new(move |text| control.clipboard_changed(None, &text)));
        }
        // The previous display's clipboard (and its watcher) ends here.
        *self.control.clipboard.write().unwrap() = clipboard;

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
            self.shared.tick(proto::now_us());
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
