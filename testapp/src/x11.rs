//! X11 backend: override-redirect window covering the root, MIT-SHM puts.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::Event;

use crate::scenario::{Canvas, Mode, Plasma, BAR_H, BAR_W};
use crate::Args;

pub fn run(args: Args) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let (w, h) = (screen.width_in_pixels as usize, screen.height_in_pixels as usize);
    let root = screen.root;
    if conn.extension_information(x11rb::protocol::shm::X11_EXTENSION_NAME)?.is_none() {
        bail!("MIT-SHM missing");
    }

    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        w as u16,
        h as u16,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &xproto::CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0xF0F0F0)
            .event_mask(xproto::EventMask::POINTER_MOTION | xproto::EventMask::EXPOSURE),
    )?;
    conn.map_window(win)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &xproto::CreateGCAux::new())?;

    let bytes = w * h * 4;
    let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, bytes, libc::IPC_CREAT | 0o600) };
    if shmid < 0 {
        bail!("shmget: {}", std::io::Error::last_os_error());
    }
    let ptr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
    if ptr as isize == -1 {
        bail!("shmat: {}", std::io::Error::last_os_error());
    }
    let seg = conn.generate_id()?;
    conn.shm_attach(seg, shmid as u32, false)?;
    conn.get_input_focus()?.reply()?;
    unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
    let px: &'static mut [u32] = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, w * h) };
    let mut c = Canvas { px, w, h };

    let put = |x: usize, y: usize, pw: usize, ph: usize| -> Result<()> {
        conn.shm_put_image(
            win,
            gc,
            w as u16,
            h as u16,
            x as u16,
            y as u16,
            pw as u16,
            ph as u16,
            x as i16,
            y as i16,
            screen.root_depth,
            xproto::ImageFormat::Z_PIXMAP.into(),
            false,
            seg,
            0,
        )?;
        // Round trip: the server has consumed the buffer before we touch it again.
        conn.get_input_focus()?.reply()?;
        Ok(())
    };

    c.text(0);
    put(0, 0, w, h)?;
    let plasma = Plasma::new();
    let interval = Duration::from_secs_f64(1.0 / args.fps);
    let mut next = Instant::now();
    let mut frame: usize = 0;
    let mut marker: u16 = 0;
    let (mut box_x, mut box_dx) = (0usize, 8isize);
    let fd = conn.stream().as_raw_fd();
    eprintln!("rt-testapp {w}x{h} at {} fps", args.fps);

    loop {
        let mut input = false;
        while let Some(ev) = conn.poll_for_event()? {
            if let Event::MotionNotify(m) = ev {
                marker = m.root_x as u16;
                input = true;
            }
        }
        let now = Instant::now();
        if now >= next {
            next = if now - next > interval { now + interval } else { next + interval };
            frame += 1;
            match args.mode {
                Mode::Idle => {
                    c.barcode(marker);
                    put(0, 0, BAR_W, BAR_H)?;
                }
                Mode::Boxed => {
                    let old = box_x;
                    let max = w - 256;
                    let nx = (box_x as isize + box_dx).clamp(0, max as isize) as usize;
                    if nx == 0 || nx == max {
                        box_dx = -box_dx;
                    }
                    box_x = nx;
                    let by = h / 2 - 128;
                    c.text(0);
                    let hue = (frame as u32 * 3) & 0xFF;
                    c.fill(box_x, by, 256, 256, 0x3060C0 | hue << 16);
                    c.barcode(marker);
                    let x0 = old.min(box_x);
                    put(x0, by, (old.max(box_x) + 256 - x0).min(w - x0), 256)?;
                    put(0, 0, BAR_W, BAR_H)?;
                }
                Mode::Full => {
                    plasma.draw(&mut c, frame);
                    c.barcode(marker);
                    put(0, 0, w, h)?;
                }
                Mode::Scroll => {
                    c.text(frame * 3);
                    c.barcode(marker);
                    put(0, 0, w, h)?;
                }
            }
            continue;
        }
        if input {
            c.barcode(marker);
            put(0, 0, BAR_W, BAR_H)?;
            // Drawing did a round trip that may have buffered more events.
            continue;
        }
        let wait = next.saturating_duration_since(Instant::now());
        let ts = libc::timespec { tv_sec: wait.as_secs() as _, tv_nsec: wait.subsec_nanos() as _ };
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::ppoll(&mut pfd, 1, &ts, std::ptr::null()) };
    }
}
