//! Latency probe app. Draws a scenario plus a 64-bit barcode at its top-left
//! corner: 64 blocks of 16x16 px, block i = bit i (white = 1).
//!   bits 0..8   magic 0xA5
//!   bits 8..48  CLOCK_REALTIME microseconds when the frame was drawn (low 40 bits)
//!   bits 48..64 x coordinate of the last pointer motion received
//! The client decodes it to measure app-draw -> client-display latency and
//! input -> app -> client round trips. Pointer motion is redrawn immediately.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::Event;

const BLOCK: usize = 16;
const BITS: usize = 64;
const BAR_W: usize = BLOCK * BITS;
const BAR_H: usize = BLOCK;
const MAGIC: u64 = 0xA5;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Static UI; only the barcode changes.
    Idle,
    /// A 256x256 box moving over static UI.
    Boxed,
    /// Whole window changes every frame with smooth video-like content.
    Full,
    /// Text-like content scrolling vertically.
    Scroll,
}

struct Args {
    mode: Mode,
    fps: f64,
}

fn parse_args() -> Result<Args> {
    let mut a = Args { mode: Mode::Idle, fps: 165.0 };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let val = it.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--mode" => {
                a.mode = match val.as_str() {
                    "idle" => Mode::Idle,
                    "box" => Mode::Boxed,
                    "full" => Mode::Full,
                    "scroll" => Mode::Scroll,
                    m => bail!("unknown mode {m}"),
                }
            }
            "--fps" => a.fps = val.parse()?,
            _ => bail!("unknown flag {flag}"),
        }
    }
    Ok(a)
}

fn now_us() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64
}

struct Canvas {
    px: &'static mut [u32],
    w: usize,
    h: usize,
}

impl Canvas {
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        for yy in y..(y + h).min(self.h) {
            let row = &mut self.px[yy * self.w..yy * self.w + self.w];
            row[x.min(self.w)..(x + w).min(self.w)].fill(c);
        }
    }

    fn barcode(&mut self, marker: u16) {
        let v = MAGIC | ((now_us() & ((1 << 40) - 1)) << 8) | ((marker as u64) << 48);
        for i in 0..BITS {
            let c = if v >> i & 1 == 1 { 0xFFFFFF } else { 0 };
            self.fill(i * BLOCK, 0, BLOCK, BLOCK, c);
        }
    }

    /// Fake UI text: rows of dark word-like blocks on a light background.
    fn text(&mut self, scroll: usize) {
        for y in BAR_H..self.h {
            let doc_y = y + scroll;
            let line = doc_y / 22;
            let in_line = doc_y % 22;
            let row = &mut self.px[y * self.w..(y + 1) * self.w];
            if !(6..18).contains(&in_line) {
                row.fill(0xF0F0F0);
                continue;
            }
            let mut seed = (line as u32).wrapping_mul(2654435761) | 1;
            let mut x = 24;
            row.fill(0xF0F0F0);
            while x < row.len() - 24 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let word = 12 + (seed % 70) as usize;
                let end = (x + word).min(row.len() - 24);
                let shade = (0x20 + (seed >> 24) / 4) * 0x010101;
                for (i, p) in row[x..end].iter_mut().enumerate() {
                    // glyph-like vertical stems
                    if (i + in_line) % 3 != 0 {
                        *p = shade;
                    }
                }
                x = end + 8;
            }
        }
    }
}

struct Plasma {
    sx: Vec<u32>,
    sy: Vec<u32>,
}

impl Plasma {
    fn new() -> Self {
        let table = |period: f64| -> Vec<u32> {
            (0..4096).map(|i| ((((i as f64) * std::f64::consts::TAU / period).sin() + 1.0) * 63.0) as u32).collect()
        };
        Self { sx: table(700.0), sy: table(500.0) }
    }

    fn draw(&self, c: &mut Canvas, t: usize) {
        for y in BAR_H..c.h {
            let vy = self.sy[(y + t * 3) & 4095];
            let vy2 = self.sx[(y * 2 + t) & 4095];
            let row = &mut c.px[y * c.w..(y + 1) * c.w];
            for (x, p) in row.iter_mut().enumerate() {
                let a = self.sx[(x + t * 5) & 4095] + vy;
                let b = self.sy[(x * 2 + 4096 - (t * 2 & 4095)) & 4095] + vy2;
                *p = (a << 16) | (b << 8) | ((a + b) >> 1);
            }
        }
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
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
