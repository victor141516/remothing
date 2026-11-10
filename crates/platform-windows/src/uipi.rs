//! User Interface Privilege Isolation check. Windows silently drops injected
//! input (SendInput still reports success) while the foreground window
//! belongs to a process of higher integrity than the sender: elevated apps,
//! and UIAccess tools such as Magnifier or the On-Screen Keyboard, which run
//! at high integrity. Detecting it lets the server say why remote input does
//! nothing instead of failing silently.

use std::sync::Mutex;

use windows::core::{Owned, PWSTR};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TokenUIAccess,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

#[derive(Clone, Copy)]
struct Level {
    /// Mandatory integrity RID: 0x2000 medium, 0x3000 high, 0x4000 system.
    integrity: u32,
    ui_access: bool,
}

pub struct ForegroundCheck {
    /// This process; None when its own token is unreadable (no check then).
    own: Option<Level>,
    /// Last foreground window (as an address) and why input to it is dropped.
    last: Mutex<(usize, Option<String>)>,
}

impl ForegroundCheck {
    pub fn new() -> Self {
        // SAFETY: pseudo handle of the current process.
        Self { own: process_level(unsafe { GetCurrentProcess() }), last: Mutex::new((0, None)) }
    }

    /// Why input to the current foreground window is dropped, if it is.
    /// Only looks the window's process up when the foreground window changes.
    pub fn blocked(&self) -> Option<String> {
        let own = self.own?;
        if own.ui_access {
            // UIAccess senders are exempt from UIPI.
            return None;
        }
        // SAFETY: no preconditions.
        let hwnd = unsafe { GetForegroundWindow() };
        let mut last = self.last.lock().unwrap();
        if last.0 != hwnd.0 as usize {
            *last = (hwnd.0 as usize, foreground_reason(hwnd, own));
        }
        last.1.clone()
    }
}

fn foreground_reason(hwnd: HWND, own: Level) -> Option<String> {
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    // SAFETY: valid output pointer; a stale HWND only yields pid 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    // SAFETY: plain query access; handle owned below.
    let process = unsafe { Owned::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?) };
    let name = image_name(*process).unwrap_or_else(|| format!("process {pid}"));
    match process_level(*process) {
        Some(l) if l.integrity > own.integrity => Some(format!(
            "{name} ({}, {} integrity; this server runs at {})",
            if l.ui_access { "UIAccess" } else { "elevated" },
            level_name(l.integrity),
            level_name(own.integrity),
        )),
        Some(_) => None,
        // Readable to any same-user process unless it runs above us.
        None => Some(format!("{name} (token unreadable: most likely runs at a higher integrity level)")),
    }
}

fn level_name(rid: u32) -> &'static str {
    match rid {
        0..0x1000 => "untrusted",
        0x1000..0x2000 => "low",
        0x2000..0x3000 => "medium",
        0x3000..0x4000 => "high",
        _ => "system",
    }
}

fn process_level(process: HANDLE) -> Option<Level> {
    let mut token = HANDLE::default();
    // SAFETY: valid process handle; output handle owned below.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    // SAFETY: handle owned here.
    let token = unsafe { Owned::new(token) };
    // TOKEN_MANDATORY_LABEL is followed by the SID it points to; u64 keeps it aligned.
    let mut buf = [0u64; 8];
    let mut len = 0u32;
    // SAFETY: buffer size passed matches the buffer.
    unsafe {
        GetTokenInformation(*token, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), size_of_val(&buf) as u32, &mut len)
    }
    .ok()?;
    // SAFETY: GetTokenInformation(TokenIntegrityLevel) filled a TOKEN_MANDATORY_LABEL
    // whose SID lies in `buf`; the integrity RID is its last sub-authority.
    let integrity = unsafe {
        let sid = (*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        *GetSidSubAuthority(sid, u32::from(count.checked_sub(1)?))
    };
    let mut ui_access = 0u32;
    // SAFETY: TokenUIAccess is a DWORD.
    unsafe {
        GetTokenInformation(
            *token,
            TokenUIAccess,
            Some((&mut ui_access as *mut u32).cast()),
            size_of::<u32>() as u32,
            &mut len,
        )
    }
    .ok()?;
    Some(Level { integrity, ui_access: ui_access != 0 })
}

fn image_name(process: HANDLE) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: buffer length passed in `len`.
    unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(path.rsplit('\\').next().unwrap_or(&path).to_string())
}
