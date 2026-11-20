//! Input injection through XTEST, relative to one display's area of the X screen.

use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use remothing_core::platform::{Button, InputProvider};
use remothing_core::proto::WHEEL_NOTCH;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

pub struct XTestInput {
    conn: RustConnection,
    root: Window,
    display: String,
    /// Position of the captured area on the X screen (RandR output origin).
    origin: (i32, i32),
    size: (u32, u32),
    spare: Mutex<SpareKeys>,
    /// Wheel units not sent yet (less than a notch each way): the X core
    /// protocol and XTEST only have wheel buttons, one click per notch, so
    /// trackpad and touch scrolling arrive here in whole notches.
    wheel_rest: Mutex<(i32, i32)>,
}

/// Keycodes the layout leaves without symbols, lent in turn to typed
/// characters that no key of the layout gives (`text`). XTEST presses keys,
/// not characters.
#[derive(Default)]
struct SpareKeys {
    codes: Vec<u8>,
    next: usize,
}

/// evdev keycodes map to X keycodes with this offset under the evdev XKB rules.
const EVDEV_OFFSET: u16 = 8;
const EV_LEFTSHIFT: u16 = 42;
/// A program reads the key's new symbol when it handles the key press, which
/// can be after the next characters were typed: with several keys in turn,
/// the symbol it finds is still the one that was pressed.
const SPARE_KEYS: usize = 8;
const XK_TAB: u32 = 0xff09;
const XK_RETURN: u32 = 0xff0d;

/// The X keysym of a character.
fn keysym(c: char) -> Option<u32> {
    Some(match c {
        '\n' => XK_RETURN,
        '\t' => XK_TAB,
        // Latin-1 keysyms are the code points; the rest of Unicode has its own range.
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        c if c.is_control() => return None,
        c => 0x0100_0000 | c as u32,
    })
}

impl XTestInput {
    /// `origin` and `size`: the captured area on the X screen.
    pub fn new(display: &str, origin: (i32, i32), size: (u32, u32)) -> Result<Self> {
        let (conn, screen) = x11rb::connect(Some(display)).context("connect to X for input")?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?.reply()?;
        let (spare, wheel_rest) = (Mutex::default(), Mutex::default());
        Ok(Self { conn, root, display: display.to_string(), origin, size, spare, wheel_rest })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn click(&self, button: u8, steps: u32) -> Result<()> {
        for _ in 0..steps {
            self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
            self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
        }
        Ok(())
    }
}

impl Drop for XTestInput {
    fn drop(&mut self) {
        // The lent keycodes go back to having no symbol: the next input
        // connection looks for free ones again.
        if let Ok(spare) = self.spare.get_mut() {
            for &code in &spare.codes {
                let _ = self.conn.change_keyboard_mapping(1, code, 1, &[0]);
            }
            let _ = self.conn.flush();
        }
    }
}

impl InputProvider for XTestInput {
    fn motion(&self, x: u32, y: u32) -> Result<()> {
        let (ox, oy) = self.origin;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, (ox + x as i32) as i16, (oy + y as i32) as i16)
    }

    fn motion_relative(&self, dx: i32, dy: i32) -> Result<()> {
        // XTEST motion with detail 1 is relative to where the pointer is; the
        // X server keeps it on the screen, across outputs as a real mouse goes.
        let clamp = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 1, clamp(dx), clamp(dy))
    }

    fn pointer(&self) -> Result<Option<(u32, u32)>> {
        let p = self.conn.query_pointer(self.root)?.reply()?;
        let (w, h) = (self.size.0 as i32, self.size.1 as i32);
        let x = (p.root_x as i32 - self.origin.0).clamp(0, w - 1) as u32;
        let y = (p.root_y as i32 - self.origin.1).clamp(0, h - 1) as u32;
        Ok(Some((x, y)))
    }

    fn button(&self, button: Button, down: bool) -> Result<()> {
        let detail = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
            Button::Back => 8,
            Button::Forward => 9,
        };
        let kind = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(kind, detail, 0, 0)
    }

    fn wheel(&self, dx: i32, dy: i32) -> Result<()> {
        let (nx, ny) = {
            let mut rest = self.wheel_rest.lock().unwrap();
            rest.0 += dx;
            rest.1 += dy;
            let notches = (rest.0 / WHEEL_NOTCH, rest.1 / WHEEL_NOTCH);
            rest.0 -= notches.0 * WHEEL_NOTCH;
            rest.1 -= notches.1 * WHEEL_NOTCH;
            notches
        };
        // X wheel buttons: 4/5 vertical, 6/7 horizontal.
        self.click(if ny < 0 { 4 } else { 5 }, ny.unsigned_abs())?;
        self.click(if nx < 0 { 6 } else { 7 }, nx.unsigned_abs())
    }

    fn key(&self, evdev: u16, down: bool) -> Result<()> {
        let Ok(code) = u8::try_from(evdev + EVDEV_OFFSET) else { return Ok(()) };
        let kind = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(kind, code, 0, 0)
    }

    fn text(&self, text: &str) -> Result<()> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        // Read every time: the layout can change under us (setxkbmap).
        let map = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = map.keysyms_per_keycode as usize;
        let mut syms = map.keysyms;
        let mut spare = self.spare.lock().unwrap();
        if spare.codes.is_empty() {
            spare.codes = (min..=max)
                .rev()
                .filter(|&code| syms[(code - min) as usize * per..][..per].iter().all(|&s| s == 0))
                .take(SPARE_KEYS)
                .collect();
        }
        let shift = (EV_LEFTSHIFT + EVDEV_OFFSET) as u8;
        for c in text.chars() {
            let Some(sym) = keysym(c) else { continue };
            // The first two symbols of a key: alone and with Shift.
            let key_with = |column: usize| {
                syms.chunks(per).position(|row| row.get(column) == Some(&sym)).map(|i| min + i as u8)
            };
            let (code, shifted) = match (key_with(0), key_with(1)) {
                (Some(code), _) => (code, false),
                (None, Some(code)) => (code, true),
                (None, None) => {
                    if spare.codes.is_empty() {
                        continue;
                    }
                    let code = spare.codes[spare.next];
                    spare.next = (spare.next + 1) % spare.codes.len();
                    self.conn.change_keyboard_mapping(1, code, 1, &[sym])?;
                    let row = &mut syms[(code - min) as usize * per..][..per];
                    row.fill(0);
                    row[0] = sym;
                    (code, false)
                }
            };
            if shifted {
                self.fake(xproto::KEY_PRESS_EVENT, shift, 0, 0)?;
            }
            self.fake(xproto::KEY_PRESS_EVENT, code, 0, 0)?;
            self.fake(xproto::KEY_RELEASE_EVENT, code, 0, 0)?;
            if shifted {
                self.fake(xproto::KEY_RELEASE_EVENT, shift, 0, 0)?;
            }
        }
        Ok(())
    }

    fn set_keyboard_layout(&self, layout: &str) -> Result<()> {
        if layout.is_empty() || !layout.chars().all(|c| c.is_ascii_alphanumeric() || "_-,()".contains(c)) {
            bail!("invalid layout {layout:?}");
        }
        let status = std::process::Command::new("setxkbmap")
            .args(["-display", &self.display, "-layout", layout])
            .status()
            .context("run setxkbmap")?;
        if !status.success() {
            bail!("setxkbmap -layout {layout} failed: {status}");
        }
        Ok(())
    }
}
