//! Real X11 displays (an existing X server, e.g. `:0`): one display per active
//! RandR output, captured with XShmGetImage on XDamage-reported areas.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use remothing_core::geom::Rect;
use remothing_core::platform::{CaptureSession, CursorProvider, CursorState, DisplayInfo, DisplayKind, FrameInfo};
use remothing_core::proto::now_us;
use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::cursor::{cursor_image, pointer_position};

/// Lists the active outputs of `display` (whole screen when RandR has none).
pub fn list(display: &str) -> Result<Vec<DisplayInfo>> {
    let (conn, screen) = x11rb::connect(Some(display)).with_context(|| format!("connect to {display}"))?;
    let s = &conn.setup().roots[screen];
    let whole = || DisplayInfo {
        id: format!("x11:{display}:screen"),
        name: format!("X11 {display}"),
        width: s.width_in_pixels as u32,
        height: s.height_in_pixels as u32,
        refresh_hz: 0.0,
        x: 0,
        y: 0,
        primary: true,
        kind: DisplayKind::Real,
    };
    if conn.randr_query_version(1, 3).ok().and_then(|c| c.reply().ok()).is_none() {
        return Ok(vec![whole()]);
    }
    let res = conn.randr_get_screen_resources_current(s.root)?.reply()?;
    let primary = conn.randr_get_output_primary(s.root)?.reply()?.output;
    let mut out = Vec::new();
    for &output in &res.outputs {
        let o = conn.randr_get_output_info(output, res.config_timestamp)?.reply()?;
        if o.crtc == 0 || o.connection != randr::Connection::CONNECTED {
            continue;
        }
        let c = conn.randr_get_crtc_info(o.crtc, res.config_timestamp)?.reply()?;
        let refresh_hz = res
            .modes
            .iter()
            .find(|m| m.id == c.mode)
            .filter(|m| m.htotal > 0 && m.vtotal > 0)
            .map_or(0.0, |m| m.dot_clock as f64 / (m.htotal as f64 * m.vtotal as f64));
        let name = String::from_utf8_lossy(&o.name).into_owned();
        out.push(DisplayInfo {
            id: format!("x11:{display}:{name}"),
            name: format!("{name} ({display})"),
            width: c.width as u32,
            height: c.height as u32,
            refresh_hz,
            x: c.x as i32,
            y: c.y as i32,
            primary: output == primary,
            kind: DisplayKind::Real,
        });
    }
    if out.is_empty() {
        out.push(whole());
    }
    Ok(out)
}

/// Shared-memory segment the X server writes XShmGetImage results into.
struct ShmSegment {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for ShmSegment {}

impl ShmSegment {
    fn new(conn: &RustConnection, len: usize) -> Result<Self> {
        // SAFETY: plain SysV shm; results checked.
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if id < 0 {
                bail!("shmget: {}", std::io::Error::last_os_error());
            }
            let ptr = libc::shmat(id, std::ptr::null(), 0);
            if ptr as isize == -1 {
                bail!("shmat: {}", std::io::Error::last_os_error());
            }
            let seg = conn.generate_id()?;
            conn.shm_attach(seg, id as u32, false)?;
            // Round trip: the server has attached the segment before we mark it for removal.
            conn.get_input_focus()?.reply()?;
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            Ok(Self { seg, ptr: ptr as *mut u8, len })
        }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        unsafe { libc::shmdt(self.ptr as *const libc::c_void) };
    }
}

pub struct X11Session {
    conn: RustConnection,
    root: xproto::Window,
    damage: damage::Damage,
    region: xfixes::Region,
    area: Rect,
    origin: (i32, i32),
    shm: ShmSegment,
    first: bool,
    cursor_pending: bool,
    /// Where the pointer was last reported (`cursor_position`).
    pointer: Option<(u32, u32)>,
}

impl X11Session {
    pub fn open(display: &str, info: &DisplayInfo) -> Result<Self> {
        let (conn, screen) = x11rb::connect(Some(display)).with_context(|| format!("connect to {display}"))?;
        let s = &conn.setup().roots[screen];
        if s.root_depth != 24 && s.root_depth != 32 {
            bail!("unsupported X depth {}", s.root_depth);
        }
        let root = s.root;
        conn.damage_query_version(1, 1)?.reply()?;
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.shm_query_version()?.reply().context("MIT-SHM unavailable")?;
        let damage = conn.generate_id()?;
        conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)?;
        let region = conn.generate_id()?;
        conn.xfixes_create_region(region, &[])?;
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
        let shm = ShmSegment::new(&conn, (info.width * info.height * 4) as usize)?;
        conn.flush()?;
        Ok(Self {
            conn,
            root,
            damage,
            region,
            area: Rect::new(0, 0, info.width, info.height),
            origin: (info.x, info.y),
            shm,
            first: true,
            cursor_pending: true,
            pointer: None,
        })
    }
}

impl CursorProvider for X11Session {
    fn cursor_change(&mut self) -> Result<Option<CursorState>> {
        if !std::mem::take(&mut self.cursor_pending) {
            return Ok(None);
        }
        Ok(Some(CursorState::Visible(Arc::new(cursor_image(&self.conn)?))))
    }

    fn cursor_position(&mut self) -> Result<Option<(u32, u32)>> {
        let size = (self.area.w, self.area.h);
        pointer_position(&self.conn, self.root, self.origin, size, &mut self.pointer)
    }
}

impl CaptureSession for X11Session {
    fn size(&self) -> (u32, u32) {
        (self.area.w, self.area.h)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<FrameInfo>> {
        if std::mem::take(&mut self.first) {
            return Ok(Some(FrameInfo { capture_us: now_us(), dirty: vec![self.area], moved: Vec::new() }));
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
        self.conn.damage_subtract(self.damage, x11rb::NONE, self.region)?;
        let reply = self.conn.xfixes_fetch_region(self.region)?.reply()?;
        let (ox, oy) = self.origin;
        // Screen coordinates -> this output's coordinates, dropping other outputs' areas.
        let dirty: Vec<Rect> = reply
            .rectangles
            .iter()
            .filter_map(|r| {
                let x0 = (r.x as i32 - ox).max(0);
                let y0 = (r.y as i32 - oy).max(0);
                let x1 = (r.x as i32 + r.width as i32 - ox).min(self.area.w as i32);
                let y1 = (r.y as i32 + r.height as i32 - oy).min(self.area.h as i32);
                (x0 < x1 && y0 < y1).then(|| Rect::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
            })
            .collect();
        if dirty.is_empty() {
            return Ok(None);
        }
        Ok(Some(FrameInfo { capture_us, dirty, moved: Vec::new() }))
    }

    fn read(&mut self, rects: &[Rect], dst: &mut [u8], dst_stride: usize) -> Result<()> {
        let (ox, oy) = self.origin;
        for r in rects.iter().filter_map(|r| r.clip(self.area.w, self.area.h)) {
            self.conn
                .shm_get_image(
                    self.root,
                    (ox + r.x as i32) as i16,
                    (oy + r.y as i32) as i16,
                    r.w as u16,
                    r.h as u16,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    self.shm.seg,
                    0,
                )?
                .reply()?;
            let wb = r.w as usize * 4;
            debug_assert!(wb * r.h as usize <= self.shm.len);
            // SAFETY: the reply arrived, so the server finished writing r.w*r.h pixels.
            let img = unsafe { std::slice::from_raw_parts(self.shm.ptr, wb * r.h as usize) };
            for row in 0..r.h as usize {
                let o = (r.y as usize + row) * dst_stride + r.x as usize * 4;
                dst[o..o + wb].copy_from_slice(&img[row * wb..(row + 1) * wb]);
            }
        }
        Ok(())
    }
}
