//! Clipboard of the interactive window station: text, pictures, and the
//! files Explorer copies (CF_HDROP, read only; as the service, through a
//! helper run as the user, see `read_as_user`).
//!
//! A message-only window on its own thread listens for clipboard updates
//! (WM_CLIPBOARDUPDATE) and is the owner when something is set:
//! SetClipboardData needs an owner window, and the clipboard must be opened
//! by the thread that owns it. Updates are read a moment later, not at
//! once: the program that copies is often still writing (OLE sets the data,
//! then flushes it), and opening the clipboard under it makes its copy
//! fail. Text crosses as UTF-16 with CRLF line ends on the Windows side and
//! as UTF-8 with LF on ours. Pictures cross as PNG: the registered "PNG"
//! format browsers and Office use, read as it is, else a device-independent
//! bitmap (CF_DIBV5 / CF_DIB, which Windows also makes from a CF_BITMAP)
//! converted; a picture set here goes as "PNG" and as CF_DIB, the format
//! every program pasting a bitmap takes (Windows derives CF_DIBV5 and
//! CF_BITMAP from it).

use std::ffi::OsStr;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use remothing_core::platform::{Clipboard, ClipboardSink, Copied};
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, E_ACCESSDENIED, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, PostMessageW, SetTimer, HWND_MESSAGE,
    MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_TIMER,
};

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;
/// BITMAPINFOHEADER.biCompression values: plain pixels, or pixels with
/// their channel masks given.
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BITMAPINFOHEADER_SIZE: usize = 40;
/// Most a decoded picture may take (a 16 MiB PNG can unpack to gigabytes).
const MAX_PIXEL_BYTES: usize = 1 << 29;
/// The subcommand that prints what is on the clipboard (`print_clipboard`).
pub const READ_COMMAND: &str = "clipboard-read";
/// What waits in `Shared::pending`.
const WM_SET: u32 = WM_APP;
const WM_STOP: u32 = WM_APP + 1;
/// Another program may hold the clipboard open for a moment.
const OPEN_TRIES: u32 = 10;
const OPEN_RETRY: Duration = Duration::from_millis(10);
/// Timer that reads the clipboard this long after its last update.
const READ_TIMER: usize = 1;
const READ_DELAY_MS: u32 = 100;
/// A read that fails (most often: another program, such as the clipboard
/// history or a second remothing, still holds the clipboard open; a picture
/// keeps them busy longer) is tried again this many times, READ_DELAY_MS apart.
const READ_RETRIES: u32 = 10;
/// Longest wait for the clipboard thread to write (it retries opening for 100 ms).
const SET_WAIT: Duration = Duration::from_secs(1);

/// Formats with their data to put on the clipboard, and who waits for it.
struct Pending {
    formats: Vec<(u32, Vec<u8>)>,
    done: mpsc::Sender<Result<()>>,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<Option<Pending>>,
    on_change: Mutex<Option<ClipboardSink>>,
    /// The next read reports what was there when watching started.
    found: AtomicBool,
}

pub struct WindowClipboard {
    /// The listener window (an HWND, kept as an integer to cross threads).
    window: isize,
    shared: Arc<Shared>,
}

impl WindowClipboard {
    pub fn new() -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let (ready, window) = mpsc::channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new().name("clipboard".into()).spawn(move || {
            // SAFETY: a message-only window of a system class, created,
            // used and destroyed on this thread.
            let created = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("remothing clipboard"),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                )
                .and_then(|hwnd| AddClipboardFormatListener(hwnd).map(|()| hwnd))
            };
            match created {
                Ok(hwnd) => {
                    let _ = ready.send(Ok(hwnd.0 as isize));
                    listen(hwnd, &thread_shared);
                }
                Err(e) => {
                    let _ = ready.send(Err(e));
                }
            }
        })?;
        let window = window.recv().context("clipboard thread")?.context("clipboard listener window")?;
        Ok(Self { window, shared })
    }

    fn post(&self, message: u32) -> Result<()> {
        // SAFETY: posting to a window handle; a stale one only fails the call.
        unsafe { PostMessageW(Some(HWND(self.window as _)), message, WPARAM(0), LPARAM(0)) }?;
        Ok(())
    }

    /// Returns once the formats are on the clipboard: a paste pressed next pastes them.
    fn set(&self, formats: Vec<(u32, Vec<u8>)>) -> Result<()> {
        let (done, written) = mpsc::channel();
        *self.shared.pending.lock().unwrap() = Some(Pending { formats, done });
        self.post(WM_SET)?;
        match written.recv_timeout(SET_WAIT) {
            Ok(result) => result,
            // Newer content took this one's place before it was written.
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!("the clipboard thread did not answer")),
        }
    }
}

impl Clipboard for WindowClipboard {
    fn set_text(&self, text: &str) -> Result<()> {
        let wide: Vec<u8> = text
            .replace("\r\n", "\n")
            .replace('\n', "\r\n")
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        self.set(vec![(CF_UNICODETEXT, wide)])
    }

    fn set_image(&self, png: &[u8]) -> Result<()> {
        // Decoded here, not on the clipboard thread, which keeps listening meanwhile.
        let dib = png_dib(png).context("decode the picture")?;
        self.set(vec![(png_format()?, png.to_vec()), (CF_DIB, dib)])
    }

    fn watch(&self, on_change: ClipboardSink) {
        *self.shared.on_change.lock().unwrap() = Some(on_change);
        // What is on the clipboard now is the first report.
        self.shared.found.store(true, Ordering::Relaxed);
        let _ = self.post(WM_CLIPBOARDUPDATE);
    }
}

impl Drop for WindowClipboard {
    fn drop(&mut self) {
        let _ = self.post(WM_STOP);
    }
}

/// Message loop of the listener window, until WM_STOP.
fn listen(hwnd: HWND, shared: &Shared) {
    // As the service (SYSTEM) Windows hides from this process what programs
    // copy through OLE: a helper running as the user reads it (`read_as_user`).
    let as_system = crate::service::is_local_system().unwrap_or(false);
    let mut msg = MSG::default();
    // Reads of the current update tried again so far.
    let mut retries = 0;
    // SAFETY: `msg` is a valid MSG; the window belongs to this thread.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        match msg.message {
            WM_CLIPBOARDUPDATE => {
                retries = 0;
                // SAFETY: a timer on this thread's window; setting it again
                // restarts it, so a burst of updates is read once.
                unsafe { SetTimer(Some(hwnd), READ_TIMER, READ_DELAY_MS, None) };
            }
            WM_TIMER if msg.wParam.0 == READ_TIMER => {
                // SAFETY: the timer set above.
                let _ = unsafe { KillTimer(Some(hwnd), READ_TIMER) };
                let copied = match read(Some(hwnd)) {
                    Ok(None) if as_system => read_as_user(),
                    read => read,
                };
                let busy = |e: &anyhow::Error| e.downcast_ref::<windows::core::Error>().is_some_and(|e| e.code() == E_ACCESSDENIED);
                match copied {
                    Err(e) if retries < READ_RETRIES && busy(&e) => {
                        retries += 1;
                        // SAFETY: a timer on this thread's window.
                        unsafe { SetTimer(Some(hwnd), READ_TIMER, READ_DELAY_MS, None) };
                        continue;
                    }
                    Err(e) => eprintln!("clipboard: {e:#}"),
                    // Our own copy comes back here too; the core ignores what it already has.
                    Ok(Some(copied)) => {
                        if let Some(on_change) = shared.on_change.lock().unwrap().as_ref() {
                            on_change(copied, !shared.found.load(Ordering::Relaxed));
                        }
                    }
                    Ok(None) => {}
                }
                // Read, or given up on: what comes next is a change.
                shared.found.store(false, Ordering::Relaxed);
            }
            WM_SET => {
                let Some(Pending { formats, done }) = shared.pending.lock().unwrap().take() else { continue };
                let _ = done.send(write(hwnd, &formats));
            }
            WM_STOP => break,
            _ => {
                // SAFETY: a message retrieved by GetMessageW above.
                unsafe { DispatchMessageW(&msg) };
            }
        }
    }
    // SAFETY: the window was created on this thread.
    unsafe {
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = DestroyWindow(hwnd);
    }
}

/// The id of the registered "PNG" clipboard format.
fn png_format() -> Result<u32> {
    // SAFETY: a NUL-terminated constant name.
    let format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    ensure!(format != 0, "register the PNG clipboard format: {}", windows::core::Error::from_win32());
    Ok(format)
}

/// `hwnd`: the window that owns the clipboard while it is open (None: this
/// process's helper run, which only reads).
fn open(hwnd: Option<HWND>) -> Result<()> {
    let mut tries = 0;
    loop {
        // SAFETY: `hwnd` is this thread's window, or none.
        match unsafe { OpenClipboard(hwnd) } {
            Ok(()) => return Ok(()),
            Err(e) if tries == OPEN_TRIES => return Err(e).context("open clipboard"),
            Err(_) => {
                tries += 1;
                std::thread::sleep(OPEN_RETRY);
            }
        }
    }
}

/// Files (Explorer's copy) when there are, else text, else a picture; None
/// for anything else. An error says why what is there could not be read.
fn read(hwnd: Option<HWND>) -> Result<Option<Copied>> {
    let png = png_format()?;
    open(hwnd)?;
    // SAFETY: the clipboard is open; its handles stay valid until it is closed.
    let available = |format| unsafe { IsClipboardFormatAvailable(format) }.is_ok();
    // SAFETY: as above; each handle is used as the block its format names.
    let copied = unsafe {
        if available(CF_HDROP) {
            GetClipboardData(CF_HDROP).context("read the copied files").map(|data| Some(Copied::Files(drop_files(HDROP(data.0)))))
        } else if available(CF_UNICODETEXT) {
            GetClipboardData(CF_UNICODETEXT)
                .context("read the copied text")
                .and_then(|data| unicode_text(HGLOBAL(data.0)))
                .map(|text| Some(Copied::Text(text)))
        } else if available(png) {
            GetClipboardData(png)
                .context("read the copied PNG")
                .and_then(|data| with_block(HGLOBAL(data.0), |bytes| png_file(bytes).map(<[u8]>::to_vec)))
                .map(|png| Some(Copied::Image(png)))
        } else if let Some(format) = [CF_DIBV5, CF_DIB].into_iter().find(|&f| available(f)) {
            GetClipboardData(format)
                .context("read the copied picture")
                .and_then(|data| with_block(HGLOBAL(data.0), dib_png))
                .map(|png| Some(Copied::Image(png)))
        } else {
            Ok(None)
        }
    };
    // SAFETY: opened above.
    let _ = unsafe { CloseClipboard() };
    copied
}

/// What is on the clipboard as the logged-in user sees it. Explorer (and
/// other programs using OLE) puts its copies there rendered on request,
/// and Windows shows a process running as SYSTEM only OLE's own
/// "DataObject" format: neither CF_HDROP nor the text, nor through OLE's
/// own path, nor impersonating the user (all measured). This program, run
/// as the user, prints it (`print_clipboard`).
fn read_as_user() -> Result<Option<Copied>> {
    let out = crate::service::run_as_user(&[OsStr::new(READ_COMMAND)]).context("read the clipboard as the user")?;
    let Some(newline) = out.iter().position(|&b| b == b'\n') else { return Ok(None) };
    let (kind, body) = (&out[..newline], &out[newline + 1..]);
    Ok(match kind {
        b"files" => Some(Copied::Files(String::from_utf8_lossy(body).lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())),
        b"text" => Some(Copied::Text(String::from_utf8_lossy(body).into_owned())),
        b"image" => Some(Copied::Image(body.to_vec())),
        _ => None,
    })
}

/// `remothing clipboard-read`, run as the user by `read_as_user`: prints
/// "files" and one path per line, "text" and the text, "image" and the PNG
/// file, or nothing.
pub fn print_clipboard() -> Result<()> {
    let mut out = std::io::stdout().lock();
    match read(None)? {
        Some(Copied::Files(files)) => {
            writeln!(out, "files")?;
            for file in files {
                writeln!(out, "{}", file.display())?;
            }
        }
        Some(Copied::Text(text)) => write!(out, "text\n{text}")?,
        Some(Copied::Image(png)) => {
            out.write_all(b"image\n")?;
            out.write_all(&png)?;
        }
        None => {}
    }
    out.flush()?;
    Ok(())
}

/// The paths of a CF_HDROP block.
fn drop_files(drop: HDROP) -> Vec<PathBuf> {
    // SAFETY: `drop` is a valid CF_HDROP block for the duration of the call;
    // each name is read into a buffer of the length asked for.
    unsafe {
        (0..DragQueryFileW(drop, u32::MAX, None))
            .filter_map(|i| {
                let mut name = vec![0u16; DragQueryFileW(drop, i, None) as usize + 1];
                let len = DragQueryFileW(drop, i, Some(&mut name)) as usize;
                (len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&name[..len])))
            })
            .collect()
    }
}

/// Runs `f` on the bytes of a global memory block (as large as Windows
/// allocated it, which may be more than was asked for).
fn with_block<T>(memory: HGLOBAL, f: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
    // SAFETY: a valid global memory block, read within its size while locked.
    unsafe {
        let bytes = GlobalLock(memory) as *const u8;
        if bytes.is_null() {
            bail!("lock the copied data: {}", windows::core::Error::from_win32());
        }
        let result = f(std::slice::from_raw_parts(bytes, GlobalSize(memory)));
        let _ = GlobalUnlock(memory);
        result
    }
}

/// A CF_UNICODETEXT block, with LF line ends.
fn unicode_text(memory: HGLOBAL) -> Result<String> {
    with_block(memory, |bytes| {
        let wide: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        Ok(String::from_utf16_lossy(&wide).replace("\r\n", "\n"))
    })
}

/// Puts `formats` on the clipboard, replacing what was there.
fn write(hwnd: HWND, formats: &[(u32, Vec<u8>)]) -> Result<()> {
    open(Some(hwnd))?;
    // SAFETY: the clipboard is open with `hwnd` as owner; each block is as
    // large as its data and, once set, belongs to the clipboard.
    let set = unsafe {
        EmptyClipboard().context("empty clipboard").and_then(|()| {
            for (format, data) in formats {
                let memory = GlobalAlloc(GMEM_MOVEABLE, data.len()).context("allocate clipboard data")?;
                let bytes = GlobalLock(memory) as *mut u8;
                std::ptr::copy_nonoverlapping(data.as_ptr(), bytes, data.len());
                let _ = GlobalUnlock(memory);
                SetClipboardData(*format, Some(HANDLE(memory.0))).map(|_| ()).inspect_err(|_| {
                    let _ = GlobalFree(Some(memory));
                })?;
            }
            Ok(())
        })
    };
    // SAFETY: opened above.
    let _ = unsafe { CloseClipboard() };
    set
}

/// The PNG file at the start of `bytes` (a clipboard block may be longer):
/// up to the end of its IEND chunk.
fn png_file(bytes: &[u8]) -> Result<&[u8]> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    ensure!(bytes.starts_with(SIGNATURE), "not a PNG file");
    let mut at = SIGNATURE.len();
    while let Some(header) = bytes.get(at..at + 8) {
        let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        // Length, type, data, CRC.
        at += 12 + len;
        if &header[4..] == b"IEND" {
            ensure!(at <= bytes.len(), "the PNG file is cut short");
            return Ok(&bytes[..at]);
        }
    }
    bail!("the PNG file is cut short")
}

/// A DIB (BITMAPINFOHEADER or a later version, then the pixels) as PNG.
/// Uncompressed 24 and 32 bits a pixel, as Windows makes them from a
/// screen bitmap; 32-bit pixels whose alpha bytes are all zero are opaque
/// (that byte was unused before CF_DIBV5).
fn dib_png(dib: &[u8]) -> Result<Vec<u8>> {
    let u32_at = |at: usize| dib.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).context("the bitmap is cut short");
    let header = u32_at(0)? as usize;
    let (width, height) = (u32_at(4)? as i32, u32_at(8)? as i32);
    let bits = u32_at(12)? >> 16;
    let (compression, colors) = (u32_at(16)?, u32_at(32)? as usize);
    ensure!(header >= BITMAPINFOHEADER_SIZE && width > 0 && height != 0, "not a bitmap");
    ensure!(
        (bits == 24 && compression == BI_RGB) || (bits == 32 && (compression == BI_RGB || compression == BI_BITFIELDS)),
        "a bitmap of {bits} bits a pixel, compression {compression}: not taken"
    );
    // The red, green and blue masks follow a BITMAPINFOHEADER, and are part of later ones.
    let mut pixels_at = header + colors * 4;
    if compression == BI_BITFIELDS {
        ensure!(
            (u32_at(40)?, u32_at(44)?, u32_at(48)?) == (0xff_0000, 0xff00, 0xff),
            "a bitmap with unusual channel masks: not taken"
        );
        if header == BITMAPINFOHEADER_SIZE {
            pixels_at += 12;
        }
    }
    let (width, rows) = (width as usize, height.unsigned_abs() as usize);
    let step = bits as usize / 8;
    let stride = (width * step).div_ceil(4) * 4;
    ensure!(width * rows * 4 <= MAX_PIXEL_BYTES, "a bitmap of {width}x{rows} is too large");
    let pixels = dib.get(pixels_at..pixels_at + stride * rows).context("the bitmap is cut short")?;
    let alpha = bits == 32 && pixels.chunks_exact(stride).any(|row| row[..width * 4].chunks_exact(4).any(|p| p[3] != 0));
    let mut rgba = Vec::with_capacity(width * rows * 4);
    for y in 0..rows {
        // A positive height means the rows go bottom up.
        let row = if height > 0 { rows - 1 - y } else { y };
        for p in pixels[row * stride..][..width * step].chunks_exact(step) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], if alpha { p[3] } else { 255 }]);
        }
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width as u32, rows as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // A clipboard picture is read once, nearby: speed over size.
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header()?.write_image_data(&rgba)?;
    Ok(png)
}

/// A PNG file as a CF_DIB block: BITMAPINFOHEADER, then 32-bit BGRA
/// pixels, bottom row first.
fn png_dib(png: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let size = reader.output_buffer_size().context("the picture is too large")?;
    ensure!(size <= MAX_PIXEL_BYTES, "a picture of {size} bytes decoded is too large");
    let mut pixels = vec![0; size];
    let info = reader.next_frame(&mut pixels)?;
    let (width, height) = (info.width as usize, info.height as usize);
    let channels = info.color_type.samples();
    let mut dib = Vec::with_capacity(BITMAPINFOHEADER_SIZE + width * height * 4);
    for value in [BITMAPINFOHEADER_SIZE as u32, info.width, info.height] {
        dib.extend_from_slice(&value.to_le_bytes());
    }
    // Planes 1, 32 bits a pixel, BI_RGB, size of the pixels, then
    // resolution and palette fields left at 0.
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&BI_RGB.to_le_bytes());
    dib.extend_from_slice(&((width * height * 4) as u32).to_le_bytes());
    dib.resize(BITMAPINFOHEADER_SIZE, 0);
    for row in pixels[..info.line_size * height].chunks_exact(info.line_size).rev() {
        for p in row[..width * channels].chunks_exact(channels) {
            let [r, g, b, a] = match *p {
                [l] => [l, l, l, 255],
                [l, a] => [l, l, l, a],
                [r, g, b] => [r, g, b, 255],
                // RGBA, the only other color type of 8-bit samples.
                _ => [p[0], p[1], p[2], p[3]],
            };
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Ok(dib)
}
