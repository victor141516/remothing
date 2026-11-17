//! Displays as Quartz sees them. Positions and sizes of the desktop are in
//! points, pictures in pixels (twice as many each way on a Retina display):
//! `Geometry` converts between the two for input and the cursor.

use anyhow::{bail, Context, Result};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsActive, CGDisplayIsBuiltin, CGDisplayMode,
    CGDisplayPixelsHigh, CGDisplayPixelsWide, CGError, CGGetActiveDisplayList, CGMainDisplayID,
};
use remothing_core::platform::{DisplayInfo, DisplayKind};

const ID_PREFIX: &str = "sck:";

/// Where a display is on the desktop and how big its picture is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    pub id: CGDirectDisplayID,
    /// Desktop rectangle, points, origin at the top left of the main display.
    pub bounds: CGRect,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
}

impl Geometry {
    pub fn of(id: CGDirectDisplayID) -> Result<Self> {
        let bounds = CGDisplayBounds(id);
        let (width, height, refresh_hz) = match CGDisplayCopyDisplayMode(id) {
            Some(mode) => (
                CGDisplayMode::pixel_width(Some(&mode)) as u32,
                CGDisplayMode::pixel_height(Some(&mode)) as u32,
                CGDisplayMode::refresh_rate(Some(&mode)),
            ),
            // A display just added (a virtual one) has no mode in this process
            // until the main run loop delivered the change
            // (`with_main_run_loop`); its sizes are current already. They
            // are points, which are pixels on our virtual displays (no HiDPI).
            None if CGDisplayIsActive(id) => (CGDisplayPixelsWide(id) as u32, CGDisplayPixelsHigh(id) as u32, 0.0),
            None => bail!("display {id} is not active (disconnected?)"),
        };
        if width == 0 || height == 0 || bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            bail!("display {id} reports no size");
        }
        // Built-in panels report 0 Hz; they run at 60 or, with ProMotion, up to 120.
        let refresh_hz = if refresh_hz > 0.0 { refresh_hz } else { 60.0 };
        Ok(Self { id, bounds, width, height, refresh_hz })
    }

    /// Picture pixels per point, horizontally and vertically.
    pub fn scale(&self) -> (f64, f64) {
        (self.width as f64 / self.bounds.size.width, self.height as f64 / self.bounds.size.height)
    }

    /// Desktop point of display pixel (x, y).
    pub fn desktop_point(&self, x: f64, y: f64) -> CGPoint {
        let (sx, sy) = self.scale();
        CGPoint::new(self.bounds.origin.x + x / sx, self.bounds.origin.y + y / sy)
    }

    /// Display pixel of a desktop point, clamped to the display.
    pub fn display_pixel(&self, p: CGPoint) -> (u32, u32) {
        let (sx, sy) = self.scale();
        let x = ((p.x - self.bounds.origin.x) * sx).clamp(0.0, (self.width - 1) as f64);
        let y = ((p.y - self.bounds.origin.y) * sy).clamp(0.0, (self.height - 1) as f64);
        (x as u32, y as u32)
    }

    /// `p` moved inside the display, to the nearest point on it.
    pub fn clamp(&self, p: CGPoint) -> CGPoint {
        let o = self.bounds.origin;
        let s = self.bounds.size;
        CGPoint::new(p.x.clamp(o.x, o.x + s.width - 1.0), p.y.clamp(o.y, o.y + s.height - 1.0))
    }

    /// `p` (desktop points) is on the display.
    pub fn contains(&self, p: CGPoint) -> bool {
        let (o, s) = (self.bounds.origin, self.bounds.size);
        (o.x..o.x + s.width).contains(&p.x) && (o.y..o.y + s.height).contains(&p.y)
    }

    pub fn info(&self) -> DisplayInfo {
        let name = if CGDisplayIsBuiltin(self.id) { "Built-in display".to_string() } else { format!("Display {}", self.id) };
        DisplayInfo {
            id: format!("{ID_PREFIX}{}", self.id),
            name,
            width: self.width,
            height: self.height,
            refresh_hz: self.refresh_hz,
            x: self.bounds.origin.x as i32,
            y: self.bounds.origin.y as i32,
            primary: self.id == CGMainDisplayID(),
            kind: DisplayKind::Real,
            mode: None,
        }
    }
}

pub(crate) fn parse_id(id: &str) -> Result<CGDirectDisplayID> {
    id.strip_prefix(ID_PREFIX).and_then(|n| n.parse().ok()).with_context(|| format!("unknown display {id}"))
}

/// The displays macOS shows the desktop on.
pub(crate) fn active_ids() -> Result<Vec<CGDirectDisplayID>> {
    let mut ids = [0 as CGDirectDisplayID; 32];
    let mut count = 0u32;
    // SAFETY: the buffer holds the 32 ids asked for; count is written back.
    let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        bail!("CGGetActiveDisplayList failed ({})", err.0);
    }
    Ok(ids[..count as usize].to_vec())
}

pub(crate) fn list() -> Result<Vec<DisplayInfo>> {
    let mut out = Vec::new();
    for id in active_ids()? {
        match Geometry::of(id) {
            Ok(g) => out.push(g.info()),
            Err(e) => eprintln!("display {id}: {e:#}"),
        }
    }
    Ok(out)
}
