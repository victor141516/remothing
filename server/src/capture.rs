//! Damage-driven capture: XDamage tells us which areas changed, pixels are
//! read straight from the Xvfb shared-memory framebuffer.

use std::sync::Arc;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::Event;

use crate::proto;
use crate::shared::{Rect, Shared};
use crate::xfb::XvfbFramebuffer;

pub fn run(display: &str, fb: XvfbFramebuffer, shared: Arc<Shared>) -> Result<()> {
    let (conn, screen) = x11rb::connect(Some(display)).context("connect to X for capture")?;
    let root = conn.setup().roots[screen].root;
    conn.damage_query_version(1, 1)?.reply()?;
    conn.xfixes_query_version(5, 0)?.reply()?;

    let damage = conn.generate_id()?;
    conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)?;
    let region = conn.generate_id()?;
    conn.xfixes_create_region(region, &[])?;
    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    conn.flush()?;

    send_cursor(&conn, &shared)?;
    let full = [Rect { x: 0, y: 0, w: fb.width, h: fb.height }];
    shared.ingest(&fb, &full, proto::now_us());

    let mut rects = Vec::new();
    loop {
        let first = conn.wait_for_event()?;
        let capture_us = proto::now_us();
        let (mut damaged, mut cursor) = (false, false);
        let mut ev = Some(first);
        while let Some(e) = ev {
            match e {
                Event::DamageNotify(_) => damaged = true,
                Event::XfixesCursorNotify(_) => cursor = true,
                _ => {}
            }
            ev = conn.poll_for_event()?;
        }
        if damaged {
            // Subtract + fetch is a round trip, so every drawing request that
            // produced this damage has been executed when the reply arrives.
            conn.damage_subtract(damage, x11rb::NONE, region)?;
            let reply = conn.xfixes_fetch_region(region)?.reply()?;
            rects.clear();
            rects.extend(reply.rectangles.iter().map(|r| Rect {
                x: r.x.max(0) as u32,
                y: r.y.max(0) as u32,
                w: r.width as u32,
                h: r.height as u32,
            }));
            shared.ingest(&fb, &rects, capture_us);
        }
        if cursor {
            send_cursor(&conn, &shared)?;
        }
    }
}

fn send_cursor(conn: &impl Connection, shared: &Shared) -> Result<()> {
    let img = conn.xfixes_get_cursor_image()?.reply()?;
    let mut rgba = Vec::with_capacity(img.cursor_image.len() * 4);
    for &p in &img.cursor_image {
        let a = p >> 24;
        let un = |c: u32| if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 };
        rgba.extend_from_slice(&[un((p >> 16) & 0xff), un((p >> 8) & 0xff), un(p & 0xff), a as u8]);
    }
    shared.set_cursor(proto::cursor(img.width, img.height, img.xhot, img.yhot, &rgba));
    Ok(())
}
