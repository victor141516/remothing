//! The pointer: its shape from NSCursor's current system cursor, rendered at
//! the display's pixel scale, and its position from Quartz.
//!
//! Apple marks `currentSystemCursor` as going away ("will always be nil in a
//! future version of macOS"). Where it is nil the cursor is drawn into the
//! picture by ScreenCaptureKit and the clients are told to show none.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2_app_kit::NSCursor;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGColorSpace, CGContext, CGEvent, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::NSRect;
use remothing_core::platform::{CursorShape, CursorState};

use crate::display::Geometry;

extern "C-unwind" {
    // Not in objc2-core-graphics (only its callback-taking variants are).
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: Option<&CGColorSpace>,
        bitmap_info: u32,
    ) -> Option<NonNull<CGContext>>;
}

/// How often the shape is looked at: rendering it costs a little, and a
/// change seen a frame late is not noticed.
const SHAPE_INTERVAL: Duration = Duration::from_millis(30);

pub(crate) struct Cursor {
    geometry: Geometry,
    separate: bool,
    told_hidden: bool,
    shape: Option<CursorShape>,
    checked: Option<Instant>,
    position: Option<(u32, u32)>,
}

impl Cursor {
    pub fn new(geometry: Geometry) -> Self {
        #[allow(deprecated)]
        let separate = autoreleasepool(|_| NSCursor::currentSystemCursor().is_some());
        if !separate {
            eprintln!("cursor: macOS does not tell the cursor's shape; it is drawn into the picture");
        }
        Self { geometry, separate, told_hidden: false, shape: None, checked: None, position: None }
    }

    /// The cursor is sent apart from the picture.
    pub fn separate(&self) -> bool {
        self.separate
    }

    pub fn change(&mut self) -> Option<CursorState> {
        if !self.separate {
            // Once: the picture carries it.
            return (!std::mem::replace(&mut self.told_hidden, true)).then_some(CursorState::Hidden);
        }
        if self.checked.is_some_and(|t| t.elapsed() < SHAPE_INTERVAL) {
            return None;
        }
        self.checked = Some(Instant::now());
        // On another display the pointer shows nothing here, as DXGI tells
        // per output on Windows: a client following the cursor from another
        // window draws it only where it is.
        let here = CGEvent::new(None).is_some_and(|e| self.geometry.contains(CGEvent::location(Some(&e))));
        if !here {
            return self.shape.take().map(|_| CursorState::Hidden);
        }
        // Whether a program hid the cursor is not public on macOS any more
        // (CGCursorIsVisible is "no longer supported"): only the shape is sent.
        let shape = autoreleasepool(|_| self.render())?;
        if self.shape.as_ref() == Some(&shape) {
            return None;
        }
        self.shape = Some(shape.clone());
        Some(CursorState::Visible(Arc::new(shape)))
    }

    pub fn position(&mut self) -> Option<(u32, u32)> {
        let event = CGEvent::new(None)?;
        let pos = self.geometry.display_pixel(CGEvent::location(Some(&event)));
        (self.position.replace(pos) != Some(pos)).then_some(pos)
    }

    /// The current system cursor as straight-alpha RGBA at the display's scale.
    fn render(&self) -> Option<CursorShape> {
        #[allow(deprecated)]
        let cursor = NSCursor::currentSystemCursor()?;
        let image = cursor.image();
        let hot = cursor.hotSpot();
        let size = image.size();
        let (sx, sy) = self.geometry.scale();
        let width = (size.width * sx).ceil() as usize;
        let height = (size.height * sy).ceil() as usize;
        if width == 0 || height == 0 || width > 256 || height > 256 {
            return None;
        }
        // Asked for at pixel size, so the representation for this scale is picked.
        let mut rect = NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width as f64, height as f64));
        // SAFETY: valid rect pointer; no context or hints.
        let cg: objc2::rc::Retained<CGImage> = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
        let mut rgba = vec![0u8; width * height * 4];
        // SAFETY: framework constant.
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
        let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
        // SAFETY: the buffer holds width * height RGBA pixels and outlives the context.
        let ctx = unsafe { CGBitmapContextCreate(rgba.as_mut_ptr().cast(), width, height, 8, width * 4, Some(&space), info) }?;
        // SAFETY: created just above with +1 retain count.
        let ctx = unsafe { objc2_core_foundation::CFRetained::from_raw(ctx) };
        CGContext::draw_image(Some(&ctx), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width as f64, height as f64)), Some(&cg));
        drop(ctx);
        for px in rgba.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a != 0 && a != 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        let hot_x = ((hot.x * sx) as u32).min(width as u32 - 1);
        let hot_y = ((hot.y * sy) as u32).min(height as u32 - 1);
        Some(CursorShape { width: width as u32, height: height as u32, hot_x, hot_y, rgba })
    }
}
