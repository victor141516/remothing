//! Latest screen contents, per-tile motion history and per-client state.
//!
//! The capture thread is the only writer of `fb`. Each client pulls the union
//! of tiles changed since its last frame, so a slow client skips intermediate
//! frames instead of queueing them (bounded latency, always converges to the
//! exact current screen).
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
use crate::geom::Rect;
use crate::platform::GpuSurface;
use crate::proto::{self, ENC_LZ4, ENC_RAW, FLAG_KEY, FLAG_VIDEO, FRAME_HEADER_LEN};

/// A tile is "moving" when its 3rd most recent change is this recent (>= ~20 Hz).
const HOT_WINDOW_US: u64 = 150_000;
/// A video tile unchanged for this long is refined back to lossless.
const COOL_US: u64 = 250_000;

pub struct Shared {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
}

struct Inner {
    /// Bumped whenever the streamed display (and so the geometry) changes.
    epoch: u64,
    width: u32,
    height: u32,
    tile: u32,
    tiles_x: u32,
    tiles_y: u32,
    /// BGRX, stride = width * 4.
    fb: Vec<u8>,
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
    /// Content changed since last sent.
    dirty: Vec<bool>,
    /// Client currently shows this tile from the video layer.
    video: Vec<bool>,
    /// Changed since last copied into the video encoder's input surface.
    stale: Vec<bool>,
    /// Scratch: tiles to send losslessly in the frame being built.
    send: Vec<bool>,
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
        self.pending = true;
        self.oldest_us = now;
        self.newest_us = now;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub tile: u32,
}

pub enum Pull {
    Frame { msg: Vec<u8>, key: bool },
    /// The display changed: send HELLO, recreate the encoder, then pull again.
    Reset(Geometry),
    /// Nothing to send; pull again at `wake_at_us` (a video tile cools down) or when woken.
    Idle { wake_at_us: Option<u64> },
}

/// Calls `f(first_tile, tile_count, rect)` for each horizontal run of set flags,
/// clearing them when `clear`.
fn for_each_run(flags: &mut [bool], g: &Geometry, clear: bool, mut f: impl FnMut(usize, usize, Rect)) {
    let (t, tiles_x) = (g.tile, g.width.div_ceil(g.tile));
    let tiles_y = g.height.div_ceil(t);
    for ty in 0..tiles_y {
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

impl Inner {
    fn geometry(&self) -> Geometry {
        Geometry { width: self.width, height: self.height, tile: self.tile }
    }

    fn configure(&mut self, width: u32, height: u32, surface: Option<GpuSurface>) {
        self.width = width;
        self.height = height;
        self.tiles_x = width.div_ceil(self.tile);
        self.tiles_y = height.div_ceil(self.tile);
        let n = (self.tiles_x * self.tiles_y) as usize;
        self.fb = vec![0; (width * height * 4) as usize];
        self.surface = surface;
        self.changes = vec![[0; 3]; n];
        self.epoch += 1;
    }
}

impl Shared {
    pub fn new(tile: u32) -> Self {
        Self {
            inner: Mutex::new(Inner {
                epoch: 0,
                width: 0,
                height: 0,
                tile,
                tiles_x: 0,
                tiles_y: 0,
                fb: Vec::new(),
                surface: None,
                changes: Vec::new(),
                cursor: None,
                clients: Vec::new(),
            }),
            next_id: AtomicU64::new(1),
        }
    }

    /// Switches to a new display: geometry, blank framebuffer, every client
    /// resynchronised (HELLO + full frame + keyframe).
    pub fn reconfigure(&self, width: u32, height: u32, surface: Option<GpuSurface>) {
        let mut g = self.inner.lock().unwrap();
        g.configure(width, height, surface);
        for c in &g.clients {
            c.waker.unpark();
        }
    }

    pub fn geometry(&self) -> Geometry {
        self.inner.lock().unwrap().geometry()
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
            dirty: Vec::new(),
            video: Vec::new(),
            stale: Vec::new(),
            send: Vec::new(),
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
            for t in 0..c.video.len() {
                if std::mem::take(&mut c.video[t]) {
                    c.dirty[t] = true;
                    c.pending = true;
                }
            }
        }
    }

    /// Publishes tiles found changed by `diff` (whose copy holds the new pixels).
    /// Only the changed tiles are copied under the lock.
    pub fn commit(&self, diff: &TileDiff, capture_us: u64) {
        if diff.changed.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        if g.geometry() != diff.geometry {
            return;
        }
        let stride = g.width as usize * 4;
        for &idx in &diff.changed {
            let (x0, len, y0, y1) = diff.tile_span(idx);
            for o in (y0..y1).map(|y| y * stride + x0) {
                g.fb[o..o + len].copy_from_slice(&diff.copy[o..o + len]);
            }
            let ch = &mut g.changes[idx];
            *ch = [capture_us, ch[0], ch[1]];
            for c in g.clients.iter_mut().filter(|c| c.epoch == g.epoch) {
                c.dirty[idx] = true;
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

    /// Builds the next FRAME for this client: lossless rects (horizontal runs
    /// of tiles, LZ4), tile layer switches, and an H.264 frame when any video
    /// tile changed. Without an encoder everything stays lossless.
    pub fn pull(&self, id: u64, codec: &mut ClientCodec, force_key: bool) -> Result<Pull> {
        let now = proto::now_us();
        let mut guard = self.inner.lock().unwrap();
        let geometry = guard.geometry();
        let Inner { epoch, fb, changes, clients, surface, .. } = &mut *guard;
        let Some(c) = clients.iter_mut().find(|c| c.id == id) else {
            return Ok(Pull::Idle { wake_at_us: None });
        };
        if c.epoch != *epoch {
            if geometry.width == 0 {
                return Ok(Pull::Idle { wake_at_us: None });
            }
            c.epoch = *epoch;
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
        if !has_lossless && !video_dirty && switches.is_empty() {
            c.pending = false;
            return Ok(Pull::Idle { wake_at_us });
        }

        // Gather lossless rects (raw) under the lock; compress after releasing it.
        let stride = geometry.width as usize * 4;
        codec.raw.clear();
        codec.rects.clear();
        for_each_run(&mut c.send, &geometry, false, |first, n, r| {
            let bx = r.x as usize * 4;
            for y in r.y..r.y + r.h {
                let o = y as usize * stride + bx;
                codec.raw.extend_from_slice(&fb[o..o + r.w as usize * 4]);
            }
            codec.rects.push((r, first, n));
        });

        if !c.pending {
            c.oldest_us = now;
            c.newest_us = now;
        }
        c.pending = false;
        c.seq = c.seq.wrapping_add(1);
        let (seq, oldest_us, newest_us) = (c.seq, c.oldest_us, c.newest_us);

        let load_start = proto::now_us();
        let encoder = if video_dirty { codec.encoder.as_mut() } else { None };
        let loaded = encoder.is_some();
        if let Some(e) = encoder {
            let mut stale = Vec::new();
            if c.stale.iter().all(|&s| s) {
                c.stale.fill(false);
                stale.push(Rect::new(0, 0, geometry.width, geometry.height));
            } else {
                for_each_run(&mut c.stale, &geometry, true, |_, _, r| stale.push(r));
            }
            match (e.gpu_input(), surface.as_ref()) {
                (true, Some(s)) => e.load_gpu(s, &stale)?,
                _ => e.load_cpu(fb, stride, &stale)?,
            }
        }
        drop(guard);

        let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + codec.raw.len() / 2 + 64 * 1024);
        msg.resize(FRAME_HEADER_LEN, 0);
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
            let v = codec.encoder.as_mut().expect("loaded implies encoder").encode(force_key)?;
            encode_us = (proto::now_us() - load_start) as u32;
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

/// Capture-thread tile differ: compares new pixels against its own copy of the
/// screen without holding the shared lock, so only truly changed tiles are
/// published (damage over-reports: blinking redraws, identical repaints...).
pub struct TileDiff {
    geometry: Geometry,
    tiles_x: u32,
    /// BGRX, stride = width * 4: the screen as last published.
    copy: Vec<u8>,
    touched: Vec<bool>,
    changed: Vec<usize>,
}

impl TileDiff {
    pub fn new(geometry: Geometry) -> Self {
        let tiles_x = geometry.width.div_ceil(geometry.tile);
        let n = (tiles_x * geometry.height.div_ceil(geometry.tile)) as usize;
        Self {
            geometry,
            tiles_x,
            copy: vec![0; (geometry.width * geometry.height * 4) as usize],
            touched: vec![false; n],
            changed: Vec::new(),
        }
    }

    /// Tile `idx` as (x offset in bytes, row length in bytes, first row, end row).
    fn tile_span(&self, idx: usize) -> (usize, usize, usize, usize) {
        let g = &self.geometry;
        let t = g.tile;
        let (tx, ty) = (idx as u32 % self.tiles_x, idx as u32 / self.tiles_x);
        let x0 = (tx * t) as usize * 4;
        let len = ((tx + 1) * t).min(g.width) as usize * 4 - x0;
        (x0, len, (ty * t) as usize, ((ty + 1) * t).min(g.height) as usize)
    }

    /// Compares the `rects` areas of `src` (full-display BGRX, `stride` bytes per
    /// row) tile by tile; changed tiles are copied in and listed for `commit`.
    pub fn diff(&mut self, src: &[u8], stride: usize, rects: &[Rect]) -> usize {
        let g = self.geometry;
        let t = g.tile;
        for r in rects.iter().filter_map(|r| r.clip(g.width, g.height)) {
            for ty in r.y / t..=(r.y + r.h - 1) / t {
                for tx in r.x / t..=(r.x + r.w - 1) / t {
                    self.touched[(ty * self.tiles_x + tx) as usize] = true;
                }
            }
        }
        self.changed.clear();
        let own_stride = g.width as usize * 4;
        for idx in 0..self.touched.len() {
            if !std::mem::take(&mut self.touched[idx]) {
                continue;
            }
            let mut differs = false;
            let (x0, len, y0, y1) = self.tile_span(idx);
            for y in y0..y1 {
                let s = &src[y * stride + x0..y * stride + x0 + len];
                let d = &mut self.copy[y * own_stride + x0..y * own_stride + x0 + len];
                // Rows above the first difference were equal, so the tile ends up exact.
                if differs || s != d {
                    differs = true;
                    d.copy_from_slice(s);
                }
            }
            if differs {
                self.changed.push(idx);
            }
        }
        self.changed.len()
    }
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
}

impl ClientCodec {
    pub fn new(tuning: Tuning) -> Self {
        Self { encoder: None, tuning, cost: Vec::new(), raw: Vec::new(), rects: Vec::new() }
    }
}
