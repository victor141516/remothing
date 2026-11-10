/// Pixel rectangle in display coordinates (origin top-left of the captured display).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Clips to a `width` x `height` surface; `None` when nothing remains.
    pub fn clip(self, width: u32, height: u32) -> Option<Rect> {
        let x1 = (self.x.saturating_add(self.w)).min(width);
        let y1 = (self.y.saturating_add(self.h)).min(height);
        (self.x < x1 && self.y < y1).then(|| Rect::new(self.x, self.y, x1 - self.x, y1 - self.y))
    }
}

/// A screen area whose contents moved: `dst` now shows what was at (`src_x`, `src_y`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MovedRect {
    pub src_x: u32,
    pub src_y: u32,
    pub dst: Rect,
}
