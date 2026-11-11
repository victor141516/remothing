//! The shared screen copy, split into bands of one tile row with their own
//! lock, plus the capture-side differ that updates it in place.
//!
//! The capture thread compares new pixels against the bands and writes the
//! changed tiles directly; client threads read the bands they send. A band
//! lock is held only while one tile row is compared or read, so capture and
//! clients rarely wait on each other and no pixel is copied twice.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::geom::{MovedRect, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub tile: u32,
}

impl Geometry {
    pub fn tiles_x(&self) -> u32 {
        self.width.div_ceil(self.tile)
    }

    pub fn tiles(&self) -> usize {
        (self.tiles_x() * self.height.div_ceil(self.tile)) as usize
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// Tile `idx` as a pixel rect.
    pub fn tile_rect(&self, idx: usize) -> Rect {
        let (t, tx) = (self.tile, self.tiles_x());
        let (x, y) = (idx as u32 % tx * t, idx as u32 / tx * t);
        Rect::new(x, y, (x + t).min(self.width) - x, (y + t).min(self.height) - y)
    }

    /// Calls `f(tile_index)` for every tile overlapping `r` (already clipped).
    pub fn for_each_tile(&self, r: Rect, mut f: impl FnMut(usize)) {
        if r.w == 0 || r.h == 0 {
            return;
        }
        let (t, tiles_x) = (self.tile, self.tiles_x());
        for ty in r.y / t..=(r.y + r.h - 1) / t {
            for tx in r.x / t..=(r.x + r.w - 1) / t {
                f((ty * tiles_x + tx) as usize);
            }
        }
    }
}

/// Screen copy, 4 bytes per pixel (BGRX, or R10G10B10A2 for an HDR picture,
/// see `platform::Picture`), one band per tile row.
pub struct Pixels {
    pub geometry: Geometry,
    bands: Vec<Mutex<Vec<u8>>>,
}

impl Pixels {
    pub fn new(geometry: Geometry) -> Self {
        let rows = geometry.height.div_ceil(geometry.tile);
        let bands = (0..rows)
            .map(|b| {
                let h = ((b + 1) * geometry.tile).min(geometry.height) - b * geometry.tile;
                Mutex::new(vec![0; geometry.stride() * h as usize])
            })
            .collect();
        Self { geometry, bands }
    }

    /// Copies `r` (inside one tile row) as tightly packed BGRX rows into `out`.
    pub fn read_rect(&self, r: Rect, out: &mut Vec<u8>) {
        let t = self.geometry.tile;
        let stride = self.geometry.stride();
        let band = self.bands[(r.y / t) as usize].lock().unwrap();
        let (bx, wb) = (r.x as usize * 4, r.w as usize * 4);
        for y in r.y % t..r.y % t + r.h {
            let o = y as usize * stride + bx;
            out.extend_from_slice(&band[o..o + wb]);
        }
    }

    /// Runs `f(band_pixels, band_origin_y)` with the band of tile row `row` locked.
    pub fn with_band<R>(&self, row: u32, f: impl FnOnce(&[u8], u32) -> R) -> R {
        let band = self.bands[row as usize].lock().unwrap();
        f(&band, row * self.geometry.tile)
    }

    /// Runs `f(band_pixels, band_origin_y)` for the band of tile row `row`
    /// with an HDR picture's 10-bit pixels turned into the 8-bit BGRX an
    /// encoder takes (the same curve, fewer codes), in `scratch`. Only the
    /// columns of `r` are converted.
    pub fn with_band_8bit<R>(&self, row: u32, r: Rect, scratch: &mut Vec<u8>, f: impl FnOnce(&[u8], u32) -> R) -> R {
        let stride = self.geometry.stride();
        let band = self.bands[row as usize].lock().unwrap();
        scratch.resize(band.len(), 0);
        let (bx, wb) = (r.x as usize * 4, r.w as usize * 4);
        let to8 = |v: u32| ((v & 0x3ff) * 255 + 511) / 1023;
        for (src, dst) in band.chunks_exact(stride).zip(scratch.chunks_exact_mut(stride)) {
            for (s, d) in src[bx..bx + wb].chunks_exact(4).zip(dst[bx..bx + wb].chunks_exact_mut(4)) {
                let p = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                d.copy_from_slice(&[to8(p >> 20) as u8, to8(p >> 10) as u8, to8(p) as u8, 255]);
            }
        }
        drop(band);
        f(scratch, row * self.geometry.tile)
    }

    fn row_hash(&self, y: u32, bx: usize, wb: usize) -> u64 {
        let t = self.geometry.tile;
        let o = (y % t) as usize * self.geometry.stride() + bx;
        row_hash(&self.bands[(y / t) as usize].lock().unwrap()[o..o + wb])
    }

    fn row_eq(&self, y: u32, bx: usize, other: &[u8]) -> bool {
        let t = self.geometry.tile;
        let o = (y % t) as usize * self.geometry.stride() + bx;
        self.bands[(y / t) as usize].lock().unwrap()[o..o + other.len()] == *other
    }

    /// Applies a screen move, correct for overlapping source and destination.
    pub fn apply_move(&self, m: &MovedRect, scratch: &mut Vec<u8>) {
        let t = self.geometry.tile;
        let stride = self.geometry.stride();
        let (wb, h) = (m.dst.w as usize * 4, m.dst.h);
        let (sx, dx) = (m.src_x as usize * 4, m.dst.x as usize * 4);
        // Moving down: walk rows bottom-up so unread source rows are not overwritten.
        let rows: Box<dyn Iterator<Item = u32>> = if m.dst.y > m.src_y { Box::new((0..h).rev()) } else { Box::new(0..h) };
        for i in rows {
            let (sy, dy) = (m.src_y + i, m.dst.y + i);
            scratch.clear();
            {
                let band = self.bands[(sy / t) as usize].lock().unwrap();
                let o = (sy % t) as usize * stride + sx;
                scratch.extend_from_slice(&band[o..o + wb]);
            }
            let mut band = self.bands[(dy / t) as usize].lock().unwrap();
            let o = (dy % t) as usize * stride + dx;
            band[o..o + wb].copy_from_slice(scratch);
        }
    }
}

pub fn row_hash(row: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut chunks = row.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
    }
    for &b in chunks.remainder() {
        h = (h ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    h
}

/// Capture-thread differ for one display session.
pub struct Differ {
    pub geometry: Geometry,
    touched: Vec<bool>,
    /// Tiles whose pixels changed in the last `diff`.
    pub changed: Vec<usize>,
    /// Moves applied by the last `diff`, replayed by `commit`.
    pub moves: Vec<MovedRect>,
    /// Tiles whose CPU copy is behind the screen (skipped while video-only).
    cpu_stale: Vec<bool>,
    /// Large changes to skip before trying scroll detection again after a miss
    /// (keeps the cost off full-screen video content).
    scroll_backoff: u32,
    row_index: HashMap<u64, u32>,
    scratch: Vec<u8>,
}

impl Differ {
    pub fn new(geometry: Geometry) -> Self {
        let n = geometry.tiles();
        Self {
            geometry,
            touched: vec![false; n],
            changed: Vec::new(),
            moves: Vec::new(),
            cpu_stale: vec![false; n],
            scroll_backoff: 0,
            row_index: HashMap::new(),
            scratch: Vec::new(),
        }
    }

    /// Tiles marked `skip` (shown only from video, encoders read the GPU
    /// surface) are reported changed without reading pixels; their CPU copy is
    /// refreshed later through [`Differ::stale_tiles`]. Returns the tiles to
    /// read and compare, merged into horizontal runs within each tile row.
    pub fn mark_rects(&mut self, rects: &[Rect], skip: &[bool]) -> Vec<Rect> {
        let g = self.geometry;
        for r in rects.iter().filter_map(|r| r.clip(g.width, g.height)) {
            g.for_each_tile(r, |i| self.touched[i] = true);
        }
        self.changed.clear();
        let mut read: Vec<Rect> = Vec::new();
        for i in 0..self.touched.len() {
            if !std::mem::take(&mut self.touched[i]) {
                continue;
            }
            if skip.get(i).copied().unwrap_or(false) {
                self.cpu_stale[i] = true;
                self.changed.push(i);
                continue;
            }
            let r = g.tile_rect(i);
            match read.last_mut() {
                Some(last) if last.y == r.y && last.x + last.w == r.x => last.w += r.w,
                _ => read.push(r),
            }
        }
        read
    }

    /// CPU-stale tiles that should be read now (no longer skipped, or settled).
    pub fn stale_tiles(&mut self, refresh: impl Fn(usize) -> bool) -> Vec<Rect> {
        let g = self.geometry;
        let mut out: Vec<Rect> = Vec::new();
        for (i, stale) in self.cpu_stale.iter_mut().enumerate() {
            if *stale && refresh(i) {
                *stale = false;
                let r = g.tile_rect(i);
                // Merged into horizontal runs: fewer, larger readbacks.
                match out.last_mut() {
                    Some(last) if last.y == r.y && last.x + last.w == r.x => last.w += r.w,
                    _ => out.push(r),
                }
            }
        }
        out
    }

    /// Applies `moved` (or, when `find_scroll` and there are none, a detected
    /// vertical scroll) to `px`, then compares the `tiles` rects of `src`
    /// (full-display BGRX, `stride` bytes per row) with `px`, writing changed
    /// tiles in place. Appends changed tile indices to `changed`.
    pub fn diff(
        &mut self,
        px: &Pixels,
        src: &[u8],
        stride: usize,
        tiles: &[Rect],
        moved: &[MovedRect],
        find_scroll: bool,
    ) {
        let g = self.geometry;
        self.moves.clear();
        for m in moved {
            let src_rect = Rect::new(m.src_x, m.src_y, m.dst.w, m.dst.h);
            if src_rect.clip(g.width, g.height) == Some(src_rect) && m.dst.clip(g.width, g.height) == Some(m.dst) {
                px.apply_move(m, &mut self.scratch);
                self.moves.push(*m);
            }
        }
        if moved.is_empty() && find_scroll {
            if let Some(m) = self.detect_scroll(px, src, stride, tiles) {
                px.apply_move(&m, &mut self.scratch);
                self.moves.push(m);
            }
        }
        let own_stride = g.stride();
        let t = g.tile;
        // Tiles come grouped by tile row; lock each band once.
        let mut k = 0;
        while k < tiles.len() {
            let row = tiles[k].y / t;
            let end = k + tiles[k..].iter().take_while(|r| r.y / t == row).count();
            let mut band = px.bands[row as usize].lock().unwrap();
            for run in &tiles[k..end] {
                // A run spans several tiles of this row; each tile is compared on its own.
                let mut x = run.x;
                while x < run.x + run.w {
                    let tw = (x / t * t + t).min(run.x + run.w) - x;
                    let (bx, wb) = (x as usize * 4, tw as usize * 4);
                    let mut differs = false;
                    for y in run.y..run.y + run.h {
                        let s = &src[y as usize * stride + bx..y as usize * stride + bx + wb];
                        let o = (y % t) as usize * own_stride + bx;
                        let d = &mut band[o..o + wb];
                        // Rows above the first difference were equal, so the tile ends up exact.
                        if differs || s != d {
                            differs = true;
                            d.copy_from_slice(s);
                        }
                    }
                    if differs {
                        self.changed.push((row * g.tiles_x() + x / t) as usize);
                    }
                    x += tw;
                }
            }
            k = end;
        }
    }

    /// Finds a vertical scroll inside the bounding box of `tiles` (providers
    /// like XDamage report no moves): sample rows of the new frame are looked
    /// up by hash among the old rows, the shift with most votes is verified on
    /// spread rows. Wrong guesses only cost bandwidth: the tile diff that
    /// follows still compares every touched tile against the moved copy.
    fn detect_scroll(&mut self, px: &Pixels, src: &[u8], stride: usize, tiles: &[Rect]) -> Option<MovedRect> {
        const MIN_HEIGHT: u32 = 128;
        const MIN_WIDTH: u32 = 256;
        const SAMPLES: u32 = 48;
        const BACKOFF: u32 = 10;
        const AMBIGUOUS: u32 = u32::MAX;
        let first = tiles.first()?;
        let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x + first.w, first.y + first.h);
        for r in tiles {
            (x0, y0, x1, y1) = (x0.min(r.x), y0.min(r.y), x1.max(r.x + r.w), y1.max(r.y + r.h));
        }
        if y1 - y0 < MIN_HEIGHT || x1 - x0 < MIN_WIDTH {
            return None;
        }
        if self.scroll_backoff > 0 {
            self.scroll_backoff -= 1;
            return None;
        }
        let (bx, wb) = (x0 as usize * 4, (x1 - x0) as usize * 4);
        let new_row = |y: u32| &src[y as usize * stride + bx..y as usize * stride + bx + wb];

        // Rows that repeat (blank lines, patterns) cannot tell the shift apart:
        // they are marked ambiguous and never vote.
        self.row_index.clear();
        for y in y0..y1 {
            self.row_index.entry(px.row_hash(y, bx, wb)).and_modify(|v| *v = AMBIGUOUS).or_insert(y);
        }
        let mut votes: Vec<(i64, u32)> = Vec::new();
        let mut samples = 0;
        let step = ((y1 - y0) / (SAMPLES * 4)).max(1);
        for y in (y0..y1).step_by(step as usize) {
            let Some(&oy) = self.row_index.get(&row_hash(new_row(y))) else { continue };
            if oy == AMBIGUOUS {
                continue;
            }
            let dy = y as i64 - oy as i64;
            match votes.iter_mut().find(|v| v.0 == dy) {
                Some(v) => v.1 += 1,
                None => votes.push((dy, 1)),
            }
            samples += 1;
            if samples == SAMPLES {
                break;
            }
        }
        let found = votes.iter().filter(|v| v.0 != 0).max_by_key(|v| v.1).map(|v| v.0).filter(|&dy| {
            let (top, bottom) = (y0 as i64 + dy.max(0), y1 as i64 + dy.min(0));
            if bottom - top < (MIN_HEIGHT / 2) as i64 {
                return false;
            }
            let check = ((bottom - top) / 32).max(1) as usize;
            let (mut ok, mut total) = (0, 0);
            for y in (top..bottom).step_by(check) {
                total += 1;
                ok += px.row_eq((y - dy) as u32, bx, new_row(y as u32)) as u32;
            }
            ok * 10 >= total * 7
        });
        let Some(dy) = found else {
            self.scroll_backoff = BACKOFF;
            return None;
        };
        let top = (y0 as i64 + dy.max(0)) as u32;
        let h = ((y1 as i64 + dy.min(0)) - top as i64) as u32;
        Some(MovedRect { src_x: x0, src_y: (top as i64 - dy) as u32, dst: Rect::new(x0, top, x1 - x0, h) })
    }
}
