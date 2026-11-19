//! Clipboard of the interactive window station: text, and the files
//! Explorer copies (CF_HDROP, read only; as the service, through a helper
//! run as the user, see `read_as_user`).
//!
//! A message-only window on its own thread listens for clipboard updates
//! (WM_CLIPBOARDUPDATE) and is the owner when text is set: SetClipboardData
//! needs an owner window, and the clipboard must be opened by the thread
//! that owns it. Updates are read a moment later, not at once: the program
//! that copies is often still writing (OLE sets the data, then flushes it),
//! and opening the clipboard under it makes its copy fail. Text crosses as
//! UTF-16 with CRLF line ends on the Windows side and as UTF-8 with LF on
//! ours.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use remothing_core::platform::{Clipboard, ClipboardSink, Copied};
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, PostMessageW, SetTimer, HWND_MESSAGE,
    MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_TIMER,
};

const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
/// The subcommand that prints what is on the clipboard (`print_clipboard`).
pub const READ_COMMAND: &str = "clipboard-read";
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
    // As the service (SYSTEM) Windows hides from this process what programs
    // copy through OLE: a helper running as the user reads it (`read_as_user`).
    let as_system = crate::service::is_local_system().unwrap_or(false);
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
                let copied = match read(Some(hwnd)) {
                    Ok(None) if as_system => read_as_user(),
                    read => read,
                };
                // Our own text comes back here too; the core ignores what it already has.
                match copied {
                    Ok(Some(copied)) => {
                        if let Some(on_change) = shared.on_change.lock().unwrap().as_ref() {
                            on_change(copied, changed);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("clipboard: {e:#}"),
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

/// Files (Explorer's copy) when there are, else text; None for anything
/// else (images). An error says why what is there could not be read.
fn read(hwnd: Option<HWND>) -> Result<Option<Copied>> {
    open(hwnd)?;
    // SAFETY: the clipboard is open; its handles stay valid until it is closed.
    let copied = unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_ok() {
            GetClipboardData(CF_HDROP).context("read the copied files").map(|data| Some(Copied::Files(drop_files(HDROP(data.0)))))
        } else if IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok() {
            GetClipboardData(CF_UNICODETEXT)
                .context("read the copied text")
                .and_then(|data| unicode_text(HGLOBAL(data.0)))
                .map(|text| Some(Copied::Text(text)))
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
    let out = String::from_utf8_lossy(&out);
    Ok(match out.split_once('\n') {
        Some(("files", list)) => Some(Copied::Files(list.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())),
        Some(("text", text)) => Some(Copied::Text(text.to_string())),
        _ => None,
    })
}

/// `remothing clipboard-read`, run as the user by `read_as_user`: prints
/// "files" and one path per line, or "text" and the text, or nothing.
pub fn print_clipboard() -> Result<()> {
    match read(None)? {
        Some(Copied::Files(files)) => {
            println!("files");
            for file in files {
                println!("{}", file.display());
            }
        }
        Some(Copied::Text(text)) => print!("text\n{text}"),
        None => {}
    }
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

/// A CF_UNICODETEXT block, with LF line ends.
fn unicode_text(memory: HGLOBAL) -> Result<String> {
    // SAFETY: a valid global memory block, read within its size while locked.
    unsafe {
        let chars = GlobalLock(memory) as *const u16;
        if chars.is_null() {
            bail!("lock the copied text: {}", windows::core::Error::from_win32());
        }
        let wide = std::slice::from_raw_parts(chars, GlobalSize(memory) / 2);
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        let text = String::from_utf16_lossy(&wide[..end]).replace("\r\n", "\n");
        let _ = GlobalUnlock(memory);
        Ok(text)
    }
}

/// `text`: UTF-16, NUL-terminated.
fn write_text(hwnd: HWND, text: &[u16]) -> Result<()> {
    open(Some(hwnd))?;
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
