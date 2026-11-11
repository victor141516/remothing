//! Cursor shape through XFixes and position through QueryPointer (shared by
//! every X11 capture path).

use anyhow::Result;
use remothing_core::platform::CursorShape;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, Window};

/// Current cursor image, converted from premultiplied ARGB to straight RGBA.
pub fn cursor_image(conn: &impl Connection) -> Result<CursorShape> {
    let img = conn.xfixes_get_cursor_image()?.reply()?;
    let mut rgba = Vec::with_capacity(img.cursor_image.len() * 4);
    for &p in &img.cursor_image {
        let a = p >> 24;
        let un = |c: u32| if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 };
        rgba.extend_from_slice(&[un((p >> 16) & 0xff), un((p >> 8) & 0xff), un(p & 0xff), a as u8]);
    }
    Ok(CursorShape {
        width: img.width as u32,
        height: img.height as u32,
        hot_x: img.xhot as u32,
        hot_y: img.yhot as u32,
        rgba,
    })
}

/// The pointer's position in the `size` area at `origin` of the X screen
/// (clamped to it), if it differs from `last`, which is updated. The hot
/// spot is where X reports the pointer.
pub fn pointer_position(
    conn: &impl Connection,
    root: Window,
    origin: (i32, i32),
    size: (u32, u32),
    last: &mut Option<(u32, u32)>,
) -> Result<Option<(u32, u32)>> {
    let p = conn.query_pointer(root)?.reply()?;
    let clamp = |v: i32, o: i32, max: u32| (v - o).clamp(0, max.saturating_sub(1) as i32) as u32;
    let pos = (clamp(p.root_x as i32, origin.0, size.0), clamp(p.root_y as i32, origin.1, size.1));
    if *last == Some(pos) {
        return Ok(None);
    }
    *last = Some(pos);
    Ok(Some(pos))
}
