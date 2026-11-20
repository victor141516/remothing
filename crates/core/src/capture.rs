//! Capture threads: one per display some client watches. Each drives its
//! display's `CaptureSession`, feeds that display's tile state (`Shared`) and
//! stops a while after its last client left.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::sync::watch;

use crate::geom::{MovedRect, Rect};
use crate::pixels::Differ;
use crate::platform::{CaptureSession, Clipboard, Copied, DisplayInfo, InputProvider, OpenedDisplay, Platform, ThreadRole};
use crate::proto;
use crate::shared::Shared;
use crate::telemetry::Telemetry;

/// Also bounds how late skipped CPU copies are refreshed after motion stops.
const FRAME_WAIT: Duration = Duration::from_millis(50);
const REOPEN_DELAY: Duration = Duration::from_millis(250);
/// Longest clipboard text relayed between the session and the clients.
const MAX_CLIPBOARD: usize = 1 << 20;
/// A display stays captured this long after its last client left: a reload
/// or a quick switch back finds it running.
const IDLE_STOP: Duration = Duration::from_secs(10);
/// A display missing from the list this long is gone and its clients move
/// to the default one. Not at once: a virtual display changing mode on
/// Windows is removed and added again.
const GONE_AFTER: Duration = Duration::from_secs(10);

/// Clipboard text as last seen.
#[derive(Clone)]
pub struct ClipboardText {
    /// The client that set it (None: the session itself).
    pub by: Option<u64>,
    pub text: Arc<str>,
    /// Copied while the display was open, not what was there when it opened:
    /// clients copy only such changes to their own computer's clipboard.
    pub changed: bool,
}

impl Default for ClipboardText {
    fn default() -> Self {
        Self { by: None, text: Arc::from(""), changed: false }
    }
}

/// A captured display's state, shared between its capture thread, the
/// clients watching it and the admin API.
pub struct DisplayControl {
    /// Display id the capture thread should switch to.
    request: Mutex<Option<String>>,
    current: RwLock<Option<DisplayInfo>>,
    input: RwLock<Option<Arc<dyn InputProvider>>>,
    clipboard: RwLock<Option<Arc<dyn Clipboard>>>,
    clipboard_text: watch::Sender<ClipboardText>,
    clipboard_files: watch::Sender<Arc<[PathBuf]>>,
}

impl Default for DisplayControl {
    fn default() -> Self {
        Self {
            request: Mutex::default(),
            current: RwLock::default(),
            input: RwLock::default(),
            clipboard: RwLock::default(),
            clipboard_text: watch::channel(ClipboardText::default()).0,
            clipboard_files: watch::channel(Arc::from(Vec::new())).0,
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
        // Text replaced whatever files were copied.
        self.clear_files();
        self.clipboard_changed(Some(client), text, true);
        Ok(())
    }

    fn clear_files(&self) {
        self.clipboard_files.send_if_modified(|files| {
            let modified = !files.is_empty();
            *files = Arc::from(Vec::new());
            modified
        });
    }

    /// Files on the session's clipboard (empty: none), followed across changes.
    pub fn clipboard_files(&self) -> watch::Receiver<Arc<[PathBuf]>> {
        self.clipboard_files.subscribe()
    }

    /// What the platform reports as copied.
    fn copied(&self, copied: Copied, changed: bool) {
        match copied {
            Copied::Text(text) => {
                self.clear_files();
                self.clipboard_changed(None, &text, changed);
            }
            Copied::Files(list) => {
                self.clipboard_files.send_if_modified(|files| {
                    let modified = **files != *list;
                    if modified {
                        *files = list.into();
                    }
                    modified
                });
            }
        }
    }

    /// Publishes the text unless it is what was last seen (the session
    /// reports a client's own change back). `changed`: not the text found
    /// when the display was opened.
    fn clipboard_changed(&self, client: Option<u64>, text: &str, changed: bool) {
        if text.len() > MAX_CLIPBOARD {
            return;
        }
        self.clipboard_text.send_if_modified(|last| {
            let modified = *last.text != *text;
            if modified {
                *last = ClipboardText { by: client, text: Arc::from(text), changed };
            }
            modified
        });
    }

    fn take_request(&self) -> Option<String> {
        self.request.lock().unwrap().take()
    }

    fn has_request(&self) -> bool {
        self.request.lock().unwrap().is_some()
    }
}

/// One display captured for the clients watching it.
pub struct DisplayStream {
    /// The display's id (what clients asked for).
    pub id: String,
    pub shared: Arc<Shared>,
    pub control: Arc<DisplayControl>,
    /// Clients watching; changed under `Streams::map`'s lock.
    watchers: AtomicUsize,
    /// The display went away: its clients move to the default display.
    gone: AtomicBool,
    /// Captures that brought changes, for `capture_fps`.
    captures: Mutex<Rate>,
}

/// Events a second, over windows of about a second.
#[derive(Default)]
struct Rate {
    start_us: u64,
    count: u64,
    per_sec: f64,
}

impl Rate {
    fn note(&mut self, now: u64) {
        self.roll(now);
        self.count += 1;
    }

    fn roll(&mut self, now: u64) {
        let elapsed = now.saturating_sub(self.start_us);
        if elapsed >= 1_000_000 {
            // The first window starts at the first event.
            self.per_sec = if self.start_us == 0 { 0.0 } else { self.count as f64 * 1e6 / elapsed as f64 };
            self.start_us = now;
            self.count = 0;
        }
    }
}

impl DisplayStream {
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    pub fn watchers(&self) -> usize {
        self.watchers.load(Ordering::Acquire)
    }

    /// Captures with changes a second, over the last second or so (admin status).
    pub fn capture_fps(&self) -> f64 {
        let mut c = self.captures.lock().unwrap();
        c.roll(proto::now_us());
        c.per_sec
    }

    fn set_gone(&self) {
        self.gone.store(true, Ordering::Release);
        self.shared.wake_clients();
    }
}

/// The displays being captured, by id.
pub struct Streams {
    platform: Arc<dyn Platform>,
    telemetry: Arc<Telemetry>,
    /// Applied to each newly opened display's input provider.
    keyboard_layout: Arc<RwLock<String>>,
    tile: u32,
    map: Mutex<HashMap<String, Arc<DisplayStream>>>,
    /// See `list_changes`.
    list: watch::Sender<()>,
    /// See `shown`.
    shown: RwLock<Arc<Vec<DisplayInfo>>>,
    /// Clients with the mouse captured (see `set_capturing`).
    capturing: Mutex<HashSet<u64>>,
}

impl Streams {
    pub fn new(platform: Arc<dyn Platform>, telemetry: Arc<Telemetry>, keyboard_layout: Arc<RwLock<String>>, tile: u32) -> Arc<Self> {
        Arc::new(Self {
            platform,
            telemetry,
            keyboard_layout,
            tile,
            map: Mutex::default(),
            list: watch::channel(()).0,
            shown: RwLock::default(),
            capturing: Mutex::default(),
        })
    }

    /// One more client watches display `id`; its capture starts with the
    /// first. Each call is matched by an `unwatch`.
    pub fn watch(self: &Arc<Self>, id: &str) -> Arc<DisplayStream> {
        let mut map = self.map.lock().unwrap();
        if let Some(s) = map.get(id).filter(|s| !s.gone()) {
            s.watchers.fetch_add(1, Ordering::AcqRel);
            let s = s.clone();
            self.publish_shown(&map);
            return s;
        }
        let stream = Arc::new(DisplayStream {
            id: id.to_string(),
            shared: Arc::new(Shared::new(self.tile)),
            control: Arc::new(DisplayControl::default()),
            watchers: AtomicUsize::new(1),
            gone: AtomicBool::new(false),
            captures: Mutex::default(),
        });
        stream.shared.set_follow(!self.capturing.lock().unwrap().is_empty());
        map.insert(id.to_string(), stream.clone());
        let capture = CaptureLoop { streams: self.clone(), stream: stream.clone() };
        if let Err(e) = std::thread::Builder::new().name(format!("capture {id}")).spawn(move || capture.run()) {
            self.telemetry.error("capture thread", anyhow::Error::new(e));
        }
        stream
    }

    pub fn unwatch(&self, stream: &DisplayStream) {
        let map = self.map.lock().unwrap();
        stream.watchers.fetch_sub(1, Ordering::AcqRel);
        self.publish_shown(&map);
    }

    /// Client `id` captured the mouse or let it go: while any client has it
    /// captured, every other client draws the cursor (`Shared::set_follow`).
    pub fn set_capturing(&self, id: u64, on: bool) {
        let any = {
            let mut capturing = self.capturing.lock().unwrap();
            if on {
                capturing.insert(id);
            } else {
                capturing.remove(&id);
            }
            !capturing.is_empty()
        };
        for s in self.active() {
            s.shared.set_follow(any);
        }
    }

    /// The displays being captured.
    pub fn active(&self) -> Vec<Arc<DisplayStream>> {
        let mut list: Vec<_> = self.map.lock().unwrap().values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// Display `id` changed (its mode): opened again if it is captured.
    pub fn reopen(&self, id: &str) {
        if let Some(s) = self.map.lock().unwrap().get(id) {
            s.control.select(id);
        }
    }

    /// Display `id` was removed: its clients move on now.
    pub fn gone(&self, id: &str) {
        let mut map = self.map.lock().unwrap();
        if let Some(s) = map.remove(id) {
            s.set_gone();
        }
        self.publish_shown(&map);
        drop(map);
        self.list_changed();
    }

    /// The displays some client shows: the pointer may move on to them
    /// (`InputProvider::motion_relative`).
    pub fn shown(&self) -> Arc<Vec<DisplayInfo>> {
        self.shown.read().unwrap().clone()
    }

    /// A display was opened (again): its place on the desktop may be new.
    fn refresh_shown(&self) {
        self.publish_shown(&self.map.lock().unwrap());
    }

    /// Kept ready because it is read on every relative mouse move.
    fn publish_shown(&self, map: &HashMap<String, Arc<DisplayStream>>) {
        let shown = map.values().filter(|s| s.watchers() > 0).filter_map(|s| s.control.current()).collect();
        *self.shown.write().unwrap() = Arc::new(shown);
    }

    /// Fires when displays were added, changed or removed, or the default
    /// changed: every client gets the list again.
    pub fn list_changes(&self) -> watch::Receiver<()> {
        self.list.subscribe()
    }

    pub fn list_changed(&self) {
        self.list.send_replace(());
    }

    /// Applies a keyboard layout to every captured display; the failures.
    pub fn set_keyboard_layout(&self, layout: &str) -> Vec<String> {
        let mut errors = Vec::new();
        for s in self.active() {
            if let Some(input) = s.control.input() {
                if let Err(e) = input.set_keyboard_layout(layout) {
                    errors.push(format!("{}: {e:#}", s.id));
                }
            }
        }
        errors
    }

    /// Ends `stream`'s capture if nobody watches it (checked under the lock
    /// `watch` takes, so no client joins a stream that is ending).
    fn finish(&self, stream: &Arc<DisplayStream>) -> bool {
        let mut map = self.map.lock().unwrap();
        if stream.watchers() > 0 {
            return false;
        }
        if map.get(&stream.id).is_some_and(|s| Arc::ptr_eq(s, stream)) {
            map.remove(&stream.id);
        }
        true
    }
}

struct CaptureLoop {
    streams: Arc<Streams>,
    stream: Arc<DisplayStream>,
}

impl CaptureLoop {
    /// Runs until nobody watched the display for `IDLE_STOP`, or it is gone.
    fn run(self) {
        // Every client of this display waits for this thread.
        self.streams.platform.thread_priority(ThreadRole::Capture);
        let id = self.stream.id.clone();
        // The picture every client can show (see `Shared::hdr_wanted`).
        let mut hdr = false;
        let mut idle_since = None;
        let mut missing_since: Option<Instant> = None;
        loop {
            if self.stream.gone() {
                return;
            }
            if self.idle(&mut idle_since) && self.streams.finish(&self.stream) {
                eprintln!("display {id}: no clients, capture stopped");
                return;
            }
            self.stream.control.take_request();
            hdr = self.stream.shared.hdr_wanted().unwrap_or(hdr);
            match self.open(&id, hdr) {
                Ok(Some(opened)) => {
                    missing_since = None;
                    if let Err(e) = self.stream_display(opened, hdr, &mut idle_since) {
                        self.streams.telemetry.error("capture", e);
                        std::thread::sleep(REOPEN_DELAY);
                    }
                }
                Ok(None) => {
                    if missing_since.get_or_insert_with(Instant::now).elapsed() >= GONE_AFTER {
                        eprintln!("display {id} is gone; its clients move to the default display");
                        self.streams.gone(&id);
                        return;
                    }
                    std::thread::sleep(REOPEN_DELAY);
                }
                Err(e) => {
                    self.streams.telemetry.error("capture: open display", e);
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    /// Nobody has watched the display for `IDLE_STOP`.
    fn idle(&self, since: &mut Option<Instant>) -> bool {
        if self.stream.watchers() > 0 {
            *since = None;
            return false;
        }
        since.get_or_insert_with(Instant::now).elapsed() >= IDLE_STOP
    }

    /// None while the display is not listed.
    fn open(&self, id: &str, hdr: bool) -> Result<Option<OpenedDisplay>> {
        let displays = self.streams.platform.displays();
        if !displays.list().context("list displays")?.iter().any(|d| d.id == id) {
            return Ok(None);
        }
        displays.open(id, hdr).with_context(|| format!("open display {id}")).map(Some)
    }

    /// Streams the opened display until it is to be opened again, the
    /// clients now want the other picture of an HDR display or nobody watched
    /// it for a while (Ok), or the session fails (Err).
    fn stream_display(&self, opened: OpenedDisplay, hdr: bool, idle_since: &mut Option<Instant>) -> Result<()> {
        let OpenedDisplay { info, mut capture, input, clipboard } = opened;
        let layout = self.streams.keyboard_layout.read().unwrap().clone();
        if let Err(e) = input.set_keyboard_layout(&layout) {
            eprintln!("keyboard layout {layout}: {e:#}");
        }
        let (width, height) = capture.size();
        let picture = capture.picture();
        let kind = match (picture.hdr_max > 0.0, picture.hdr_display) {
            (true, _) => format!(", HDR picture up to {:.2}x SDR white", picture.hdr_max),
            (false, true) => ", HDR display converted to SDR".to_string(),
            (false, false) => String::new(),
        };
        eprintln!("streaming display {} ({}x{}{kind})", info.id, width, height);
        // GPU-based sessions can skip CPU readback of tiles shown only from video.
        let gpu = capture.gpu_surface().is_some();
        *self.stream.control.input.write().unwrap() = Some(input);
        *self.stream.control.current.write().unwrap() = Some(info);
        self.streams.refresh_shown();
        if let Some(c) = &clipboard {
            let control = self.stream.control.clone();
            c.watch(Box::new(move |copied, changed| control.copied(copied, changed)));
        }
        // The previous display's clipboard (and its watcher) ends here.
        *self.stream.control.clipboard.write().unwrap() = clipboard;
        // Last: it sends the clients their HELLO, after which they may type.
        let pixels = self.stream.shared.reconfigure(width, height, capture.gpu_surface(), picture.hdr_max);

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
        while !self.stream.control.has_request() && !self.stream.gone() && !self.idle(idle_since) {
            // Another picture: the clients' wish changed for an HDR display,
            // or the session found the display's own picture changed.
            let wish = self.stream.shared.hdr_wanted().unwrap_or(hdr);
            if (wish != hdr && picture.hdr_display) || capture.picture() != picture {
                return Ok(());
            }
            self.stream.shared.tick(proto::now_us());
            if let Some(state) = capture.cursor_change()? {
                self.stream.shared.set_cursor(proto::cursor(&state));
            }
            // Asked only while a client captures the mouse (it costs a query on X11).
            if self.stream.shared.wants_cursor_pos() {
                if let Some(pos) = capture.cursor_position()? {
                    self.stream.shared.set_cursor_pos(pos);
                }
            }
            let frame = capture.next_frame(FRAME_WAIT)?;
            let hints = gpu.then(|| self.stream.shared.capture_hints(proto::now_us()));
            if let Some(h) = &hints {
                // CPU copies skipped while a tile was video-only catch up once it
                // settles or is shown losslessly again, before its lossless refine.
                let stale = diff.stale_tiles(|i| !h.skip[i] || h.settled[i]);
                if !stale.is_empty() {
                    diff.changed.clear();
                    read_and_diff(&mut capture, &mut diff, &stale, &[], false)?;
                    self.stream.shared.commit(&diff, proto::now_us(), true);
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
            self.stream.shared.commit(&diff, frame.capture_us, false);
            self.stream.captures.lock().unwrap().note(proto::now_us());
        }
        Ok(())
    }
}
