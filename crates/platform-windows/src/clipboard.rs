//! Text clipboard of the interactive window station.
//!
//! A message-only window on its own thread listens for clipboard updates
//! (WM_CLIPBOARDUPDATE) and is the owner when text is set: SetClipboardData
//! needs an owner window, and the clipboard must be opened by the thread
//! that owns it. Updates are read a moment later, not at once: the program
//! that copies is often still writing (OLE sets the data, then flushes it),
//! and opening the clipboard under it makes its copy fail. Text crosses as
//! UTF-16 with CRLF line ends on the Windows side and as UTF-8 with LF on
//! ours.

use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use remothing_core::platform::{Clipboard, ClipboardSink};
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, PostMessageW, SetTimer, HWND_MESSAGE,
    MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_TIMER,
};

const CF_UNICODETEXT: u32 = 13;
/// Text waits in `Shared::pending`.
const WM_SET_TEXT: u32 = WM_APP;
const WM_STOP: u32 = WM_APP + 1;
/// Another program may hold the clipboard open for a moment.
const OPEN_TRIES: u32 = 10;
const OPEN_RETRY: Duration = Duration::from_millis(10);
/// Timer that reads the clipboard this long after its last update.
const READ_TIMER: usize = 1;
const READ_DELAY_MS: u32 = 100;
/// Longest wait for the clipboard thread to write a text (it retries opening for 100 ms).
const SET_WAIT: Duration = Duration::from_secs(1);

/// Text to put on the clipboard (UTF-16, NUL-terminated), and who waits for it.
struct Pending {
    text: Vec<u16>,
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
}

impl Clipboard for WindowClipboard {
    /// Returns once the text is on the clipboard: a paste pressed next pastes it.
    fn set_text(&self, text: &str) -> Result<()> {
        let wide: Vec<u16> = text.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().chain([0]).collect();
        let (done, written) = mpsc::channel();
        *self.shared.pending.lock().unwrap() = Some(Pending { text: wide, done });
        self.post(WM_SET_TEXT)?;
        match written.recv_timeout(SET_WAIT) {
            Ok(result) => result,
            // A newer text took this one's place before it was written.
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!("the clipboard thread did not answer")),
        }
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
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid MSG; the window belongs to this thread.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        match msg.message {
            // SAFETY: a timer on this thread's window; setting it again
            // restarts it, so a burst of updates is read once.
            WM_CLIPBOARDUPDATE => unsafe {
                SetTimer(Some(hwnd), READ_TIMER, READ_DELAY_MS, None);
            },
            WM_TIMER if msg.wParam.0 == READ_TIMER => {
                // SAFETY: the timer set above.
                let _ = unsafe { KillTimer(Some(hwnd), READ_TIMER) };
                let changed = !shared.found.swap(false, Ordering::Relaxed);
                // Our own text comes back here too; the core ignores what it already has.
                if let (Some(text), Some(on_change)) = (read_text(hwnd), shared.on_change.lock().unwrap().as_ref()) {
                    on_change(text, changed);
                }
            }
            WM_SET_TEXT => {
                let Some(Pending { text, done }) = shared.pending.lock().unwrap().take() else { continue };
                let _ = done.send(write_text(hwnd, &text));
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

fn open(hwnd: HWND) -> Result<()> {
    let mut tries = 0;
    loop {
        // SAFETY: `hwnd` is this thread's window.
        match unsafe { OpenClipboard(Some(hwnd)) } {
            Ok(()) => return Ok(()),
            Err(e) if tries == OPEN_TRIES => return Err(e).context("open clipboard"),
            Err(_) => {
                tries += 1;
                std::thread::sleep(OPEN_RETRY);
            }
        }
    }
}

/// None when the clipboard holds no text (images, files) or cannot be opened.
fn read_text(hwnd: HWND) -> Option<String> {
    open(hwnd).ok()?;
    // SAFETY: the clipboard is open; the handle stays valid until it is
    // closed, and its memory is read within its size while locked.
    let text = unsafe {
        GetClipboardData(CF_UNICODETEXT).ok().and_then(|data| {
            let memory = HGLOBAL(data.0);
            let chars = GlobalLock(memory) as *const u16;
            if chars.is_null() {
                return None;
            }
            let wide = std::slice::from_raw_parts(chars, GlobalSize(memory) / 2);
            let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            let text = String::from_utf16_lossy(&wide[..end]).replace("\r\n", "\n");
            let _ = GlobalUnlock(memory);
            Some(text)
        })
    };
    // SAFETY: opened above.
    let _ = unsafe { CloseClipboard() };
    text
}

/// `text`: UTF-16, NUL-terminated.
fn write_text(hwnd: HWND, text: &[u16]) -> Result<()> {
    open(hwnd)?;
    // SAFETY: the clipboard is open with `hwnd` as owner; the block is
    // large enough for `text` and, once set, belongs to the clipboard.
    let set = unsafe {
        EmptyClipboard().context("empty clipboard").and_then(|()| {
            let memory = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(text)).context("allocate clipboard text")?;
            let chars = GlobalLock(memory) as *mut u16;
            std::ptr::copy_nonoverlapping(text.as_ptr(), chars, text.len());
            let _ = GlobalUnlock(memory);
            SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))).map(|_| ()).inspect_err(|_| {
                let _ = GlobalFree(Some(memory));
            })?;
            Ok(())
        })
    };
    // SAFETY: opened above.
    let _ = unsafe { CloseClipboard() };
    set
}
