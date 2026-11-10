//! Cursor shape through XFixes (shared by every X11 capture path).

use anyhow::Result;
use remothing_core::platform::CursorShape;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;

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
