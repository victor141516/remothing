//! Input injection with SendInput into one display of the virtual desktop.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::{bail, Result};
use remothing_core::platform::{Button, DisplayInfo, InputProvider};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WHEEL_DELTA,
    XBUTTON1, XBUTTON2,
};

use crate::desktop::{self, Followed};
use crate::keymap::{self, KeyCode};
use crate::uipi::ForegroundCheck;

// Held-key bits for Ctrl+Alt+Delete detection.
const LCTRL: u8 = 1;
const RCTRL: u8 = 2;
const LALT: u8 = 4;
const RALT: u8 = 8;
/// Delete / numpad '.' press turned into a SAS: its release is swallowed too.
const SAS_DELETE: u8 = 16;
const SAS_KPDOT: u8 = 32;

pub struct SendInputProvider {
    /// Display origin and size in virtual-desktop pixels.
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    keys: AtomicU8,
    /// Set by a rejected SendInput, cleared by the next accepted one, so a
    /// blocked episode (secure desktop, elevated foreground window) is
    /// reported once instead of for every mouse move.
    blocked: AtomicBool,
    /// UIPI drops are invisible to SendInput: checked against the foreground
    /// window instead, and reported once per blocked episode.
    uipi: ForegroundCheck,
    uipi_blocked: AtomicBool,
}

impl SendInputProvider {
    pub fn new(info: &DisplayInfo) -> Self {
        Self {
            x: info.x,
            y: info.y,
            width: info.width,
            height: info.height,
            keys: AtomicU8::new(0),
            blocked: AtomicBool::new(false),
            uipi: ForegroundCheck::new(),
            uipi_blocked: AtomicBool::new(false),
        }
    }

    fn send(&self, inputs: &[INPUT]) -> Result<()> {
        desktop::follow_input_desktop(false);
        // Input is still sent: a wrong guess must not cost working input.
        let uipi = self.uipi.blocked();
        let newly_blocked = match &uipi {
            Some(_) => !self.uipi_blocked.swap(true, Ordering::Relaxed),
            None => {
                self.uipi_blocked.store(false, Ordering::Relaxed);
                false
            }
        };
        let size = std::mem::size_of::<INPUT>() as i32;
        // SAFETY: `inputs` are fully initialized INPUT structures.
        let mut sent = unsafe { SendInput(inputs, size) } as usize;
        if sent < inputs.len() && desktop::follow_input_desktop(true) == Followed::Switched {
            // SAFETY: as above.
            sent += unsafe { SendInput(&inputs[sent..], size) } as usize;
        }
        if sent == inputs.len() {
            self.blocked.store(false, Ordering::Relaxed);
            if let (true, Some(reason)) = (newly_blocked, uipi) {
                bail!(
                    "Windows drops this input (UIPI): the foreground window belongs to {reason}. \
                     Bring a normal window to the front, close it, or run remothing as the service"
                );
            }
            return Ok(());
        }
        let err = windows::core::Error::from_win32();
        if !self.blocked.swap(true, Ordering::Relaxed) {
            bail!("SendInput rejected (secure desktop or higher-integrity foreground window?): {err}");
        }
        Ok(())
    }

    fn mouse(&self, flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
        mouse_input(0, 0, flags, data)
    }

    fn key_input(&self, code: KeyCode, down: bool) -> INPUT {
        let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
        let ext = |e: bool| if e { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
        let (vk, scan, flags) = match code {
            KeyCode::Scan { code, extended } => (0, code, KEYEVENTF_SCANCODE | ext(extended) | up),
            KeyCode::Virtual { vk, scan, extended } => (vk, scan, ext(extended) | up),
        };
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        }
    }

    /// Tracks Ctrl/Alt and turns Ctrl+Alt+Delete into a SAS request. Returns
    /// true when the key event must not be injected.
    fn intercept_sas(&self, evdev: u16, down: bool) -> bool {
        let bit = match evdev {
            keymap::EV_LEFTCTRL => LCTRL,
            keymap::EV_RIGHTCTRL => RCTRL,
            keymap::EV_LEFTALT => LALT,
            keymap::EV_RIGHTALT => RALT,
            keymap::EV_DELETE => SAS_DELETE,
            keymap::EV_KPDOT => SAS_KPDOT,
            _ => return false,
        };
        if bit & (SAS_DELETE | SAS_KPDOT) == 0 {
            if down {
                self.keys.fetch_or(bit, Ordering::Relaxed);
            } else {
                self.keys.fetch_and(!bit, Ordering::Relaxed);
            }
            return false;
        }
        let held = self.keys.load(Ordering::Relaxed);
        if !down {
            // Swallow the release of a Delete that became a SAS.
            return self.keys.fetch_and(!bit, Ordering::Relaxed) & bit != 0;
        }
        if held & (LCTRL | RCTRL) == 0 || held & (LALT | RALT) == 0 {
            return false;
        }
        self.keys.fetch_or(bit, Ordering::Relaxed);
        if let Err(e) = crate::service::send_sas() {
            eprintln!("Ctrl+Alt+Delete: SendSAS failed: {e:#}");
        }
        true
    }
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            // mouseData is a DWORD carrying signed wheel deltas.
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// Normalizes virtual-desktop pixel `p` (relative to the virtual screen
/// origin) to 0..=65535 over `extent` pixels. Windows maps back with
/// `n * extent / 65536`; rounding up makes that land exactly on `p`.
fn normalize(p: i32, extent: i32) -> i32 {
    let extent = i64::from(extent.max(1));
    ((i64::from(p) * 65536 + extent - 1) / extent).clamp(0, 65535) as i32
}

impl InputProvider for SendInputProvider {
    fn motion(&self, x: u32, y: u32) -> Result<()> {
        // Physical pixels: the process is per-monitor DPI aware (see lib.rs).
        // SAFETY: GetSystemMetrics has no preconditions.
        let (vx, vy, cx, cy) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        // Clamped so input aimed at this display never lands on a neighbour.
        let px = self.x + x.min(self.width.saturating_sub(1)) as i32;
        let py = self.y + y.min(self.height.saturating_sub(1)) as i32;
        let flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        self.send(&[mouse_input(normalize(px - vx, cx), normalize(py - vy, cy), flags, 0)])
    }

    fn button(&self, button: Button, down: bool) -> Result<()> {
        let (flags, data) = match (button, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Back, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1 as i32),
            (Button::Forward, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2 as i32),
        };
        self.send(&[self.mouse(flags, data)])
    }

    fn wheel(&self, dx: i32, dy: i32) -> Result<()> {
        let notch = WHEEL_DELTA as i32;
        // Windows: positive vertical = away from the user (up), positive horizontal = right.
        match (dx, dy) {
            (0, 0) => Ok(()),
            (0, dy) => self.send(&[self.mouse(MOUSEEVENTF_WHEEL, -dy * notch)]),
            (dx, 0) => self.send(&[self.mouse(MOUSEEVENTF_HWHEEL, dx * notch)]),
            (dx, dy) => self.send(&[self.mouse(MOUSEEVENTF_WHEEL, -dy * notch), self.mouse(MOUSEEVENTF_HWHEEL, dx * notch)]),
        }
    }

    fn key(&self, evdev: u16, down: bool) -> Result<()> {
        let Some(code) = keymap::lookup(evdev) else { return Ok(()) };
        if self.intercept_sas(evdev, down) {
            return Ok(());
        }
        self.send(&[self.key_input(code, down)])
    }
}

#[cfg(test)]
mod tests {
    use super::normalize;

    /// Every pixel must survive Windows' `n * extent / 65536` mapping (and its
    /// rounding variant) unchanged.
    #[test]
    fn normalize_round_trips() {
        for extent in [1, 640, 1080, 1440, 3600, 3849, 7680, 16384] {
            for p in 0..extent {
                let n = i64::from(normalize(p, extent));
                assert_eq!(n * i64::from(extent) / 65536, i64::from(p), "floor extent {extent} p {p}");
                assert_eq!((n * i64::from(extent) + 32768) / 65536, i64::from(p), "round extent {extent} p {p}");
            }
        }
    }
}
