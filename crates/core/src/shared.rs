//! Per-tile motion history and per-client state over the shared screen copy
//! ([`Pixels`]).
//!
//! The capture thread updates pixels in place and commits tile metadata here.
//! Each client pulls the union of tiles changed since its last frame, so a slow
//! client skips intermediate frames instead of queueing them (bounded latency,
//! always converges to the exact current screen).
//!
//! Every tile is shown by the client either from the lossless layer (exact
//! pixels) or from the video layer (H.264 of the whole screen). Tiles that keep
//! changing move to video when keeping them lossless costs too much; once they
//! stop changing they are re-sent losslessly, so anything that stands still
//! ends up pixel-exact.

use std::sync::{Arc, Mutex};
use std::thread::Thread;

use anyhow::Result;

use crate::encode::VideoEncoder;
use crate::geom::{MovedRect, Rect};
pub use crate::pixels::Geometry;
use crate::pixels::{Differ, Pixels};
use crate::platform::GpuSurface;
use crate::proto::{self, ENC_LZ4, ENC_RAW, ENC_ZSTD, FLAG_KEY, FLAG_MOTION, FLAG_VIDEO, FRAME_HEADER_LEN};

/// A tile is "moving" when its 3rd most recent change is this recent (>= ~20 Hz).
const HOT_WINDOW_US: u64 = 150_000;
/// A video tile unchanged for this long is refined back to lossless.
const COOL_US: u64 = 250_000;
/// Moves queued for one client before falling back to resending destinations.
const MAX_PENDING_COPIES: usize = 64;
/// Link time per frame that lossless sending is sized against (one 60 Hz frame).
const FRAME_SLICE_US: f64 = 16_000.0;
/// Fresh lossless changes costing more than this many slices go to video
/// first and are refined afterwards (a full repaint over a slow link would
/// otherwise block everything behind it).
const BURST_SLICES: f64 = 4.0;
/// Share of the link that refinement (video tiles back to lossless) may use,
/// leaving room so fresh changes find no queue.
const REFINE_SHARE: f64 = 0.8;
/// Below this link rate lossless rects use zstd: 1.7x smaller than LZ4 on a
/// 4K desktop capture, at a CPU cost that only pays off when the link is the limit.
const ZSTD_BELOW: f64 = 100e6 / 8.0;
const ZSTD_LEVEL: i32 = 1;
/// Share of the link the video layer may use (lossless refinement and
/// protocol overhead need the rest). Its bitrate cap and frame pacing follow it.
pub const VIDEO_SHARE: f64 = 0.7;
/// Motion mode is for a screen that moves as a whole (a game, a full-screen
/// video): entered after this many seconds in a row with at least
/// MOTION_SHARE of the screen moving, left after this many in a row with
/// less. Quick to enter and slow to leave, so that opening and closing
/// videos or games between spells of desktop work does not flip it back
/// and forth.
const MOTION_ENTER_S: u32 = 10;
const MOTION_LEAVE_S: u32 = 40;
const MOTION_SHARE: f64 = 0.5;
/// In motion mode the exact picture follows once the whole screen has been
/// still this long.
const STILL_US: u64 = 5_000_000;

/// Decides between tile mode and motion mode from samples of "at least
/// MOTION_SHARE of the screen is moving", judged a second at a time.
#[derive(Default)]
struct MotionDetector {
    on: bool,
    /// Start of the second being judged, and its samples.
    second_from: u64,
    samples: u32,
    moving: u32,
    /// Seconds in a row that spoke for the other mode.
    run: u32,
}

impl MotionDetector {
    /// Returns the new mode when the second that ends with this sample changes it.
    fn sample(&mut self, now: u64, moving: bool) -> Option<bool> {
        let mut switched = None;
        if self.samples > 0 && now.saturating_sub(self.second_from) >= 1_000_000 {
            // A second is what most of its samples were.
            let moving_second = self.moving * 2 >= self.samples;
            (self.samples, self.moving) = (0, 0);
            self.run = if moving_second == self.on { 0 } else { self.run + 1 };
            if self.run >= if self.on { MOTION_LEAVE_S } else { MOTION_ENTER_S } {
                self.run = 0;
                self.on = !self.on;
                switched = Some(self.on);
            }
        }
        if self.samples == 0 {
            self.second_from = now;
        }
        self.samples += 1;
        self.moving += moving as u32;
        switched
    }
}

pub struct Shared {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Bumped whenever the streamed display (and so the geometry) changes.
    epoch: u64,
    tile: u32,
    pixels: Arc<Pixels>,
    /// Capture GPU surface for zero-copy encoders.
    surface: Option<GpuSurface>,
    /// The picture's `Picture::hdr_max` (0: SDR).
    hdr_max: f32,
    /// Last three change times per tile, most recent first (0 = never).
    changes: Vec<[u64; 3]>,
    /// Last time anything on the screen changed.
    changed_us: u64,
    motion: MotionDetector,
    cursor: Option<Arc<Vec<u8>>>,
    /// Where the cursor's hot spot was last seen (only kept up to date while
    /// a client captures the mouse).
    cursor_pos: Option<(u32, u32)>,
    /// A client, here or on another display, has the mouse captured.
    follow: bool,
    clients: Vec<ClientState>,
}

struct ClientState {
    id: u64,
    epoch: u64,
    /// The client's encoder reads the capture GPU surface (no CPU pixels needed for video).
    gpu_input: bool,
    /// The client can show an HDR picture (`proto::VIEW_HDR`).
    hdr: bool,
    /// Content changed since last sent.
    dirty: Vec<bool>,
    /// Client currently shows this tile from the video layer.
    video: Vec<bool>,
    /// Must switch to video: moved content that the client showed from video.
    force_video: Vec<bool>,
    /// Last time a move carried video content into the tile (keeps it video meanwhile).
    video_since: Vec<u64>,
    /// Changed since last copied into the video encoder's input surface.
    stale: Vec<bool>,
    /// Scratch: tiles to send losslessly in the frame being built.
    send: Vec<bool>,
    /// The client's copy may be wrong (a frame touching it was lost): resend
    /// losslessly with an explicit layer switch.
    resync: Vec<bool>,
    /// Screen moves (scroll/copy) to replay on the client's lossless layer, in order.
    copies: Vec<MovedRect>,
    pending: bool,
    oldest_us: u64,
    newest_us: u64,
    cursor_dirty: bool,
    /// The client captured the mouse: it gets the cursor's position.
    capture: bool,
    cursor_pos_dirty: bool,
    seq: u32,
    waker: Thread,
}

impl ClientState {
    fn reset(&mut self, n: usize, now: u64) {
        self.dirty = vec![true; n];
        self.video = vec![false; n];
        self.force_video = vec![false; n];
        self.video_since = vec![0; n];
        self.stale = vec![true; n];
        self.send = vec![false; n];
        self.resync = vec![false; n];
        self.copies.clear();
        self.pending = true;
        self.oldest_us = now;
        self.newest_us = now;
    }
}

pub enum Pull {
    Frame { msg: Vec<u8>, key: bool, contents: FrameContents },
    /// The display changed: send HELLO (its frames start at `first_seq`),
    /// recreate the encoder, then pull again.
    Reset { geometry: Geometry, first_seq: u32, hdr_max: f32 },
    /// Nothing to send; pull again at `wake_at_us` (a video tile cools down) or when woken.
    Idle { wake_at_us: Option<u64> },
}

/// What a frame changes on the client, kept until it is acknowledged so a
/// lost frame can be repaired (see `recovery.rs`).
#[derive(Clone, Debug, Default)]
pub struct FrameContents {
    /// Tiles whose lossless pixels the frame sets.
    pub tiles: Vec<u32>,
    /// Tiles whose layer (lossless / video) the frame switches.
    pub switches: Vec<u32>,
    /// Moves replayed on the client's lossless layer, in order.
    pub moves: Vec<MovedRect>,
    /// The frame carries a video picture (what it shows is restored by the
    /// next keyframe, not tile by tile).
    pub video: bool,
}

/// Calls `f(first_tile, tile_count, rect)` for each horizontal run of set flags,
/// clearing them when `clear`. Every rect lies within one tile row.
fn for_each_run(flags: &mut [bool], g: &Geometry, clear: bool, mut f: impl FnMut(usize, usize, Rect)) {
    let (t, tiles_x) = (g.tile, g.tiles_x());
    for ty in 0..g.height.div_ceil(t) {
        let row = &mut flags[(ty * tiles_x) as usize..((ty + 1) * tiles_x) as usize];
        let mut tx = 0;
        while tx < tiles_x {
            if !row[tx as usize] {
                tx += 1;
                continue;
            }
            let start = tx;
            while tx < tiles_x && row[tx as usize] {
                if clear {
                    row[tx as usize] = false;
                }
                tx += 1;
            }
            let (x, y) = (start * t, ty * t);
            let rect = Rect::new(x, y, (tx * t).min(g.width) - x, ((ty + 1) * t).min(g.height) - y);
            f((ty * tiles_x + start) as usize, (tx - start) as usize, rect);
        }
    }
}

/// What the capture thread may skip or must refresh (see [`Shared::capture_hints`]).
pub struct CaptureHints {
    /// Shown only from video by every client, all reading the GPU surface:
    /// changes need no CPU readback.
    pub skip: Vec<bool>,
    /// Unchanged for a while: CPU copies skipped earlier should be refreshed
    /// before the tile is refined back to lossless.
    pub settled: Vec<bool>,
}

impl Shared {
    pub fn new(tile: u32) -> Self {
        Self {
            inner: Mutex::new(Inner {
                epoch: 0,
                tile,
                pixels: Arc::new(Pixels::new(Geometry { width: 0, height: 0, tile })),
                surface: None,
                hdr_max: 0.0,
                changes: Vec::new(),
                changed_us: 0,
                motion: MotionDetector::default(),
                cursor: None,
                cursor_pos: None,
                follow: false,
                clients: Vec::new(),
            }),
        }
    }

    /// Switches to a new display (or a new picture of it): geometry, blank
    /// screen copy, every client resynchronised (HELLO + full frame +
    /// keyframe). Returns the new copy.
    pub fn reconfigure(&self, width: u32, height: u32, surface: Option<GpuSurface>, hdr_max: f32) -> Arc<Pixels> {
        let mut g = self.inner.lock().unwrap();
        let geometry = Geometry { width, height, tile: g.tile };
        g.pixels = Arc::new(Pixels::new(geometry));
        g.changes = vec![[0; 3]; geometry.tiles()];
        g.changed_us = 0;
        g.motion = MotionDetector::default();
        g.surface = surface;
        g.hdr_max = hdr_max;
        g.epoch += 1;
        for c in &g.clients {
            c.waker.unpark();
        }
        g.pixels.clone()
    }

    pub fn geometry(&self) -> Geometry {
        self.inner.lock().unwrap().pixels.geometry
    }

    /// The picture's `Picture::hdr_max` (0: SDR).
    pub fn hdr_max(&self) -> f32 {
        self.inner.lock().unwrap().hdr_max
    }

    /// Registers a client; its first pull reports the geometry (`Pull::Reset`).
    /// `seq`: its last frame's sequence number on the display it watched
    /// before, so frames of that one still in flight never pass for new ones.
    pub fn add_client(&self, id: u64, waker: Thread, seq: u32) {
        let mut g = self.inner.lock().unwrap();
        let cursor_dirty = g.cursor.is_some();
        g.clients.push(ClientState {
            id,
            // Forces a Reset on the first pull.
            epoch: u64::MAX,
            gpu_input: false,
            hdr: false,
            dirty: Vec::new(),
            video: Vec::new(),
            force_video: Vec::new(),
            video_since: Vec::new(),
            stale: Vec::new(),
            send: Vec::new(),
            resync: Vec::new(),
            copies: Vec::new(),
            pending: false,
            oldest_us: 0,
            newest_us: 0,
            cursor_dirty,
            capture: false,
            cursor_pos_dirty: false,
            seq,
            waker: waker.clone(),
        });
        waker.unpark();
    }

    /// A frame was lost: the client's lossless copy of `tiles` may be wrong
    /// and it may show `layers` from the wrong layer. They are re-sent
    /// losslessly with an explicit switch to the lossless layer. Tiles shown
    /// from video on both sides need nothing: the lossless copy under a video
    /// tile is never relied on (leaving video always re-sends its pixels).
    pub fn resync(&self, id: u64, tiles: &[u32], layers: &[u32]) {
        let mut g = self.inner.lock().unwrap();
        let epoch = g.epoch;
        let Some(c) = g.clients.iter_mut().find(|c| c.id == id && c.epoch == epoch) else { return };
        for &t in layers {
            if let Some(r) = c.resync.get_mut(t as usize) {
                *r = true;
            }
        }
        for &t in tiles {
            if c.video.get(t as usize) == Some(&false) {
                c.resync[t as usize] = true;
            }
        }
        if !c.pending {
            c.pending = true;
            c.oldest_us = proto::now_us();
        }
        c.waker.unpark();
    }

    /// Unregisters a client; returns the sequence number of its last frame,
    /// which continues on the next display it watches (`add_client`).
    pub fn remove_client(&self, id: u64) -> u32 {
        let mut g = self.inner.lock().unwrap();
        let seq = g.clients.iter().find(|c| c.id == id).map_or(0, |c| c.seq);
        g.clients.retain(|c| c.id != id);
        seq
    }

    /// Wakes every client (its display went away: it moves to another).
    pub fn wake_clients(&self) {
        for c in &self.inner.lock().unwrap().clients {
            c.waker.unpark();
        }
    }

    /// Whether the client can show an HDR picture where it is now.
    pub fn set_client_hdr(&self, id: u64, hdr: bool) {
        if let Some(c) = self.inner.lock().unwrap().clients.iter_mut().find(|c| c.id == id) {
            c.hdr = hdr;
        }
    }

    /// One picture for all clients: HDR only when every client can show it.
    /// None without clients (the picture may stay as it is).
    pub fn hdr_wanted(&self) -> Option<bool> {
        let g = self.inner.lock().unwrap();
        (!g.clients.is_empty()).then(|| g.clients.iter().all(|c| c.hdr))
    }

    /// Whether the client's current encoder reads the capture GPU surface.
    pub fn set_gpu_input(&self, id: u64, gpu_input: bool) {
        if let Some(c) = self.inner.lock().unwrap().clients.iter_mut().find(|c| c.id == id) {
            c.gpu_input = gpu_input;
        }
    }

    pub fn set_cursor(&self, msg: Vec<u8>) {
        let mut g = self.inner.lock().unwrap();
        g.cursor = Some(Arc::new(msg));
        for c in &mut g.clients {
            c.cursor_dirty = true;
            c.waker.unpark();
        }
    }

    pub fn take_cursor(&self, id: u64) -> Option<Arc<Vec<u8>>> {
        let mut g = self.inner.lock().unwrap();
        let cursor = g.cursor.clone();
        let c = g.clients.iter_mut().find(|c| c.id == id)?;
        if !std::mem::take(&mut c.cursor_dirty) {
            return None;
        }
        cursor
    }

    /// The client captured the mouse (or let it go): while it has, it gets
    /// the cursor's position (`take_cursor_pos`), the current one at once.
    pub fn set_client_capture(&self, id: u64, on: bool) {
        let mut g = self.inner.lock().unwrap();
        let known = g.cursor_pos.is_some();
        if let Some(c) = g.clients.iter_mut().find(|c| c.id == id) {
            c.capture = on;
            c.cursor_pos_dirty = on && known;
            c.waker.unpark();
        }
    }

    /// Some client, on this display or another, has the mouse captured:
    /// the other clients draw the cursor themselves where it is, since their
    /// user's mouse is in that other window (`take_cursor_pos`).
    pub fn set_follow(&self, on: bool) {
        let mut g = self.inner.lock().unwrap();
        if g.follow == on {
            return;
        }
        g.follow = on;
        let known = g.cursor_pos.is_some();
        for c in g.clients.iter_mut().filter(|c| !c.capture) {
            // On: where it is, once known; off: stop drawing it.
            c.cursor_pos_dirty = !on || known;
            c.waker.unpark();
        }
    }

    /// A client captured the mouse here or elsewhere: the capture loop
    /// follows the cursor.
    pub fn wants_cursor_pos(&self) -> bool {
        let g = self.inner.lock().unwrap();
        g.follow || g.clients.iter().any(|c| c.capture)
    }

    pub fn set_cursor_pos(&self, pos: (u32, u32)) {
        let mut g = self.inner.lock().unwrap();
        if g.cursor_pos == Some(pos) {
            return;
        }
        g.cursor_pos = Some(pos);
        let follow = g.follow;
        for c in g.clients.iter_mut().filter(|c| c.capture || follow) {
            c.cursor_pos_dirty = true;
            c.waker.unpark();
        }
    }

    /// S_CURSOR_POS for this client when the cursor moved: it captures the
    /// mouse, or follows another client's capture (`set_follow`).
    pub fn take_cursor_pos(&self, id: u64) -> Option<Vec<u8>> {
        let mut g = self.inner.lock().unwrap();
        let (follow, pos) = (g.follow, g.cursor_pos);
        let c = g.clients.iter_mut().find(|c| c.id == id)?;
        if !std::mem::take(&mut c.cursor_pos_dirty) {
            return None;
        }
        let flags = if follow && !c.capture { proto::CURSOR_FOLLOW } else { 0 };
        let (x, y) = pos.unwrap_or_default();
        Some(proto::cursor_pos(x, y, flags))
    }

    /// The client's encoder was replaced (another encoded size): its input
    /// is blank, so every tile is copied into it again with the next video
    /// frame.
    pub fn encoder_reset(&self, id: u64) {
        if let Some(c) = self.inner.lock().unwrap().clients.iter_mut().find(|c| c.id == id) {
            c.stale.fill(true);
        }
    }

    /// After a video failure: every video tile goes back to lossless.
    pub fn drop_video(&self, id: u64) {
        let mut g = self.inner.lock().unwrap();
        if let Some(c) = g.clients.iter_mut().find(|c| c.id == id) {
            c.gpu_input = false;
            for t in 0..c.video.len() {
                if std::mem::take(&mut c.video[t]) {
                    c.dirty[t] = true;
                    c.pending = true;
                }
            }
        }
    }

    /// Per-tile hints for the capture thread of a GPU-based session.
    pub fn capture_hints(&self, now: u64) -> CaptureHints {
        let g = self.inner.lock().unwrap();
        let live: Vec<&ClientState> = g.clients.iter().filter(|c| c.epoch == g.epoch).collect();
        let n = g.changes.len();
        let skip = (0..n)
            .map(|t| !live.is_empty() && live.iter().all(|c| c.gpu_input && c.video[t]))
            .collect();
        // A little before the tile may leave video (see `pull`).
        let settled = if g.motion.on {
            vec![now.saturating_sub(g.changed_us) + COOL_US / 2 >= STILL_US; n]
        } else {
            g.changes.iter().map(|ch| now.saturating_sub(ch[0]) >= COOL_US / 2).collect()
        };
        CaptureHints { skip, settled }
    }

    /// Judges how much of the screen is moving and switches between tile
    /// mode and motion mode. Called by the capture thread on every pass,
    /// changes or not.
    pub fn tick(&self, now: u64) {
        let mut g = self.inner.lock().unwrap();
        let hot = g.changes.iter().filter(|ch| ch[2] != 0 && now.saturating_sub(ch[2]) < HOT_WINDOW_US).count();
        let moving = hot > 0 && hot as f64 >= g.changes.len() as f64 * MOTION_SHARE;
        if g.motion.sample(now, moving).is_some() {
            // What may go to video or leave it just changed for everyone.
            for c in &g.clients {
                c.waker.unpark();
            }
        }
    }

    /// The screen is in motion mode (see `pull`).
    pub fn motion(&self) -> bool {
        self.inner.lock().unwrap().motion.on
    }

    /// Publishes what `diff` found: replays its moves for every client and
    /// marks its changed tiles. `refresh` = CPU copies caught up after being
    /// skipped (not new screen changes: motion history is left alone).
    pub fn commit(&self, diff: &Differ, capture_us: u64, refresh: bool) {
        if diff.changed.is_empty() && diff.moves.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        let geometry = g.pixels.geometry;
        if geometry != diff.geometry {
            return;
        }
        let (t, tiles_x) = (geometry.tile, geometry.tiles_x());
        for m in &diff.moves {
            let src = Rect::new(m.src_x, m.src_y, m.dst.w, m.dst.h);
            let (dx, dy) = (m.dst.x as i64 - m.src_x as i64, m.dst.y as i64 - m.src_y as i64);
            for c in g.clients.iter_mut().filter(|c| c.epoch == g.epoch) {
                geometry.for_each_tile(m.dst, |i| c.stale[i] = true);
                if c.copies.len() >= MAX_PENDING_COPIES {
                    geometry.for_each_tile(m.dst, |i| c.dirty[i] = true);
                    continue;
                }
                // The client replays the move on its lossless layer. Source tiles it
                // does not hold exactly land shifted in the destination: unsent
                // changes are resent; content it shows from video stays video there
                // (scrolling video content would otherwise turn into a cascade of
                // lossless resends) and is refined once the moves stop.
                let mut unsynced = Vec::new();
                geometry.for_each_tile(src, |i| {
                    if c.dirty[i] || c.video[i] {
                        let (tx, ty) = (i as u32 % tiles_x * t, i as u32 / tiles_x * t);
                        let x0 = (tx as i64).max(src.x as i64) + dx;
                        let y0 = (ty as i64).max(src.y as i64) + dy;
                        let x1 = ((tx + t) as i64).min((src.x + src.w) as i64) + dx;
                        let y1 = ((ty + t) as i64).min((src.y + src.h) as i64) + dy;
                        if x0 < x1 && y0 < y1 && x0 >= 0 && y0 >= 0 {
                            let r = Rect::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
                            unsynced.push((r, c.video[i]));
                        }
                    }
                });
                for (r, from_video) in unsynced {
                    let Some(r) = r.clip(geometry.width, geometry.height) else { continue };
                    geometry.for_each_tile(r, |i| {
                        if from_video {
                            c.force_video[i] = true;
                            c.video_since[i] = capture_us;
                        } else {
                            c.dirty[i] = true;
                        }
                    });
                }
                c.copies.push(*m);
            }
        }
        for &idx in &diff.changed {
            if !refresh {
                let ch = &mut g.changes[idx];
                *ch = [capture_us, ch[0], ch[1]];
                g.changed_us = g.changed_us.max(capture_us);
            }
            for c in g.clients.iter_mut().filter(|c| c.epoch == g.epoch) {
                // A refreshed tile only matters to clients showing it losslessly.
                if !refresh || !c.video[idx] {
                    c.dirty[idx] = true;
                }
                c.stale[idx] = true;
            }
        }
        if !refresh && !diff.moves.is_empty() {
            g.changed_us = g.changed_us.max(capture_us);
        }
        for c in g.clients.iter_mut().filter(|c| c.epoch == g.epoch) {
            if !c.pending {
                c.pending = true;
                c.oldest_us = capture_us;
            }
            c.newest_us = capture_us;
            c.waker.unpark();
        }
    }

    /// Builds the next FRAME for this client: screen moves, lossless rects
    /// (horizontal runs of tiles, LZ4 or zstd), tile layer switches, and an
    /// H.264 frame when any video tile changed. Without an encoder everything
    /// stays lossless. `link` is the server's model of the client's link.
    pub fn pull(&self, id: u64, codec: &mut ClientCodec, force_key: bool, link: Link) -> Result<Pull> {
        let now = proto::now_us();
        codec.paced = false;
        let mut guard = self.inner.lock().unwrap();
        let Inner { epoch, pixels, changes, clients, surface, changed_us, motion: detector, hdr_max, .. } = &mut *guard;
        let hdr = *hdr_max > 0.0;
        let changed_us = *changed_us;
        let geometry = pixels.geometry;
        let Some(c) = clients.iter_mut().find(|c| c.id == id) else {
            return Ok(Pull::Idle { wake_at_us: None });
        };
        if c.epoch != *epoch {
            if geometry.width == 0 {
                return Ok(Pull::Idle { wake_at_us: None });
            }
            c.epoch = *epoch;
            c.gpu_input = false;
            c.reset(changes.len(), now);
            codec.cost = vec![geometry.tile * geometry.tile; changes.len()];
            return Ok(Pull::Reset { geometry, first_seq: c.seq.wrapping_add(1), hdr_max: *hdr_max });
        }

        let tuning = codec.tuning;
        // Pacing, bursts and refinement follow the (conservative) estimate;
        // what content the link affords follows the budget.
        let rate = link.rate.min(tuning.bandwidth_mbps * 1e6 / 8.0);
        let budget = link.budget(&tuning);
        let slice = rate * FRAME_SLICE_US / 1e6;
        let has_encoder = codec.encoder.is_some();

        // Moving tiles go to video only when keeping them lossless would cost
        // too much: per frame (transfer time adds latency) or per second
        // (budget). Tiles already on video count too (their last lossless
        // size), so the decision is the same whichever layer they are on.
        let hot = |i: usize| changes[i][2] != 0 && now.saturating_sub(changes[i][2]) < HOT_WINDOW_US;
        let (mut hot_frame_bytes, mut hot_rate) = (0u64, 0f64);
        for i in (0..changes.len()).filter(|&i| hot(i)) {
            let cost = codec.cost[i] as u64;
            hot_frame_bytes += cost;
            hot_rate += cost as f64 * 2e6 / (changes[i][0] - changes[i][2]).max(1) as f64;
        }
        let frame_limit = ((tuning.max_lossless_frame_kb * 1024) as f64).min(budget * FRAME_SLICE_US * 2.0 / 1e6);
        let allow_video = has_encoder && (hot_frame_bytes as f64 > frame_limit || hot_rate > budget / 2.0);
        // A large one-off change (window opened, page loaded) would hold the
        // link for many frames losslessly: show it from video now, refine later.
        let fresh_cost: u64 = (0..changes.len())
            .filter(|&i| c.dirty[i] && !c.video[i] && !c.force_video[i] && !(allow_video && hot(i)))
            .map(|i| codec.cost[i] as u64)
            .sum();
        let burst = has_encoder && fresh_cost as f64 > slice * BURST_SLICES;
        // Refinement runs from a token bucket at REFINE_SHARE of the link.
        let refill = rate * REFINE_SHARE * now.saturating_sub(codec.refine_at) as f64 / 1e6;
        codec.refine_tokens = (codec.refine_tokens + refill).min(slice * 2.0);
        codec.refine_at = now;

        // Video frames are paced so their bytes fit the video share of the
        // link: a slow link gets fewer, decent pictures instead of a queue.
        // At most once per client refresh, on average: a grid (`video_slot`)
        // moves one refresh per frame. A capture up to half a refresh early
        // for its slot goes now: a short wait costs a thread wake-up that
        // often comes milliseconds late (measured ~4 ms in the container).
        let min = codec.min_video_us;
        let slot_due = (codec.video_slot + min).saturating_sub(min / 2);
        let video_due = codec.next_video_us.max(slot_due);
        let video_ready = now >= video_due;
        let mut video_deferred = false;
        let mut refine_deferred = false;

        let mut switches: Vec<u16> = Vec::new();
        let mut video_dirty = false;
        // Motion mode: whatever changes is shown from video (no weighing of
        // what it would cost losslessly) and nothing goes back to lossless
        // until the whole screen has been still for a while. Tiles that are
        // exact and do not change stay as they are.
        let motion = detector.on && has_encoder;
        let mut wake_at_us: Option<u64> = None;
        let mut wake = |t: u64| wake_at_us = Some(wake_at_us.map_or(t, |w: u64| w.min(t)));
        for (i, ch) in changes.iter().enumerate() {
            let dirty = std::mem::take(&mut c.dirty[i]);
            let forced = std::mem::take(&mut c.force_video[i]) && has_encoder;
            c.send[i] = false;
            if std::mem::take(&mut c.resync[i]) {
                // Repair after a loss: exact pixels and an explicit layer, now.
                c.video[i] = false;
                c.send[i] = true;
                switches.push(i as u16);
                continue;
            }
            let cools_at = if motion { changed_us + STILL_US } else { ch[0].max(c.video_since[i]) + COOL_US };
            if c.video[i] {
                // Back to lossless once still, or while moving if that is
                // affordable (e.g. it went to video with a one-off burst).
                // Content moved in from video keeps video until it settles.
                let affordable = !motion && hot(i) && !allow_video && now >= c.video_since[i] + COOL_US;
                let cooled = !forced && (now >= cools_at || !has_encoder || affordable);
                // Without an encoder lossless is the only layer: no pacing.
                if cooled && (!has_encoder || codec.refine_tokens > 0.0) {
                    codec.refine_tokens -= codec.cost[i] as f64;
                    c.video[i] = false;
                    c.send[i] = true;
                    switches.push(i as u16);
                } else {
                    if !video_ready && (dirty || forced) {
                        // Keep the change for the next paced video frame.
                        c.dirty[i] = dirty;
                        c.force_video[i] = forced;
                        video_deferred = true;
                    } else {
                        video_dirty |= dirty || forced;
                    }
                    if cooled {
                        // Out of tokens: retry once the bucket has refilled a little.
                        let wait = (-codec.refine_tokens).max(0.0) / (rate * REFINE_SHARE) * 1e6;
                        wake(now + wait as u64 + 1_000);
                        refine_deferred = true;
                    } else {
                        wake(cools_at);
                    }
                }
            } else if forced || (allow_video && hot(i)) || (burst && dirty) || (motion && dirty) {
                if !video_ready {
                    // Switching needs a video picture: wait for the next paced one.
                    c.dirty[i] = dirty;
                    c.force_video[i] = forced;
                    video_deferred = true;
                    continue;
                }
                c.video[i] = true;
                video_dirty = true;
                switches.push(i as u16 | 0x8000);
                wake(cools_at);
            } else {
                c.send[i] = dirty;
            }
        }
        if video_deferred {
            wake(video_due);
        }
        // Only what the estimate held back tells the link model there was
        // more to send: not the configured bandwidth, not the client's
        // refresh rate.
        let estimate_bound = link.rate < tuning.bandwidth_mbps * 1e6 / 8.0;
        codec.paced = estimate_bound && ((video_deferred && now < codec.next_video_us) || refine_deferred);
        let has_lossless = c.send.iter().any(|&s| s);
        if !has_lossless && !video_dirty && switches.is_empty() && c.copies.is_empty() {
            // Deferred video keeps the pending changes' age for latency stats.
            c.pending = video_deferred;
            return Ok(Pull::Idle { wake_at_us });
        }

        codec.rects.clear();
        for_each_run(&mut c.send, &geometry, false, |first, n, r| codec.rects.push((r, first, n)));
        codec.encoder_rects.clear();
        if video_dirty {
            for_each_run(&mut c.stale, &geometry, true, |_, _, r| codec.encoder_rects.push(r));
        }
        if !c.pending {
            c.oldest_us = now;
            c.newest_us = now;
        }
        c.pending = video_deferred;
        c.seq = c.seq.wrapping_add(1);
        let (seq, oldest_us, newest_us) = (c.seq, c.oldest_us, c.newest_us);
        codec.copies.clear();
        codec.copies.append(&mut c.copies);
        let mut contents = FrameContents { moves: codec.copies.clone(), ..Default::default() };
        for &(_, first, n) in &codec.rects {
            contents.tiles.extend((first..first + n).map(|t| t as u32));
        }
        contents.switches.extend(switches.iter().map(|s| (s & 0x7fff) as u32));
        let (pixels, surface) = (pixels.clone(), surface.clone());
        drop(guard);

        // Pixels are read band by band without the metadata lock; content newer
        // than the metadata is harmless (those tiles are dirty again).
        codec.raw.clear();
        for &(r, _, _) in &codec.rects {
            pixels.read_rect(r, &mut codec.raw);
        }
        let load_start = proto::now_us();
        let encoder = if video_dirty { codec.encoder.as_mut() } else { None };
        let loaded = encoder.is_some();
        if let Some(e) = encoder {
            match (e.gpu_input(), surface.as_ref()) {
                (true, Some(s)) => e.load_gpu(s, &codec.encoder_rects)?,
                _ => {
                    let stride = geometry.stride();
                    for r in &codec.encoder_rects {
                        // Rects come per tile row: hand the encoder that band.
                        let row = r.y / geometry.tile;
                        let load = |band: &[u8], y0| e.load_cpu(band, stride, y0, &[*r]);
                        if hdr {
                            pixels.with_band_8bit(row, *r, &mut codec.band8, load)?;
                        } else {
                            pixels.with_band(row, load)?;
                        }
                    }
                }
            }
        }
        let upload_us = proto::now_us() - load_start;

        let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + codec.raw.len() / 2 + 64 * 1024);
        msg.resize(FRAME_HEADER_LEN, 0);
        // Moves replay first, before any rect of this frame.
        for m in &codec.copies {
            for v in [m.src_x, m.src_y, m.dst.x, m.dst.y, m.dst.w, m.dst.h] {
                msg.extend_from_slice(&(v as u16).to_le_bytes());
            }
        }
        let use_zstd = budget < ZSTD_BELOW;
        if use_zstd && codec.zstd.is_none() {
            codec.zstd = Some(zstd::bulk::Compressor::new(ZSTD_LEVEL)?);
        }
        let mut raw_at = 0;
        for &(r, first_tile, n_tiles) in &codec.rects {
            let raw = &codec.raw[raw_at..raw_at + (r.w * r.h * 4) as usize];
            raw_at += raw.len();
            for v in [r.x, r.y, r.w, r.h] {
                msg.extend_from_slice(&(v as u16).to_le_bytes());
            }
            let header_at = msg.len();
            msg.extend_from_slice(&[ENC_RAW, 0, 0, 0, 0, 0, 0, 0]);
            let data_at = msg.len();
            let compressed = match codec.zstd.as_mut().filter(|_| use_zstd) {
                Some(z) => {
                    msg.resize(data_at + zstd::zstd_safe::compress_bound(raw.len()), 0);
                    z.compress_to_buffer(raw, &mut msg[data_at..]).ok().map(|n| (n, ENC_ZSTD))
                }
                None => {
                    msg.resize(data_at + lz4_flex::block::get_maximum_output_size(raw.len()), 0);
                    lz4_flex::block::compress_into(raw, &mut msg[data_at..]).ok().map(|n| (n, ENC_LZ4))
                }
            };
            // Both bounds exceed the raw size, so the raw fallback fits too.
            let len = match compressed {
                Some((n, enc)) if n < raw.len() => {
                    msg[header_at] = enc;
                    n
                }
                _ => {
                    msg[data_at..data_at + raw.len()].copy_from_slice(raw);
                    raw.len()
                }
            };
            msg.truncate(data_at + len);
            msg[header_at + 4..header_at + 8].copy_from_slice(&(len as u32).to_le_bytes());
            codec.cost[first_tile..first_tile + n_tiles].fill(((len / n_tiles) as u32).max(1));
        }
        msg.extend_from_slice(&(switches.len() as u16).to_le_bytes());
        for s in &switches {
            msg.extend_from_slice(&s.to_le_bytes());
        }

        let mut key = false;
        let mut encode_us = 0u32;
        if loaded {
            let encode_start = proto::now_us();
            let v = codec.encoder.as_mut().expect("loaded implies encoder").encode(force_key)?;
            encode_us = (proto::now_us() - encode_start) as u32;
            let paced = (v.data.len() as f64 / (rate * VIDEO_SHARE) * 1e6) as u64;
            // The grid moves one refresh per frame, so a frame that waited
            // for its slot does not push the next one later; after a gap
            // (captures come unevenly) up to two catch up.
            codec.video_slot = (codec.video_slot + min).max(now.saturating_sub(2 * min));
            codec.next_video_us = now + paced;
            key = v.key;
            let wire = codec.encoder.as_ref().expect("loaded implies encoder").codec().wire();
            msg[1] = FLAG_VIDEO | if v.key { FLAG_KEY } else { 0 } | wire << proto::FLAG_CODEC_SHIFT;
            msg.extend_from_slice(&(v.data.len() as u32).to_le_bytes());
            msg.extend_from_slice(&v.data);
        }
        if motion {
            msg[1] |= FLAG_MOTION;
        }
        msg[0] = proto::S_FRAME;
        msg[2..4].copy_from_slice(&(codec.rects.len() as u16).to_le_bytes());
        msg[4..8].copy_from_slice(&seq.to_le_bytes());
        msg[8..16].copy_from_slice(&oldest_us.to_le_bytes());
        msg[16..24].copy_from_slice(&newest_us.to_le_bytes());
        msg[32..40].copy_from_slice(&now.to_le_bytes());
        msg[40..44].copy_from_slice(&encode_us.to_le_bytes());
        msg[44..46].copy_from_slice(&(codec.copies.len() as u16).to_le_bytes());
        msg[46..48].copy_from_slice(&((upload_us / 10).min(u16::MAX as u64) as u16).to_le_bytes());
        contents.video = loaded;
        Ok(Pull::Frame { msg, key, contents })
    }

    /// Capture surface for creating zero-copy encoders.
    pub fn surface(&self) -> Option<GpuSurface> {
        self.inner.lock().unwrap().surface.clone()
    }
}

/// The server's model of one client's link (see [`crate::pacer::Pacer`]).
#[derive(Clone, Copy, Debug)]
pub struct Link {
    /// Estimated bandwidth, bytes/s.
    pub rate: f64,
    /// The link queued at least once: the estimate is its capacity, not
    /// just a lower bound still being raised (slow start).
    pub proven: bool,
}

impl Link {
    /// Bytes/s content may be planned against: the estimate, within the
    /// configured cap. It starts low and slow start raises it as fast as
    /// the link confirms, so the first frames suit a slow link too.
    pub fn budget(&self, tuning: &Tuning) -> f64 {
        self.rate.min(tuning.bandwidth_mbps * 1e6 / 8.0)
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Tuning {
    /// Upper bound on the lossless budget (the link estimate usually decides);
    /// moving areas beyond half of it go to video.
    pub bandwidth_mbps: f64,
    /// Moving areas costing more than this per frame go to video (transfer
    /// time is latency); slow links lower it further.
    pub max_lossless_frame_kb: u64,
}

/// Per-client state owned by its producer thread (outside the shared lock).
pub struct ClientCodec {
    pub encoder: Option<Box<dyn VideoEncoder>>,
    pub tuning: Tuning,
    /// Last compressed size per tile when sent losslessly; drives the video decision.
    cost: Vec<u32>,
    /// Scratch: raw pixels and (rect, first tile, tile count) of lossless rects.
    raw: Vec<u8>,
    /// Scratch: an HDR picture's band in 8 bits, for encoders fed from the CPU.
    band8: Vec<u8>,
    rects: Vec<(Rect, usize, usize)>,
    /// Scratch: areas to refresh in the encoder input.
    encoder_rects: Vec<Rect>,
    copies: Vec<MovedRect>,
    /// Refinement token bucket (bytes) and its last refill time.
    refine_tokens: f64,
    refine_at: u64,
    zstd: Option<zstd::bulk::Compressor<'static>>,
    /// Earliest time of the next video frame (video pacing).
    next_video_us: u64,
    /// Least time between video frames: the client's refresh interval.
    pub min_video_us: u64,
    /// Slot on that grid the last video frame took.
    video_slot: u64,
    /// The last pull held content back for pacing (video or refinement).
    paced: bool,
}

impl ClientCodec {
    pub fn new(tuning: Tuning) -> Self {
        Self {
            encoder: None,
            tuning,
            cost: Vec::new(),
            raw: Vec::new(),
            band8: Vec::new(),
            rects: Vec::new(),
            encoder_rects: Vec::new(),
            copies: Vec::new(),
            refine_tokens: 0.0,
            refine_at: 0,
            zstd: None,
            next_video_us: 0,
            min_video_us: 0,
            video_slot: 0,
            paced: false,
        }
    }

    /// The last pull held content back to stay within the link estimate.
    pub fn paced(&self) -> bool {
        self.paced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds ten samples per second for `seconds` and returns the modes the
    /// detector switched to, with the second of each switch.
    fn run(seconds: u64, moving: impl Fn(u64) -> bool) -> Vec<(u64, bool)> {
        let mut d = MotionDetector::default();
        (0..seconds * 10).filter_map(|i| d.sample(1 + i * 100_000, moving(i / 10)).map(|on| (i / 10, on))).collect()
    }

    #[test]
    fn motion_needs_ten_seconds_in_a_row() {
        assert_eq!(run(60, |_| true), [(10, true)]);
        // Nine moving seconds, one quiet, again and again: never.
        assert_eq!(run(300, |s| s % 10 != 9), []);
    }

    #[test]
    fn motion_leaves_only_after_forty_quiet_seconds_in_a_row() {
        assert_eq!(run(100, |s| s < 20), [(10, true), (60, false)]);
        // One moving second every 39 keeps it on.
        assert_eq!(run(400, |s| s < 20 || s % 39 == 0), [(10, true)]);
    }

    /// Short videos between spells of desktop work: one switch, not one per video.
    #[test]
    fn motion_does_not_flip_between_short_videos() {
        assert_eq!(run(600, |s| s % 25 < 12), [(10, true)]);
    }
}
