//! Asks Windows to favour the server while it streams, as Sunshine does
//! (task 003, candidates 1, 2 and 4). Measured on this PC: with every CPU
//! busy the server stalled 70-140 ms many times a second until its process
//! class was high; with the GPU busy, encoding took three times as long
//! until its GPU scheduling class was realtime (thread priorities and the
//! high GPU class changed nothing).
//!
//! - Threads: capture at the highest priority, the rest of the stream above
//!   normal.
//! - While clients are connected: high process class, realtime GPU
//!   scheduling class, a 1 ms system timer (the producer threads wait with
//!   it), and the desktop compositor scheduled as multimedia (MMCSS). All
//!   reverted when the last client leaves.
//!
//! The realtime GPU class is set whether or not hardware-accelerated GPU
//! scheduling (HAGS) is on, as the user decided; Sunshine reports NVIDIA
//! driver freezes with it under HAGS (it falls back to "high" there).

use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Dwm::DwmEnableMMCSS;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod, TIMERR_NOERROR};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, SE_INC_BASE_PRIORITY_NAME, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, SetPriorityClass, SetThreadPriority, HIGH_PRIORITY_CLASS,
    NORMAL_PRIORITY_CLASS, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST,
};

use remothing_core::platform::ThreadRole;

/// D3DKMT_SCHEDULINGPRIORITYCLASS values (d3dkmthk.h).
const GPU_CLASS_NORMAL: i32 = 2;
const GPU_CLASS_REALTIME: i32 = 5;
const TIMER_MS: u32 = 1;

#[link(name = "gdi32")]
extern "system" {
    /// Both return an NTSTATUS (0 = success).
    fn D3DKMTSetProcessSchedulingPriorityClass(process: HANDLE, class: i32) -> i32;
    fn D3DKMTGetProcessSchedulingPriorityClass(process: HANDLE, class: *mut i32) -> i32;
}

pub fn thread_priority(role: ThreadRole) {
    let priority = match role {
        ThreadRole::Capture => THREAD_PRIORITY_HIGHEST,
        ThreadRole::Stream => THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // SAFETY: the pseudo-handle of the calling thread.
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), priority) } {
        eprintln!("thread priority {role:?}: {e}");
    }
}

/// See the module comment. Returns one line per setting.
pub fn streaming(active: bool) -> Vec<String> {
    let mut done = Vec::new();
    let on = |what: &str, r: Result<(), String>| match r {
        Ok(()) => format!("{what}: ok"),
        Err(e) => format!("{what}: failed ({e})"),
    };
    // SAFETY: pseudo-handle of this process.
    let process = unsafe { GetCurrentProcess() };
    let class = if active { HIGH_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS };
    // SAFETY: plain process setting.
    let r = unsafe { SetPriorityClass(process, class) }.map_err(|e| e.to_string());
    done.push(on(if active { "process class high" } else { "process class normal" }, r));

    if active {
        // Raising the GPU class needs this privilege; without it the call may still succeed.
        if let Err(e) = enable_privilege() {
            done.push(format!("privilege to raise priorities: not enabled ({e})"));
        }
    }
    let gpu = if active { GPU_CLASS_REALTIME } else { GPU_CLASS_NORMAL };
    let mut now = -1;
    // SAFETY: plain process setting through gdi32; `now` is a valid out pointer.
    let status = unsafe {
        let set = D3DKMTSetProcessSchedulingPriorityClass(process, gpu);
        D3DKMTGetProcessSchedulingPriorityClass(process, &mut now);
        set
    };
    // Read back: the call can succeed without the class changing.
    let r = match (status, now == gpu) {
        (0, true) => Ok(()),
        (0, false) => Err(format!("accepted, but the class is {now}")),
        _ => Err(format!("NTSTATUS {status:#x}")),
    };
    done.push(on(if active { "GPU scheduling class realtime" } else { "GPU scheduling class normal" }, r));

    // SAFETY: plain system calls; every Begin is matched by the End below.
    let timer = unsafe { if active { timeBeginPeriod(TIMER_MS) } else { timeEndPeriod(TIMER_MS) } };
    let r = if timer == TIMERR_NOERROR { Ok(()) } else { Err(format!("error {timer}")) };
    done.push(on(if active { "timer resolution 1 ms" } else { "timer resolution released" }, r));

    // SAFETY: plain system call.
    let r = unsafe { DwmEnableMMCSS(active) }.map_err(|e| e.to_string());
    done.push(on(if active { "compositor as multimedia (MMCSS)" } else { "compositor MMCSS off" }, r));
    done
}

fn enable_privilege() -> windows::core::Result<()> {
    let mut token = HANDLE::default();
    // SAFETY: plain token calls on this process; the handle is closed below.
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)?;
        let mut luid = LUID::default();
        let looked = LookupPrivilegeValueW(None, SE_INC_BASE_PRIORITY_NAME, &mut luid);
        let mut privileges = TOKEN_PRIVILEGES { PrivilegeCount: 1, ..Default::default() };
        privileges.Privileges[0].Luid = luid;
        privileges.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        let adjusted = looked.and_then(|()| AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None));
        let _ = CloseHandle(token);
        adjusted
    }
}
