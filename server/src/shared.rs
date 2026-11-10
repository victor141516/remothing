//! Latest screen contents, per-tile motion history and per-client state.
//!
//! The capture thread is the only writer of `fb`. Each client pulls the union
//! of tiles changed since its last frame, so a slow client skips intermediate
//! frames instead of queueing them (bounded latency, always converges to the
//! exact current screen).
//!
//! Every tile is shown by the client either from the lossless layer (exact
//! pixels) or from the video layer (H.264 of the whole screen). Tiles that keep
//! changing move to video; once they stop changing they are re-sent losslessly,
//! so anything that stands still ends up pixel-exact.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;

use anyhow::Result;

use crate::proto::{self, ENC_LZ4, ENC_RAW, FLAG_KEY, FLAG_VIDEO, FRAME_HEADER_LEN};
use crate::video::VideoEncoder;
use crate::xfb::XvfbFramebuffer;

/// A tile is "moving" when its 3rd most recent change is this recent (>= ~20 Hz).
const HOT_WINDOW_US: u64 = 150_000;
/// A video tile unchanged for this long is refined back to lossless.
const COOL_US: u64 = 250_000;

pub struct Shared {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
}

struct Inner {
    width: u32,
    height: u32,
    tile: u32,
    tiles_x: u32,
    tiles_y: u32,
    /// BGRX, stride = width * 4.
    fb: Vec<u8>,
    /// Last three change times per tile, most recent first (0 = never).
    changes: Vec<[u64; 3]>,
    cursor: Option<Arc<Vec<u8>>>,
    clients: Vec<ClientState>,
    /// Scratch: tiles touched by the current damage batch.
    touched: Vec<bool>,
}

struct ClientState {
    id: u64,
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

pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub tile: u32,
}

pub enum Pull {
    Frame { msg: Vec<u8>, key: bool },
    /// Nothing to send; pull again at `wake_at_us` (a video tile cools down) or when woken.
    Idle { wake_at_us: Option<u64> },
}

impl Shared {
    pub fn new(width: u32, height: u32, tile: u32) -> Self {
        let tiles_x = width.div_ceil(tile);
        let tiles_y = height.div_ceil(tile);
        let n = (tiles_x * tiles_y) as usize;
        Self {
            inner: Mutex::new(Inner {
                width,
                height,
                tile,
                tiles_x,
                tiles_y,
                fb: vec![0; (width * height * 4) as usize],
                changes: vec![[0; 3]; n],
                cursor: None,
                clients: Vec::new(),
                touched: vec![false; n],
            }),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn geometry(&self) -> Geometry {
        let g = self.inner.lock().unwrap();
        Geometry { width: g.width, height: g.height, tile: g.tile }
    }

    pub fn new_client_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers a client; its first frame is the full screen, lossless.
    pub fn add_client(&self, id: u64, waker: Thread) {
        let mut g = self.inner.lock().unwrap();
        let n = g.touched.len();
        let now = proto::now_us();
        let cursor_dirty = g.cursor.is_some();
        g.clients.push(ClientState {
            id,
            dirty: vec![true; n],
            video: vec![false; n],
            stale: vec![true; n],
            send: vec![false; n],
            pending: true,
            oldest_us: now,
            newest_us: now,
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
        if !c.cursor_dirty {
            return None;
        }
        c.cursor_dirty = false;
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

    /// Copies damaged areas from the X framebuffer, keeping only tiles whose
    /// pixels actually changed. Returns the number of changed tiles.
    pub fn ingest(&self, src: &XvfbFramebuffer, rects: &[Rect], capture_us: u64) -> usize {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        let t = g.tile;
        for r in rects {
            let x1 = (r.x + r.w).min(g.width);
            let y1 = (r.y + r.h).min(g.height);
            if r.x >= x1 || r.y >= y1 {
                continue;
            }
            for ty in r.y / t..=(y1 - 1) / t {
                for tx in r.x / t..=(x1 - 1) / t {
                    g.touched[(ty * g.tiles_x + tx) as usize] = true;
                }
            }
        }
        let stride = g.width as usize * 4;
        let mut changed = 0;
        for idx in 0..g.touched.len() {
            if !std::mem::take(&mut g.touched[idx]) {
                continue;
            }
            let tx = idx as u32 % g.tiles_x;
            let ty = idx as u32 / g.tiles_x;
            let x0 = (tx * t) as usize * 4;
            let x1 = (((tx + 1) * t).min(g.width)) as usize * 4;
            let y0 = ty * t;
            let y1 = ((ty + 1) * t).min(g.height);
            let mut differs = false;
            for y in y0..y1 {
                let s = &src.row(y)[x0..x1];
                let d = &mut g.fb[y as usize * stride + x0..y as usize * stride + x1];
                if differs || s != d {
                    differs = true;
                    d.copy_from_slice(s);
                }
            }
            if differs {
                // Rows above the first difference were equal, so the tile is now exact.
                changed += 1;
                let ch = &mut g.changes[idx];
                *ch = [capture_us, ch[0], ch[1]];
                for c in &mut g.clients {
                    c.dirty[idx] = true;
                    c.stale[idx] = true;
                }
            }
        }
        if changed > 0 {
            for c in &mut g.clients {
                if !c.pending {
                    c.pending = true;
                    c.oldest_us = capture_us;
                }
                c.newest_us = capture_us;
                c.waker.unpark();
            }
        }
        changed
    }

    /// Builds the next FRAME for this client: lossless rects (horizontal runs
    /// of tiles, LZ4), tile layer switches, and an H.264 frame when any video
    /// tile changed. Without an encoder everything stays lossless.
    pub fn pull(&self, id: u64, codec: &mut ClientCodec, force_key: bool) -> Result<Pull> {
        let now = proto::now_us();
        let mut guard = self.inner.lock().unwrap();
        let Inner { width, height, tile, tiles_x, tiles_y, fb, changes, clients, .. } = &mut *guard;
        let (t, width, height, tiles_x, tiles_y) = (*tile, *width, *height, *tiles_x, *tiles_y);
        let Some(c) = clients.iter_mut().find(|c| c.id == id) else {
            return Ok(Pull::Idle { wake_at_us: None });
        };

        // Moving tiles go to video only when keeping them lossless would cost
        // too much: per frame (transfer time adds latency) or per second (budget).
        let hot = |i: usize| changes[i][2] != 0 && now.saturating_sub(changes[i][2]) < HOT_WINDOW_US;
        let (mut hot_frame_bytes, mut hot_rate) = (0u64, 0f64);
        for i in (0..changes.len()).filter(|&i| hot(i) && !c.video[i]) {
            let cost = codec.cost[i] as u64;
            hot_frame_bytes += cost;
            hot_rate += cost as f64 * 2e6 / (changes[i][0] - changes[i][2]).max(1) as f64;
        }
        let allow_video = codec.encoder.is_some()
            && (hot_frame_bytes > codec.max_lossless_frame_bytes || hot_rate > codec.budget_bytes_per_sec / 2.0);

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
        let stride = width as usize * 4;
        codec.raw.clear();
        codec.rects.clear();
        for ty in 0..tiles_y {
            let row = &c.send[(ty * tiles_x) as usize..((ty + 1) * tiles_x) as usize];
            let mut tx = 0;
            while tx < tiles_x {
                if !row[tx as usize] {
                    tx += 1;
                    continue;
                }
                let start = tx;
                while tx < tiles_x && row[tx as usize] {
                    tx += 1;
                }
                let (x, y) = (start * t, ty * t);
                let w = (tx * t).min(width) - x;
                let h = ((ty + 1) * t).min(height) - y;
                let bx = x as usize * 4;
                for yy in y..y + h {
                    let o = yy as usize * stride + bx;
                    codec.raw.extend_from_slice(&fb[o..o + w as usize * 4]);
                }
                codec.rects.push(([x, y, w, h], (ty * tiles_x + start) as usize, (tx - start) as usize));
            }
        }

        if !c.pending {
            c.oldest_us = now;
            c.newest_us = now;
        }
        c.pending = false;
        c.seq = c.seq.wrapping_add(1);
        let (seq, oldest_us, newest_us) = (c.seq, c.oldest_us, c.newest_us);

        let mut encode_us = 0u32;
        let encoder = if video_dirty { codec.encoder.as_mut() } else { None };
        let loaded = encoder.is_some();
        let load_start = proto::now_us();
        if let Some(e) = encoder {
            if c.stale.iter().all(|&s| s) {
                e.load_rect(fb, stride, 0, 0, width, height)?;
                c.stale.fill(false);
            } else {
                for ty in 0..tiles_y {
                    let row = &mut c.stale[(ty * tiles_x) as usize..((ty + 1) * tiles_x) as usize];
                    let mut tx = 0;
                    while tx < tiles_x {
                        if !row[tx as usize] {
                            tx += 1;
                            continue;
                        }
                        let start = tx;
                        while tx < tiles_x && row[tx as usize] {
                            row[tx as usize] = false;
                            tx += 1;
                        }
                        let (x, y) = (start * t, ty * t);
                        e.load_rect(fb, stride, x, y, (tx * t).min(width) - x, ((ty + 1) * t).min(height) - y)?;
                    }
                }
            }
        }
        drop(guard);

        let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + codec.raw.len() / 2 + 64 * 1024);
        msg.resize(FRAME_HEADER_LEN, 0);
        let mut raw_at = 0;
        for &([x, y, w, h], first_tile, n_tiles) in &codec.rects {
            let raw = &codec.raw[raw_at..raw_at + (w * h * 4) as usize];
            raw_at += raw.len();
            msg.extend_from_slice(&(x as u16).to_le_bytes());
            msg.extend_from_slice(&(y as u16).to_le_bytes());
            msg.extend_from_slice(&(w as u16).to_le_bytes());
            msg.extend_from_slice(&(h as u16).to_le_bytes());
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
            let per_tile = (len / n_tiles) as u32;
            codec.cost[first_tile..first_tile + n_tiles].fill(per_tile.max(1));
        }
        msg.extend_from_slice(&(switches.len() as u16).to_le_bytes());
        for s in &switches {
            msg.extend_from_slice(&s.to_le_bytes());
        }

        let mut key = false;
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
}

/// Per-client state owned by its producer thread (outside the shared lock).
pub struct ClientCodec {
    pub encoder: Option<VideoEncoder>,
    /// Last compressed size per tile when sent losslessly; drives the video decision.
    cost: Vec<u32>,
    budget_bytes_per_sec: f64,
    max_lossless_frame_bytes: u64,
    /// Scratch: raw pixels and geometry ([x, y, w, h], first tile, tile count) of lossless rects.
    raw: Vec<u8>,
    rects: Vec<([u32; 4], usize, usize)>,
}

impl ClientCodec {
    pub fn new(g: &Geometry, encoder: Option<VideoEncoder>, budget_mbps: f64, max_lossless_frame_kb: u64) -> Self {
        let n = (g.width.div_ceil(g.tile) * g.height.div_ceil(g.tile)) as usize;
        Self {
            encoder,
            // Until measured, assume ~4:1 compression.
            cost: vec![g.tile * g.tile; n],
            budget_bytes_per_sec: budget_mbps * 1e6 / 8.0,
            max_lossless_frame_bytes: max_lossless_frame_kb * 1024,
            raw: Vec::new(),
            rects: Vec::new(),
        }
    }
}
