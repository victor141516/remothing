//! DXGI pointer shapes → straight-alpha RGBA, and change tracking.
//!
//! Browsers cannot XOR with the screen, so inverting pixels (monochrome
//! AND=1/XOR=1, masked-color XOR with a non-black colour) become opaque black:
//! the I-beam and similar cursors stay visible on the usual light backgrounds.

use std::sync::Arc;

use remothing_core::platform::{CursorShape, CursorState};
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

const BLACK: [u8; 4] = [0, 0, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// Converts a shape returned by `GetFramePointerShape`; `None` for unknown types
/// or a buffer smaller than the shape claims.
pub fn convert(info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> Option<CursorShape> {
    let pitch = info.Pitch as usize;
    let width = info.Width as usize;
    let kind = info.Type as i32;
    // Monochrome shapes stack the AND mask on top of the XOR mask.
    let height = if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 { info.Height as usize / 2 } else { info.Height as usize };
    let rows = if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 { height * 2 } else { height };
    if width == 0 || height == 0 || buf.len() < pitch * rows {
        return None;
    }
    let mut rgba = Vec::with_capacity(width * height * 4);
    match kind {
        k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 => {
            if pitch * 8 < width {
                return None;
            }
            let bit = |row: &[u8], x: usize| (row[x / 8] >> (7 - x % 8)) & 1;
            for y in 0..height {
                let and = &buf[y * pitch..][..pitch];
                let xor = &buf[(y + height) * pitch..][..pitch];
                for x in 0..width {
                    rgba.extend_from_slice(match (bit(and, x), bit(xor, x)) {
                        (0, 0) => &BLACK,
                        (0, _) => &WHITE,
                        (_, 0) => &CLEAR,
                        _ => &BLACK,
                    });
                }
            }
        }
        k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 || k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 => {
            if pitch < width * 4 {
                return None;
            }
            let masked = k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0;
            for y in 0..height {
                for px in buf[y * pitch..][..width * 4].chunks_exact(4) {
                    let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
                    rgba.extend_from_slice(&if !masked {
                        [r, g, b, a]
                    } else if a == 0 {
                        // Mask clear: the colour replaces the screen pixel.
                        [r, g, b, 255]
                    } else if (r, g, b) == (0, 0, 0) {
                        // XOR with black leaves the screen unchanged.
                        CLEAR
                    } else {
                        BLACK
                    });
                }
            }
        }
        _ => return None,
    }
    Some(CursorShape {
        width: width as u32,
        height: height as u32,
        hot_x: (info.HotSpot.x.max(0) as u32).min(width as u32 - 1),
        hot_y: (info.HotSpot.y.max(0) as u32).min(height as u32 - 1),
        rgba,
    })
}

/// Pointer visibility and shape as last seen by the duplication, reported only on change.
#[derive(Default)]
pub struct CursorTracker {
    visible: bool,
    shape: Option<Arc<CursorShape>>,
    changed: bool,
}

impl CursorTracker {
    pub fn set_visible(&mut self, visible: bool) {
        if visible != self.visible {
            self.visible = visible;
            self.changed = true;
        }
    }

    pub fn set_shape(&mut self, shape: CursorShape) {
        if self.shape.as_deref() != Some(&shape) {
            self.shape = Some(Arc::new(shape));
            self.changed = true;
        }
    }

    pub fn take_change(&mut self) -> Option<CursorState> {
        if !self.changed {
            return None;
        }
        if !self.visible {
            self.changed = false;
            return Some(CursorState::Hidden);
        }
        // Visible before any shape arrived: wait for the shape (DXGI sends it
        // with the first update that shows the pointer on this output).
        let shape = self.shape.clone()?;
        self.changed = false;
        Some(CursorState::Visible(shape))
    }
}
