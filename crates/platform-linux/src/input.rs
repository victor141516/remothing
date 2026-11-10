//! Input injection through XTEST, relative to one display's area of the X screen.

use anyhow::{bail, Context, Result};
use remothing_core::platform::{Button, InputProvider};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

pub struct XTestInput {
    conn: RustConnection,
    root: Window,
    display: String,
    /// Position of the captured area on the X screen (RandR output origin).
    origin: (i32, i32),
}

/// evdev keycodes map to X keycodes with this offset under the evdev XKB rules.
const EVDEV_OFFSET: u16 = 8;

impl XTestInput {
    pub fn new(display: &str, origin: (i32, i32)) -> Result<Self> {
        let (conn, screen) = x11rb::connect(Some(display)).context("connect to X for input")?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?.reply()?;
        Ok(Self { conn, root, display: display.to_string(), origin })
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

impl InputProvider for XTestInput {
    fn motion(&self, x: u32, y: u32) -> Result<()> {
        let (ox, oy) = self.origin;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, (ox + x as i32) as i16, (oy + y as i32) as i16)
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
        // X wheel buttons: 4/5 vertical, 6/7 horizontal.
        self.click(if dy < 0 { 4 } else { 5 }, dy.unsigned_abs())?;
        self.click(if dx < 0 { 6 } else { 7 }, dx.unsigned_abs())
    }

    fn key(&self, evdev: u16, down: bool) -> Result<()> {
        let Ok(code) = u8::try_from(evdev + EVDEV_OFFSET) else { return Ok(()) };
        let kind = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(kind, code, 0, 0)
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
