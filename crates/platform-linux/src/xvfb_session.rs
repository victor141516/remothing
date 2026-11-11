//! Capture of an Xvfb display: XDamage tells which areas changed, pixels come
//! straight from the Xvfb shared-memory framebuffer, cursor from XFixes.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use remothing_core::geom::Rect;
use remothing_core::platform::{CaptureSession, CursorProvider, CursorState, FrameInfo};
use remothing_core::proto::now_us;
use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::cursor::{cursor_image, pointer_position};
use crate::xfb::XvfbFramebuffer;

pub struct XvfbSession {
    conn: RustConnection,
    root: x11rb::protocol::xproto::Window,
    /// Where the pointer was last reported (`cursor_position`).
    pointer: Option<(u32, u32)>,
    damage: damage::Damage,
    region: xfixes::Region,
    fb: XvfbFramebuffer,
    first: bool,
    cursor_pending: bool,
    /// Keeps the Xvfb process alive while captured.
    _owner: Arc<dyn Send + Sync>,
}

impl XvfbSession {
    pub fn open(display: &str, shmid: i32, owner: Arc<dyn Send + Sync>) -> Result<Self> {
        let fb = XvfbFramebuffer::attach(shmid)?;
        let (conn, screen) = x11rb::connect(Some(display)).context("connect to Xvfb for capture")?;
        let root = conn.setup().roots[screen].root;
        conn.damage_query_version(1, 1)?.reply()?;
        conn.xfixes_query_version(5, 0)?.reply()?;
        let damage = conn.generate_id()?;
        conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)?;
        let region = conn.generate_id()?;
        conn.xfixes_create_region(region, &[])?;
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
        conn.flush()?;
        Ok(Self { conn, root, pointer: None, damage, region, fb, first: true, cursor_pending: true, _owner: owner })
    }
}

impl CursorProvider for XvfbSession {
    fn cursor_change(&mut self) -> Result<Option<CursorState>> {
        if !std::mem::take(&mut self.cursor_pending) {
            return Ok(None);
        }
        Ok(Some(CursorState::Visible(Arc::new(cursor_image(&self.conn)?))))
    }

    fn cursor_position(&mut self) -> Result<Option<(u32, u32)>> {
        let size = (self.fb.width, self.fb.height);
        pointer_position(&self.conn, self.root, (0, 0), size, &mut self.pointer)
    }
}

impl CaptureSession for XvfbSession {
    fn size(&self) -> (u32, u32) {
        (self.fb.width, self.fb.height)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<FrameInfo>> {
        if std::mem::take(&mut self.first) {
            let full = Rect::new(0, 0, self.fb.width, self.fb.height);
            return Ok(Some(FrameInfo { capture_us: now_us(), dirty: vec![full], moved: Vec::new() }));
        }
        let mut ev = self.conn.poll_for_event()?;
        if ev.is_none() {
            let mut pfd = libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd.
            if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
                return Ok(None);
            }
            ev = self.conn.poll_for_event()?;
        }
        let capture_us = now_us();
        let mut damaged = false;
        while let Some(e) = ev {
            match e {
                Event::DamageNotify(_) => damaged = true,
                Event::XfixesCursorNotify(_) => self.cursor_pending = true,
                _ => {}
            }
            ev = self.conn.poll_for_event()?;
        }
        if !damaged {
            return Ok(None);
        }
        // Subtract + fetch is a round trip, so every drawing request that
        // produced this damage has been executed when the reply arrives.
        self.conn.damage_subtract(self.damage, x11rb::NONE, self.region)?;
        let reply = self.conn.xfixes_fetch_region(self.region)?.reply()?;
        let dirty = reply
            .rectangles
            .iter()
            .map(|r| Rect::new(r.x.max(0) as u32, r.y.max(0) as u32, r.width as u32, r.height as u32))
            .collect();
        Ok(Some(FrameInfo { capture_us, dirty, moved: Vec::new() }))
    }

    fn read(&mut self, rects: &[Rect], dst: &mut [u8], dst_stride: usize) -> Result<()> {
        for r in rects.iter().filter_map(|r| r.clip(self.fb.width, self.fb.height)) {
            let (xb, wb) = (r.x as usize * 4, r.w as usize * 4);
            for y in r.y..r.y + r.h {
                let o = y as usize * dst_stride + xb;
                dst[o..o + wb].copy_from_slice(&self.fb.row(y)[xb..xb + wb]);
            }
        }
        Ok(())
    }

    fn mapped(&self) -> Option<(&[u8], usize)> {
        Some(self.fb.pixels())
    }
}
