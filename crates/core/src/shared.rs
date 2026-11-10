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
use crate::proto::{self, ENC_LZ4, ENC_RAW, FLAG_KEY, FLAG_VIDEO, FRAME_HEADER_LEN};

/// A tile is "moving" when its 3rd most recent change is this recent (>= ~20 Hz).
const HOT_WINDOW_US: u64 = 150_000;
/// A video tile unchanged for this long is refined back to lossless.
const COOL_US: u64 = 250_000;
/// Moves queued for one client before falling back to resending destinations.
const MAX_PENDING_COPIES: usize = 64;

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
    /// Changed since last copied into the video encoder's input surface.
    stale: Vec<bool>,
    /// Scratch: tiles to send losslessly in the frame being built.
    send: Vec<bool>,
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
        self.stale = vec![true; n];
        self.send = vec![false; n];
        self.copies.clear();
        self.pending = true;
        self.oldest_us = now;
        self.newest_us = now;
    }
}

pub enum Pull {
    Frame { msg: Vec<u8>, key: bool },
    /// The display changed: send HELLO, recreate the encoder, then pull again.
    Reset(Geometry),
    /// Nothing to send; pull again at `wake_at_us` (a video tile cools down) or when woken.
    Idle { wake_at_us: Option<u64> },
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
            stale: Vec::new(),
            send: Vec::new(),
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
                // The client replays the move on its lossless layer; source tiles it
                // does not hold exactly (unsent changes, video) land shifted in the
                // destination, so exactly those destination areas are resent.
                let mut unsynced = Vec::new();
                geometry.for_each_tile(src, |i| {
                    if c.dirty[i] || c.video[i] {
                        let (tx, ty) = (i as u32 % tiles_x * t, i as u32 / tiles_x * t);
                        let x0 = (tx as i64).max(src.x as i64) + dx;
                        let y0 = (ty as i64).max(src.y as i64) + dy;
                        let x1 = ((tx + t) as i64).min((src.x + src.w) as i64) + dx;
                        let y1 = ((ty + t) as i64).min((src.y + src.h) as i64) + dy;
                        if x0 < x1 && y0 < y1 && x0 >= 0 && y0 >= 0 {
                            unsynced.push(Rect::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32));
                        }
                    }
                });
                for r in unsynced.into_iter().filter_map(|r| r.clip(geometry.width, geometry.height)) {
                    geometry.for_each_tile(r, |i| c.dirty[i] = true);
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
    /// (horizontal runs of tiles, LZ4), tile layer switches, and an H.264 frame
    /// when any video tile changed. Without an encoder everything stays lossless.
    pub fn pull(&self, id: u64, codec: &mut ClientCodec, force_key: bool) -> Result<Pull> {
        let now = proto::now_us();
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
            return Ok(Pull::Reset(geometry));
        }

        // Moving tiles go to video only when keeping them lossless would cost
        // too much: per frame (transfer time adds latency) or per second (budget).
        let hot = |i: usize| changes[i][2] != 0 && now.saturating_sub(changes[i][2]) < HOT_WINDOW_US;
        let (mut hot_frame_bytes, mut hot_rate) = (0u64, 0f64);
        for i in (0..changes.len()).filter(|&i| hot(i) && !c.video[i]) {
            let cost = codec.cost[i] as u64;
            hot_frame_bytes += cost;
            hot_rate += cost as f64 * 2e6 / (changes[i][0] - changes[i][2]).max(1) as f64;
        }
        let tuning = codec.tuning;
        let allow_video = codec.encoder.is_some()
            && (hot_frame_bytes > tuning.max_lossless_frame_kb * 1024
                || hot_rate > tuning.bandwidth_mbps * 1e6 / 8.0 / 2.0);

        let mut switches: Vec<u16> = Vec::new();
        let mut video_dirty = false;
        let mut wake_at_us: Option<u64> = None;
        for i in 0..changes.len() {
            let dirty = std::mem::take(&mut c.dirty[i]);
            c.send[i] = false;
            if c.video[i] {
                let cools_at = changes[i][0] + COOL_US;
                if now >= cools_at || codec.encoder.is_none() {
                    c.video[i] = false;
                    c.send[i] = true;
                    switches.push(i as u16);
                } else {
                    video_dirty |= dirty;
                    wake_at_us = Some(wake_at_us.map_or(cools_at, |w| w.min(cools_at)));
                }
            } else if allow_video && hot(i) {
                c.video[i] = true;
                video_dirty = true;
                switches.push(i as u16 | 0x8000);
                let cools_at = changes[i][0] + COOL_US;
                wake_at_us = Some(wake_at_us.map_or(cools_at, |w| w.min(cools_at)));
            } else {
                c.send[i] = dirty;
            }
        }
        let has_lossless = c.send.iter().any(|&s| s);
        if !has_lossless && !video_dirty && switches.is_empty() && c.copies.is_empty() {
            c.pending = false;
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
        c.pending = false;
        c.seq = c.seq.wrapping_add(1);
        let (seq, oldest_us, newest_us) = (c.seq, c.oldest_us, c.newest_us);
        codec.copies.clear();
        codec.copies.append(&mut c.copies);
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
                        pixels.with_band(r.y / geometry.tile, |band, y0| {
                            e.load_cpu(band, stride, &[Rect::new(r.x, r.y - y0, r.w, r.h)])
                        })?;
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
            msg.resize(data_at + lz4_flex::block::get_maximum_output_size(raw.len()), 0);
            let len = match lz4_flex::block::compress_into(raw, &mut msg[data_at..]) {
                Ok(n) if n < raw.len() => {
                    msg[header_at] = ENC_LZ4;
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
        Ok(Pull::Frame { msg, key })
    }

    /// Capture surface for creating zero-copy encoders.
    pub fn surface(&self) -> Option<GpuSurface> {
        self.inner.lock().unwrap().surface.clone()
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Tuning {
    /// Lossless bandwidth budget; moving areas beyond half of it go to video.
    pub bandwidth_mbps: f64,
    /// Moving areas costing more than this per frame go to video (transfer time is latency).
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
        }
    }
}
