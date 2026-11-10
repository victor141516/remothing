//! Windows providers: DXGI Desktop Duplication capture into a shared D3D11
//! texture, SendInput injection, input-desktop following, DXGI/NVML GPU
//! monitor and the Windows service. The encoder provider is injected by the
//! binary. Built only on Windows.
#![cfg(windows)]

mod cursor;
mod desktop;
mod dxgi;
mod gpu_diff;
mod input;
mod keymap;
mod monitor;
pub mod service;
mod uipi;

use std::sync::Arc;

use anyhow::{Context, Result};
use remothing_core::encode::EncoderProvider;
use remothing_core::platform::{DisplayInfo, DisplayProvider, OpenedDisplay, Platform, SystemMonitor};
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

use crate::dxgi::DxgiSession;
use crate::input::SendInputProvider;
use crate::monitor::WindowsMonitor;

/// Real outputs of every adapter, captured with Desktop Duplication.
pub struct WindowsDisplays;

impl DisplayProvider for WindowsDisplays {
    fn provider_name(&self) -> String {
        "dxgi desktop duplication".into()
    }

    fn list(&self) -> Result<Vec<DisplayInfo>> {
        Ok(dxgi::outputs()?.into_iter().map(|o| o.info).collect())
    }

    fn open(&self, id: &str) -> Result<OpenedDisplay> {
        let out = dxgi::outputs()?.into_iter().find(|o| o.info.id == id).with_context(|| format!("display {id} not found"))?;
        let capture = DxgiSession::open(&out)?;
        let input = Arc::new(SendInputProvider::new(&out.info));
        Ok(OpenedDisplay { info: out.info, capture: Box::new(capture), input })
    }
}

pub struct WindowsPlatform {
    displays: WindowsDisplays,
    encoders: Box<dyn EncoderProvider>,
    monitor: WindowsMonitor,
}

impl WindowsPlatform {
    /// `table` is the `[platform]` config section; Windows has no options there.
    pub fn new(table: &toml::Table, encoders: Box<dyn EncoderProvider>) -> Result<Self> {
        for key in table.keys() {
            eprintln!("[platform] {key}: not used on Windows, ignored");
        }
        set_dpi_awareness();
        Ok(Self { displays: WindowsDisplays, encoders, monitor: WindowsMonitor::new() })
    }
}

/// Per-monitor-v2 awareness makes every coordinate (DXGI desktop rects,
/// GetSystemMetrics, SendInput, GetCursorPos) physical pixels; DuplicateOutput1
/// also requires it.
fn set_dpi_awareness() {
    // SAFETY: process-wide setting with no preconditions.
    if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
        // Access denied = awareness already set (manifest or an earlier call).
        if e.code() != E_ACCESSDENIED && e.code() != windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult() {
            eprintln!("SetProcessDpiAwarenessContext(per-monitor v2): {e}");
        }
    }
}

impl Platform for WindowsPlatform {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn displays(&self) -> &dyn DisplayProvider {
        &self.displays
    }

    fn encoders(&self) -> &dyn EncoderProvider {
        self.encoders.as_ref()
    }

    fn monitor(&self) -> &dyn SystemMonitor {
        &self.monitor
    }
}
