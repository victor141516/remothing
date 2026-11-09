//! Latest screen contents plus per-client dirty-tile tracking.
//!
//! The capture thread is the only writer of `fb`. Each client pulls the union
//! of tiles changed since its last frame, so a slow client skips intermediate
//! frames instead of queueing them (bounded latency, always converges to the
//! exact current screen).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::proto::{self, ENC_RAW, FRAME_HEADER_LEN};
use crate::xfb::XvfbFramebuffer;

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
    cursor: Option<Arc<Vec<u8>>>,
    clients: Vec<ClientState>,
    /// Scratch: tiles touched by the current damage batch.
    touched: Vec<bool>,
}

struct ClientState {
    id: u64,
    dirty: Vec<bool>,
    pending: bool,
    oldest_us: u64,
    newest_us: u64,
    cursor_dirty: bool,
    seq: u32,
    notify: Arc<Notify>,
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

    /// Registers a client; its first frame is the full screen.
    pub fn add_client(&self, notify: Arc<Notify>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        let n = g.touched.len();
        let now = proto::now_us();
        let cursor_dirty = g.cursor.is_some();
        g.clients.push(ClientState {
            id,
            dirty: vec![true; n],
            pending: true,
            oldest_us: now,
            newest_us: now,
            cursor_dirty,
            seq: 0,
            notify: notify.clone(),
        });
        notify.notify_one();
        id
    }

    pub fn remove_client(&self, id: u64) {
        self.inner.lock().unwrap().clients.retain(|c| c.id != id);
    }

    pub fn set_cursor(&self, msg: Vec<u8>) {
        let mut g = self.inner.lock().unwrap();
        g.cursor = Some(Arc::new(msg));
        for c in &mut g.clients {
            c.cursor_dirty = true;
            c.notify.notify_one();
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
                for c in &mut g.clients {
                    c.dirty[idx] = true;
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
                c.notify.notify_one();
            }
        }
        changed
    }

    /// Builds a FRAME message with every tile dirty for this client, merging
    /// horizontal runs of dirty tiles into single rects.
    pub fn take_frame(&self, id: u64) -> Option<Vec<u8>> {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        let ci = g.clients.iter().position(|c| c.id == id)?;
        if !g.clients[ci].pending {
            return None;
        }
        let (t, tiles_x, tiles_y, width, height) = (g.tile, g.tiles_x, g.tiles_y, g.width, g.height);
        let stride = width as usize * 4;
        let c = &mut g.clients[ci];
        let fb = &g.fb;

        let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + 64 * 1024);
        msg.resize(FRAME_HEADER_LEN, 0);
        let mut n_rects: u16 = 0;
        for ty in 0..tiles_y {
            let row = &mut c.dirty[(ty * tiles_x) as usize..((ty + 1) * tiles_x) as usize];
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
                let x = start * t;
                let y = ty * t;
                let w = (tx * t).min(width) - x;
                let h = ((ty + 1) * t).min(height) - y;
                let len = w * h * 4;
                msg.extend_from_slice(&(x as u16).to_le_bytes());
                msg.extend_from_slice(&(y as u16).to_le_bytes());
                msg.extend_from_slice(&(w as u16).to_le_bytes());
                msg.extend_from_slice(&(h as u16).to_le_bytes());
                msg.extend_from_slice(&[ENC_RAW, 0, 0, 0]);
                msg.extend_from_slice(&len.to_le_bytes());
                debug_assert_eq!(msg.len() % 4, 0);
                let bx = x as usize * 4;
                for yy in y..y + h {
                    let o = yy as usize * stride + bx;
                    msg.extend_from_slice(&fb[o..o + w as usize * 4]);
                }
                n_rects += 1;
            }
        }
        c.pending = false;
        c.seq = c.seq.wrapping_add(1);
        msg[0] = proto::S_FRAME;
        msg[2..4].copy_from_slice(&n_rects.to_le_bytes());
        msg[4..8].copy_from_slice(&c.seq.to_le_bytes());
        msg[8..16].copy_from_slice(&c.oldest_us.to_le_bytes());
        msg[16..24].copy_from_slice(&c.newest_us.to_le_bytes());
        drop(guard);
        let send_us = proto::now_us();
        msg[24..32].copy_from_slice(&send_us.to_le_bytes());
        Some(msg)
    }
}
