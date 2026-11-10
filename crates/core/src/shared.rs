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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;

use anyhow::Result;

use crate::encode::VideoEncoder;
use crate::geom::{MovedRect, Rect};
pub use crate::pixels::Geometry;
use crate::pixels::{Differ, Pixels};
use crate::platform::GpuSurface;
use crate::proto::{self, ENC_LZ4, ENC_RAW, ENC_ZSTD, FLAG_KEY, FLAG_VIDEO, FRAME_HEADER_LEN};

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
const ZSTD_BELOW: f64 = 200e6 / 8.0;
const ZSTD_LEVEL: i32 = 1;
/// Share of the link the video layer may use (lossless refinement and
/// protocol overhead need the rest). Its bitrate cap and frame pacing follow it.
pub const VIDEO_SHARE: f64 = 0.7;

pub struct Shared {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
}

struct Inner {
    /// Bumped whenever the streamed display (and so the geometry) changes.
    epoch: u64,
    tile: u32,
    pixels: Arc<Pixels>,
    /// Capture GPU surface for zero-copy encoders.
    surface: Option<GpuSurface>,
    /// Last three change times per tile, most recent first (0 = never).
    changes: Vec<[u64; 3]>,
    cursor: Option<Arc<Vec<u8>>>,
    clients: Vec<ClientState>,
}

struct ClientState {
    id: u64,
    epoch: u64,
    /// The client's encoder reads the capture GPU surface (no CPU pixels needed for video).
    gpu_input: bool,
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
    Reset { geometry: Geometry, first_seq: u32 },
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
                changes: Vec::new(),
                cursor: None,
                clients: Vec::new(),
            }),
            next_id: AtomicU64::new(1),
        }
    }

    /// Switches to a new display: geometry, blank screen copy, every client
    /// resynchronised (HELLO + full frame + keyframe). Returns the new copy.
    pub fn reconfigure(&self, width: u32, height: u32, surface: Option<GpuSurface>) -> Arc<Pixels> {
        let mut g = self.inner.lock().unwrap();
        let geometry = Geometry { width, height, tile: g.tile };
        g.pixels = Arc::new(Pixels::new(geometry));
        g.changes = vec![[0; 3]; geometry.tiles()];
        g.surface = surface;
        g.epoch += 1;
        for c in &g.clients {
            c.waker.unpark();
        }
        g.pixels.clone()
    }

    pub fn geometry(&self) -> Geometry {
        self.inner.lock().unwrap().pixels.geometry
    }

    pub fn new_client_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers a client; its first pull reports the geometry (`Pull::Reset`).
    pub fn add_client(&self, id: u64, waker: Thread) {
        let mut g = self.inner.lock().unwrap();
        let cursor_dirty = g.cursor.is_some();
        g.clients.push(ClientState {
            id,
            // Forces a Reset on the first pull.
            epoch: u64::MAX,
            gpu_input: false,
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
            seq: 0,
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

    pub fn remove_client(&self, id: u64) {
        self.inner.lock().unwrap().clients.retain(|c| c.id != id);
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
        let settled = g.changes.iter().map(|ch| now.saturating_sub(ch[0]) >= COOL_US / 2).collect();
        CaptureHints { skip, settled }
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
            }
            for c in g.clients.iter_mut().filter(|c| c.epoch == g.epoch) {
                // A refreshed tile only matters to clients showing it losslessly.
                if !refresh || !c.video[idx] {
                    c.dirty[idx] = true;
                }
                c.stale[idx] = true;
            }
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
        let Inner { epoch, pixels, changes, clients, surface, .. } = &mut *guard;
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
            return Ok(Pull::Reset { geometry, first_seq: c.seq.wrapping_add(1) });
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
        let video_ready = now >= codec.next_video_us;
        let mut video_deferred = false;
        let mut refine_deferred = false;

        let mut switches: Vec<u16> = Vec::new();
        let mut video_dirty = false;
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
            let cools_at = ch[0].max(c.video_since[i]) + COOL_US;
            if c.video[i] {
                // Back to lossless once still, or while moving if that is
                // affordable (e.g. it went to video with a one-off burst).
                // Content moved in from video keeps video until it settles.
                let affordable = hot(i) && !allow_video && now >= c.video_since[i] + COOL_US;
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
            } else if forced || (allow_video && hot(i)) || (burst && dirty) {
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
            wake(codec.next_video_us);
        }
        codec.paced = video_deferred || refine_deferred;
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
                        pixels.with_band(r.y / geometry.tile, |band, y0| e.load_cpu(band, stride, y0, &[*r]))?;
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
            codec.next_video_us = now + (v.data.len() as f64 / (rate * VIDEO_SHARE) * 1e6) as u64;
            key = v.key;
            msg[1] = FLAG_VIDEO | if v.key { FLAG_KEY } else { 0 };
            msg.extend_from_slice(&(v.data.len() as u32).to_le_bytes());
            msg.extend_from_slice(&v.data);
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
    /// just a lower bound.
    pub proven: bool,
}

impl Link {
    /// Bytes/s content may be planned against: the configured cap, lowered to
    /// the estimate once the link has shown where it runs full. Before that,
    /// a fast link must not be treated as slow because content stayed light.
    pub fn budget(&self, tuning: &Tuning) -> f64 {
        let cap = tuning.bandwidth_mbps * 1e6 / 8.0;
        if self.proven {
            self.rate.min(cap)
        } else {
            cap
        }
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
            rects: Vec::new(),
            encoder_rects: Vec::new(),
            copies: Vec::new(),
            refine_tokens: 0.0,
            refine_at: 0,
            zstd: None,
            next_video_us: 0,
            paced: false,
        }
    }

    /// The last pull held content back to stay within the link estimate.
    pub fn paced(&self) -> bool {
        self.paced
    }
}
