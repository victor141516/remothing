//! Input through Quartz events posted at the HID level, as a real mouse and
//! keyboard would. macOS drops them without a word unless the process has
//! the Accessibility permission (`permissions.rs` asks for it).

use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};
use remothing_core::platform::{Button, InputProvider};
use remothing_core::proto::WHEEL_NOTCH;

use crate::display::Geometry;
use crate::keymap::{self, Modifier};

/// Pixels per wheel notch for fractions of a notch (trackpads, touch).
const PIXELS_PER_NOTCH: i32 = 40;
/// Farther than this (points) from the last press, a press starts a new click count.
const CLICK_SLOP: f64 = 4.0;

struct State {
    /// Where the pointer was put last, desktop points.
    position: Option<CGPoint>,
    /// Buttons held: they turn moves into drags, which apps expect.
    held: Vec<Button>,
    flags: CGEventFlags,
    /// Last press: button, time, place and click count, for double clicks.
    last_press: Option<(Button, Instant, CGPoint, i64)>,
    /// Click count of the presses held now, repeated on their release.
    clicks: Vec<(Button, i64)>,
    /// Wheel pixels not sent yet (fractions of a pixel), vertical and horizontal.
    wheel_rest: (i32, i32),
}

pub(crate) struct QuartzInput {
    geometry: Geometry,
    state: Mutex<State>,
}

impl QuartzInput {
    pub fn new(geometry: Geometry) -> Self {
        let state = State {
            position: None,
            held: Vec::new(),
            flags: CGEventFlags::empty(),
            last_press: None,
            clicks: Vec::new(),
            wheel_rest: (0, 0),
        };
        Self { geometry, state: Mutex::new(state) }
    }

    fn source() -> Option<CFRetained<CGEventSource>> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
    }

    fn post(event: Option<CFRetained<CGEvent>>, flags: CGEventFlags) -> Result<()> {
        let event = event.context("Quartz refused to create the event")?;
        CGEvent::set_flags(Some(&event), flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    /// Where the pointer is now, desktop points.
    fn pointer() -> Option<CGPoint> {
        CGEvent::new(None).map(|e| CGEvent::location(Some(&e)))
    }

    fn move_to(&self, s: &mut State, to: CGPoint, delta: Option<(i32, i32)>) -> Result<()> {
        let (kind, button) = match s.held.last() {
            Some(Button::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            Some(Button::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            Some(_) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        };
        let event = CGEvent::new_mouse_event(Self::source().as_deref(), kind, to, button);
        if let (Some(e), Some((dx, dy))) = (&event, delta) {
            // Games read the movement from these, not from the position.
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventDeltaX, dx as i64);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventDeltaY, dy as i64);
        }
        s.position = Some(to);
        Self::post(event, s.flags)
    }
}

fn button_types(button: Button) -> (CGEventType, CGEventType, CGMouseButton, i64) {
    match button {
        Button::Left => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
        Button::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGMouseButton::Right, 1),
        Button::Middle => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, 2),
        Button::Back => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, 3),
        Button::Forward => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, 4),
    }
}

impl InputProvider for QuartzInput {
    fn motion(&self, x: u32, y: u32) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        let to = self.geometry.desktop_point(x as f64, y as f64);
        self.move_to(&mut s, to, None)
    }

    fn motion_relative(&self, dx: i32, dy: i32) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        let from = Self::pointer().or(s.position).unwrap_or(self.geometry.bounds.origin);
        let (sx, sy) = self.geometry.scale();
        // Counts are display pixels of movement; the pointer moves in points.
        let to = self.geometry.clamp(CGPoint::new(from.x + dx as f64 / sx, from.y + dy as f64 / sy));
        self.move_to(&mut s, to, Some((dx, dy)))
    }

    fn button(&self, button: Button, down: bool) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        let at = s.position.or_else(Self::pointer).unwrap_or(self.geometry.bounds.origin);
        let (press, release, cg_button, number) = button_types(button);
        let count = if down {
            let interval = objc2_app_kit::NSEvent::doubleClickInterval();
            let count = match s.last_press {
                Some((b, t, p, n))
                    if b == button
                        && t.elapsed().as_secs_f64() <= interval
                        && (p.x - at.x).abs() <= CLICK_SLOP
                        && (p.y - at.y).abs() <= CLICK_SLOP =>
                {
                    n + 1
                }
                _ => 1,
            };
            s.last_press = Some((button, Instant::now(), at, count));
            s.clicks.retain(|(b, _)| *b != button);
            s.clicks.push((button, count));
            s.held.retain(|b| *b != button);
            s.held.push(button);
            count
        } else {
            s.held.retain(|b| *b != button);
            let i = s.clicks.iter().position(|(b, _)| *b == button);
            i.map_or(1, |i| s.clicks.remove(i).1)
        };
        let event = CGEvent::new_mouse_event(Self::source().as_deref(), if down { press } else { release }, at, cg_button);
        if let Some(e) = &event {
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventClickState, count);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventButtonNumber, number);
        }
        Self::post(event, s.flags)
    }

    fn wheel(&self, dx: i32, dy: i32) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        // Whole notches go as a wheel's lines, so macOS accelerates them as
        // it does a real wheel; fractions as pixels, as a trackpad's.
        let notches = dx % WHEEL_NOTCH == 0 && dy % WHEEL_NOTCH == 0;
        let (units, vertical, horizontal) = if notches {
            (CGScrollEventUnit::Line, -dy / WHEEL_NOTCH, -dx / WHEEL_NOTCH)
        } else {
            let v = s.wheel_rest.0 - dy * PIXELS_PER_NOTCH;
            let h = s.wheel_rest.1 - dx * PIXELS_PER_NOTCH;
            s.wheel_rest = (v % WHEEL_NOTCH, h % WHEEL_NOTCH);
            (CGScrollEventUnit::Pixel, v / WHEEL_NOTCH, h / WHEEL_NOTCH)
        };
        if vertical == 0 && horizontal == 0 {
            return Ok(());
        }
        let event = CGEvent::new_scroll_wheel_event2(Self::source().as_deref(), units, 2, vertical, horizontal, 0);
        if let (Some(e), false) = (&event, notches) {
            CGEvent::set_integer_value_field(Some(e), CGEventField::ScrollWheelEventIsContinuous, 1);
        }
        Self::post(event, s.flags)
    }

    fn key(&self, evdev: u16, down: bool) -> Result<()> {
        // Keys this Mac has no code for are dropped, as a keyboard without them would.
        let Some(code) = keymap::lookup(evdev) else { return Ok(()) };
        let mut s = self.state.lock().unwrap();
        let event = CGEvent::new_keyboard_event(Self::source().as_deref(), code, down);
        match keymap::modifier(evdev) {
            Some(m) => {
                match m {
                    Modifier::Held(f) if down => s.flags.insert(f),
                    Modifier::Held(f) => s.flags.remove(f),
                    Modifier::Lock if down => s.flags.toggle(CGEventFlags::MaskAlphaShift),
                    Modifier::Lock => return Ok(()),
                }
                // A modifier is a change of flags, not a key press, for macOS.
                if let Some(e) = &event {
                    CGEvent::set_type(Some(e), CGEventType::FlagsChanged);
                }
                Self::post(event, s.flags)
            }
            None => Self::post(event, s.flags),
        }
    }

    fn text(&self, text: &str) -> Result<()> {
        let _s = self.state.lock().unwrap();
        let mut units = [0u16; 2];
        for c in text.chars() {
            if c == '\n' {
                for down in [true, false] {
                    Self::post(CGEvent::new_keyboard_event(Self::source().as_deref(), keymap::KVK_RETURN, down), CGEventFlags::empty())?;
                }
                continue;
            }
            let chars = c.encode_utf16(&mut units);
            for down in [true, false] {
                // Key 0 carrying the character: macOS types it whatever the layout.
                let event = CGEvent::new_keyboard_event(Self::source().as_deref(), 0, down);
                if let Some(e) = &event {
                    // SAFETY: `chars` holds `len` UTF-16 units.
                    unsafe { CGEvent::keyboard_set_unicode_string(Some(e), chars.len() as _, chars.as_ptr()) };
                }
                Self::post(event, CGEventFlags::empty())?;
            }
        }
        Ok(())
    }
}
