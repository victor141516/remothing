//! Follows the input desktop (Default ↔ Winlogon/secure desktop) per thread.
//!
//! Desktop duplication and SendInput only reach the desktop the calling thread
//! is attached to, so the capture thread (before re-duplicating) and every
//! input thread (before SendInput) re-attach when the input desktop changes.
//! A normal user process may not open the secure desktop: that is reported as
//! `Denied`, retried at most every `RETRY_INTERVAL`, and logged once per episode.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};

/// How often an unforced call re-checks the input desktop.
const CHECK_INTERVAL: Duration = Duration::from_millis(500);
/// Minimum delay between attempts while the input desktop is not accessible.
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Followed {
    /// The thread already was on the input desktop (or the check was skipped).
    Same,
    /// The thread moved to a different input desktop.
    Switched,
    /// The input desktop cannot be opened or attached (e.g. secure desktop as a normal user).
    Denied,
}

#[derive(Default)]
struct ThreadDesktop {
    /// Desktop this thread was attached to by us. Stays open: a desktop handle
    /// must remain valid while it is assigned to a thread.
    handle: Option<HDESK>,
    name: Vec<u16>,
    next_check: Option<Instant>,
    denied: bool,
}

thread_local! {
    static STATE: RefCell<ThreadDesktop> = RefCell::default();
}

/// Attaches the calling thread to the current input desktop if it changed.
/// `force` skips the periodic-check delay (after DXGI_ERROR_ACCESS_LOST or a
/// failed SendInput) but never the retry delay while access is denied.
pub fn follow_input_desktop(force: bool) -> Followed {
    STATE.with_borrow_mut(|s| {
        let now = Instant::now();
        if let Some(next) = s.next_check {
            if now < next && (!force || s.denied) {
                return if s.denied { Followed::Denied } else { Followed::Same };
            }
        }
        let was_denied = s.denied;
        match s.follow() {
            Ok(followed) => {
                if was_denied {
                    eprintln!("input desktop accessible again");
                }
                s.denied = false;
                s.next_check = Some(now + CHECK_INTERVAL);
                followed
            }
            Err(e) => {
                if !was_denied {
                    eprintln!("input desktop not accessible (retrying every {RETRY_INTERVAL:?}): {e}");
                }
                s.denied = true;
                s.next_check = Some(now + RETRY_INTERVAL);
                Followed::Denied
            }
        }
    })
}

impl ThreadDesktop {
    /// `Switched` whenever the thread was (re)attached, including the first time.
    fn follow(&mut self) -> windows::core::Result<Followed> {
        // SAFETY: plain Win32 calls; every handle opened here is either stored
        // (attached to this thread) or closed before returning.
        unsafe {
            let desk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0))?;
            let mut buf = [0u16; 256];
            let mut needed = 0u32;
            let named = GetUserObjectInformationW(
                HANDLE(desk.0),
                UOI_NAME,
                Some(buf.as_mut_ptr().cast()),
                std::mem::size_of_val(&buf) as u32,
                Some(&mut needed),
            );
            if let Err(e) = named {
                let _ = CloseDesktop(desk);
                return Err(e);
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let name = &buf[..len];
            if self.handle.is_some() && name == self.name.as_slice() {
                let _ = CloseDesktop(desk);
                return Ok(Followed::Same);
            }
            if let Err(e) = SetThreadDesktop(desk) {
                let _ = CloseDesktop(desk);
                return Err(e);
            }
            let first = self.handle.is_none();
            if let Some(old) = self.handle.replace(desk) {
                let _ = CloseDesktop(old);
            }
            self.name.clear();
            self.name.extend_from_slice(name);
            if !first {
                eprintln!("input desktop switched to {}", String::from_utf16_lossy(name));
            }
            Ok(Followed::Switched)
        }
    }
}
