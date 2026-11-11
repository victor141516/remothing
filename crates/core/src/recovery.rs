//! Frames in flight and their repair after a loss.
//!
//! Frames sent over a UDP path (WebTransport, the data channel) can be lost:
//! a datagram whose pieces never came back, or a path that died with frames
//! in flight; the client skips them. The
//! lossless layer is state, not history, so repairing it means re-sending the
//! current pixels of every tile the lost frame touched. Moves make it
//! subtler: the client replays later moves on a copy that misses the lost
//! frame, so wrong content spreads from move sources to destinations. A lost
//! layer switch leaves the client showing a tile from the wrong layer. Video
//! is repaired as a whole: the decoder lost a reference, so the next video
//! frame must be a keyframe, which also restores everything the lost
//! picture showed.

use std::collections::VecDeque;

use crate::geom::Rect;
use crate::pixels::Geometry;
use crate::shared::FrameContents;

struct Sent {
    seq: u32,
    sent_us: u64,
    /// Went over a UDP path (can be lost; WebSocket frames cannot).
    udp: bool,
    contents: FrameContents,
}

#[derive(Debug, PartialEq)]
pub struct Repair {
    /// Tiles whose lossless copy on the client may be wrong.
    pub tiles: Vec<u32>,
    /// Tiles the client may show from the wrong layer.
    pub layers: Vec<u32>,
    /// The client's decoder lost a reference picture.
    pub keyframe: bool,
}

#[derive(Default)]
pub struct SentLog {
    frames: VecDeque<Sent>,
}

/// `a` is at or before `b` in wrapping sequence order.
fn seq_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) as i32 >= 0
}

impl SentLog {
    pub fn push(&mut self, seq: u32, sent_us: u64, udp: bool, contents: FrameContents) {
        self.frames.push_back(Sent { seq, sent_us, udp, contents });
    }

    /// Every frame up to `seq` was drawn or reported lost: forget them.
    pub fn ack(&mut self, seq: u32) {
        while self.frames.front().is_some_and(|f| seq_le(f.seq, seq)) {
            self.frames.pop_front();
        }
    }

    /// The display changed: older frames will never matter again.
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Frame `seq` never reached the client. None when unknown (already
    /// repaired: losses are reported more than once, or a previous display).
    pub fn lost(&mut self, seq: u32, geometry: &Geometry) -> Option<Repair> {
        let at = self.frames.iter().position(|f| f.seq == seq)?;
        let lost = self.frames.remove(at)?;
        let mut tainted = vec![false; geometry.tiles()];
        for &t in lost.contents.tiles.iter().chain(&lost.contents.switches) {
            if let Some(x) = tainted.get_mut(t as usize) {
                *x = true;
            }
        }
        // Moves of the lost frame were not replayed: their destinations still
        // hold what was there before.
        for m in &lost.contents.moves {
            geometry.for_each_tile(clip(m.dst, geometry), |t| tainted[t] = true);
        }
        // Later moves carried wrong content further: each tainted part of a
        // move's source lands shifted in its destination (frames at or after
        // the removed position are the later ones).
        for later in self.frames.iter().skip(at) {
            for m in &later.contents.moves {
                let src = Rect::new(m.src_x, m.src_y, m.dst.w, m.dst.h);
                let (dx, dy) = (m.dst.x as i64 - m.src_x as i64, m.dst.y as i64 - m.src_y as i64);
                // The client copies the whole source before writing (one move is atomic).
                let before = tainted.clone();
                for t in before.iter().enumerate().filter(|(_, &x)| x).map(|(t, _)| t) {
                    let Some(part) = intersect(tile_rect(t, geometry), src) else { continue };
                    let (x, y) = (part.x as i64 + dx, part.y as i64 + dy);
                    if x < 0 || y < 0 {
                        continue;
                    }
                    let shifted = Rect::new(x as u32, y as u32, part.w, part.h);
                    geometry.for_each_tile(clip(shifted, geometry), |d| tainted[d] = true);
                }
            }
        }
        let tiles = tainted.iter().enumerate().filter(|(_, &t)| t).map(|(i, _)| i as u32).collect();
        Some(Repair { tiles, layers: lost.contents.switches, keyframe: lost.contents.video })
    }

    /// UDP-path frames sent before `before_us` and still unacknowledged.
    pub fn older_than(&self, before_us: u64) -> Vec<u32> {
        self.frames.iter().take_while(|f| f.sent_us < before_us).filter(|f| f.udp).map(|f| f.seq).collect()
    }

    /// Send time of the oldest unacknowledged UDP-path frame.
    pub fn oldest_udp_us(&self) -> Option<u64> {
        self.frames.iter().find(|f| f.udp).map(|f| f.sent_us)
    }
}

fn clip(r: Rect, g: &Geometry) -> Rect {
    r.clip(g.width, g.height).unwrap_or_default()
}

fn tile_rect(t: usize, g: &Geometry) -> Rect {
    let tiles_x = g.tiles_x() as usize;
    Rect::new((t % tiles_x) as u32 * g.tile, (t / tiles_x) as u32 * g.tile, g.tile, g.tile)
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    (x0 < x1 && y0 < y1).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::MovedRect;

    // 4 x 4 tiles of 64 px.
    const G: Geometry = Geometry { width: 256, height: 256, tile: 64 };

    fn frame(tiles: &[u32], moves: &[MovedRect], video: bool) -> FrameContents {
        FrameContents { tiles: tiles.to_vec(), moves: moves.to_vec(), video, ..Default::default() }
    }

    /// Scroll up by one tile row over the whole screen.
    fn scroll_up() -> MovedRect {
        MovedRect { src_x: 0, src_y: 64, dst: Rect::new(0, 0, 256, 192) }
    }

    #[test]
    fn lost_frame_taints_its_tiles_and_what_later_moves_carried_away() {
        let mut log = SentLog::default();
        log.push(1, 0, true, frame(&[13], &[], false)); // row 3, column 1
        log.push(2, 0, true, frame(&[], &[scroll_up()], false));
        log.push(3, 0, true, frame(&[], &[scroll_up()], false));
        let repair = log.lost(1, &G).unwrap();
        // Tile 13 itself, then moved to row 2 (9) and row 1 (5).
        assert_eq!(repair.tiles, vec![5, 9, 13]);
        assert!(!repair.keyframe);
        // Reported again (redundant ACKs): nothing left to repair.
        assert_eq!(log.lost(1, &G), None);
    }

    #[test]
    fn lost_layer_switch_is_reported_with_its_tile() {
        // The frame switched tile 6 to video and carried the picture: the
        // client still shows its lossless copy there.
        let mut log = SentLog::default();
        log.push(1, 0, true, FrameContents { switches: vec![6], video: true, ..Default::default() });
        let repair = log.lost(1, &G).unwrap();
        assert_eq!((repair.tiles, repair.layers, repair.keyframe), (vec![6], vec![6], true));
    }

    #[test]
    fn lost_move_taints_its_destination() {
        let mut log = SentLog::default();
        log.push(7, 0, true, frame(&[], &[scroll_up()], true));
        let repair = log.lost(7, &G).unwrap();
        assert_eq!(repair.tiles, (0..12).collect::<Vec<u32>>());
        assert!(repair.keyframe);
    }

    #[test]
    fn earlier_moves_do_not_spread_a_later_loss() {
        let mut log = SentLog::default();
        log.push(1, 0, true, frame(&[], &[scroll_up()], false));
        log.push(2, 0, true, frame(&[13], &[], false));
        assert_eq!(log.lost(2, &G).unwrap().tiles, vec![13]);
    }

    #[test]
    fn acks_wrap_around() {
        let mut log = SentLog::default();
        for seq in [u32::MAX - 1, u32::MAX, 0, 1] {
            log.push(seq, 0, true, FrameContents::default());
        }
        log.ack(0);
        assert_eq!(log.older_than(1), vec![1]);
    }
}
