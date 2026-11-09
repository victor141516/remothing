//! Input injection through XTEST.

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

pub struct Injector {
    conn: RustConnection,
    root: Window,
}

/// evdev keycodes map to X keycodes with this offset under the evdev XKB rules.
const EVDEV_OFFSET: u16 = 8;

impl Injector {
    pub fn new(display: &str) -> Result<Self> {
        let (conn, screen) = x11rb::connect(Some(display)).context("connect to X for input")?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?.reply()?;
        Ok(Self { conn, root })
    }

    pub fn motion(&self, x: u16, y: u16) -> Result<()> {
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x as i16, y as i16)
    }

    /// X button numbers: 1 left, 2 middle, 3 right, 4/5 wheel up/down, 6/7 wheel left/right, 8/9 back/forward.
    pub fn button(&self, button: u8, down: bool) -> Result<()> {
        let kind = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(kind, button, 0, 0)
    }

    pub fn key(&self, evdev: u16, down: bool) -> Result<()> {
        let Ok(code) = u8::try_from(evdev + EVDEV_OFFSET) else { return Ok(()) };
        let kind = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(kind, code, 0, 0)
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
        self.conn.flush()?;
        Ok(())
    }
}
