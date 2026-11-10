//! Win32 backend: a borderless topmost popup covering exactly one monitor
//! (physical pixels), drawing into a BGRX DIB section.
//!
//! Presentation (`--present`):
//! - `dxgi` (default): D3D11 flip-model swap chain (FLIP_SEQUENTIAL, 2 buffers)
//!   with Present1(0, ALLOW_TEARING) and dirty rects. Only the changed rects
//!   are uploaded into the back buffer. A window that exactly covers its
//!   monitor is eligible for independent flip, where DWM is bypassed and the
//!   buffer is scanned out immediately (tearing), so the barcode timestamp is
//!   not followed by an extra DWM composition frame. When DWM still composes
//!   (overlapping windows, capture), flip model avoids the redirection-surface
//!   copy that GDI needs. The frame-latency waitable object (max latency 1)
//!   keeps at most one frame queued, so a present never sits behind stale
//!   ones and pointer-driven barcode redraws cannot build a queue.
//! - `gdi`: BitBlt from the DIB section into the window DC + GdiFlush. Always
//!   composed by DWM (one extra frame of latency); kept as a fallback for
//!   sessions without a usable D3D11 device and as a comparison baseline.
//!
//! Pointer motion arrives as WM_MOUSEMOVE with client (= monitor-relative)
//! coordinates; the loop waits with MsgWaitForMultipleObjectsEx(QS_ALLINPUT),
//! so input wakes it immediately, and the barcode is redrawn right away.
//! Raw input (WM_INPUT) is not used: it reports relative deltas for mice,
//! while the barcode needs the absolute position WM_MOUSEMOVE already carries.

use std::cell::Cell;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::{w, Interface, Owned, BOOL, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter, IDXGIFactory2, IDXGIFactory5, IDXGISwapChain2, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING,
    DXGI_PRESENT_PARAMETERS, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint, EnumDisplayMonitors,
    GdiFlush, GetDC, GetMonitorInfoW, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObjectEx, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
    TIMER_ALL_ACCESS,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, LoadCursorW,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassExW, ShowWindow, TranslateMessage,
    CS_OWNDC, IDC_ARROW, MONITORINFOF_PRIMARY, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, SW_SHOWNOACTIVATE,
    WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_MOUSEMOVE, WM_PAINT, WM_QUIT, WNDCLASSEXW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::scenario::{Canvas, Mode, Plasma, BAR_H, BAR_W};
use crate::Args;

pub enum MonitorSel {
    Primary,
    /// Index in EnumDisplayMonitors order.
    Index(usize),
    /// GDI device name, e.g. `\\.\DISPLAY5`.
    Device(String),
}

#[derive(Clone, Copy, PartialEq)]
pub enum PresentKind {
    Dxgi,
    Gdi,
}

struct Monitor {
    handle: HMONITOR,
    rect: RECT,
    device: String,
    primary: bool,
}

fn monitors() -> Result<Vec<Monitor>> {
    unsafe extern "system" fn collect(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        (*(data.0 as *mut Vec<HMONITOR>)).push(h);
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut handles as *mut _ as isize)) }
        .ok()
        .context("EnumDisplayMonitors")?;
    handles
        .into_iter()
        .map(|handle| {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            unsafe { GetMonitorInfoW(handle, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) }
                .ok()
                .context("GetMonitorInfoW")?;
            let len = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
            Ok(Monitor {
                handle,
                rect: mi.monitorInfo.rcMonitor,
                device: String::from_utf16_lossy(&mi.szDevice[..len]),
                primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            })
        })
        .collect()
}

fn select_monitor(sel: &MonitorSel) -> Result<Monitor> {
    let mut all = monitors()?;
    let pos = match sel {
        MonitorSel::Primary => all.iter().position(|m| m.primary),
        MonitorSel::Index(i) => (*i < all.len()).then_some(*i),
        MonitorSel::Device(d) => all.iter().position(|m| m.device.eq_ignore_ascii_case(d)),
    };
    match pos {
        Some(i) => Ok(all.swap_remove(i)),
        None => {
            let list: Vec<String> = all
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let r = m.rect;
                    format!("  {i}: {} {}x{} at {},{}", m.device, r.right - r.left, r.bottom - r.top, r.left, r.top)
                })
                .collect();
            bail!("no such monitor; available:\n{}", list.join("\n"))
        }
    }
}

thread_local! {
    static MARKER: Cell<u16> = const { Cell::new(0) };
    static INPUT: Cell<bool> = const { Cell::new(false) };
    static REPAINT: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            // Low word of lParam is the signed client x; the barcode carries its 16 bits.
            MARKER.set(lp.0 as u16);
            INPUT.set(true);
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            REPAINT.set(true);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Top-down 32 bpp DIB section selected into a memory DC; its bits are the canvas.
struct Dib {
    bitmap: HBITMAP,
    dc: HDC,
    bits: *mut u32,
    len: usize,
}

impl Dib {
    fn new(w: usize, h: usize) -> Result<Self> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
            .context("CreateDIBSection")?;
        let dc = unsafe { CreateCompatibleDC(None) };
        unsafe { SelectObject(dc, bitmap.into()) };
        Ok(Self { bitmap, dc, bits: bits.cast(), len: w * h })
    }

    fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bitmap.into());
        }
    }
}

struct DxgiPresenter {
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain2,
    back: ID3D11Texture2D,
    waitable: Owned<HANDLE>,
    ready: bool,
    present_flags: DXGI_PRESENT,
    stride: usize,
    /// Rects of the previous present. With two flip-sequential buffers the
    /// back buffer last held the frame before that, so it needs both sets.
    prev: Vec<RECT>,
}

impl DxgiPresenter {
    fn new(hwnd: HWND, monitor: HMONITOR, w: usize, h: usize) -> Result<Self> {
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }?;
        // Create the device on the adapter that scans out this monitor so
        // presents never cross adapters.
        let adapter = adapter_for(&factory, monitor);
        let (mut dev, mut ctx): (Option<ID3D11Device>, Option<ID3D11DeviceContext>) = (None, None);
        let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
        unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
        }
        .context("D3D11CreateDevice")?;
        let (dev, ctx) = (dev.context("no device")?, ctx.context("no context")?);

        let mut tearing = BOOL(0);
        let tearing = unsafe {
            factory.cast::<IDXGIFactory5>().and_then(|f5| {
                f5.CheckFeatureSupport(
                    DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                    &mut tearing as *mut BOOL as *mut _,
                    size_of::<BOOL>() as u32,
                )
            })
        }
        .is_ok()
            && tearing.as_bool();
        let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
        if tearing {
            flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
        }
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w as u32,
            Height: h as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags.0 as u32,
            ..Default::default()
        };
        let swap: IDXGISwapChain2 = unsafe { factory.CreateSwapChainForHwnd(&dev, hwnd, &desc, None, None) }
            .context("CreateSwapChainForHwnd")?
            .cast()?;
        unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }?;
        unsafe { swap.SetMaximumFrameLatency(1) }?;
        let waitable = unsafe { Owned::new(swap.GetFrameLatencyWaitableObject()) };
        // The D3D11 runtime renames buffer 0 on every flip, so this always
        // refers to the current back buffer.
        let back: ID3D11Texture2D = unsafe { swap.GetBuffer(0) }?;
        let present_flags = if tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) };
        Ok(Self { ctx, swap, back, waitable, ready: false, present_flags, stride: w, prev: Vec::with_capacity(4) })
    }

    fn tearing(&self) -> bool {
        self.present_flags == DXGI_PRESENT_ALLOW_TEARING
    }

    fn upload(&self, px: &[u32], r: &RECT) {
        let b = D3D11_BOX {
            left: r.left as u32,
            top: r.top as u32,
            front: 0,
            right: r.right as u32,
            bottom: r.bottom as u32,
            back: 1,
        };
        let src = &px[r.top as usize * self.stride + r.left as usize..];
        unsafe {
            self.ctx.UpdateSubresource(&self.back, 0, Some(&b), src.as_ptr().cast(), (self.stride * 4) as u32, 0)
        };
    }

    fn present(&mut self, px: &[u32], rects: &[RECT]) -> Result<()> {
        for r in self.prev.iter().filter(|r| !rects.contains(r)) {
            self.upload(px, r);
        }
        for r in rects {
            self.upload(px, r);
        }
        let params = DXGI_PRESENT_PARAMETERS {
            DirtyRectsCount: rects.len() as u32,
            pDirtyRects: rects.as_ptr() as *mut RECT,
            pScrollRect: std::ptr::null_mut(),
            pScrollOffset: std::ptr::null_mut(),
        };
        unsafe { self.swap.Present1(0, self.present_flags, &params) }.ok().context("Present1")?;
        self.prev.clear();
        self.prev.extend_from_slice(rects);
        self.ready = false;
        Ok(())
    }
}

fn adapter_for(factory: &IDXGIFactory2, monitor: HMONITOR) -> Option<IDXGIAdapter> {
    (0..)
        .map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok())
        .find(|a| {
            (0..).map_while(|j| unsafe { a.EnumOutputs(j) }.ok()).any(|o| {
                unsafe { o.GetDesc() }.is_ok_and(|d| d.Monitor == monitor)
            })
        })
        .and_then(|a| a.cast().ok())
}

struct GdiPresenter {
    window_dc: HDC,
    mem_dc: HDC,
}

impl GdiPresenter {
    fn present(&self, rects: &[RECT]) -> Result<()> {
        for r in rects {
            unsafe {
                BitBlt(
                    self.window_dc,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    Some(self.mem_dc),
                    r.left,
                    r.top,
                    SRCCOPY,
                )
            }
            .context("BitBlt")?;
        }
        // GDI batches calls per thread; flush so the blit is submitted now.
        unsafe { GdiFlush() }.ok().context("GdiFlush")?;
        Ok(())
    }
}

enum Presenter {
    Dxgi(DxgiPresenter),
    Gdi(GdiPresenter),
}

impl Presenter {
    /// Whether a frame can be presented without blocking.
    fn ready(&mut self) -> bool {
        match self {
            Presenter::Dxgi(d) => {
                if !d.ready && unsafe { WaitForSingleObjectEx(*d.waitable, 0, false) } == WAIT_OBJECT_0 {
                    d.ready = true;
                }
                d.ready
            }
            Presenter::Gdi(_) => true,
        }
    }

    /// Object that signals when `ready` would turn true.
    fn ready_handle(&self) -> Option<HANDLE> {
        match self {
            Presenter::Dxgi(d) => Some(*d.waitable),
            Presenter::Gdi(_) => None,
        }
    }

    /// Called when a wait on `ready_handle` was satisfied (and consumed it).
    fn set_ready(&mut self) {
        if let Presenter::Dxgi(d) = self {
            d.ready = true;
        }
    }

    fn present(&mut self, px: &[u32], rects: &[RECT]) -> Result<()> {
        match self {
            Presenter::Dxgi(d) => d.present(px, rects),
            Presenter::Gdi(g) => g.present(rects),
        }
    }
}

fn rect(x: usize, y: usize, w: usize, h: usize) -> RECT {
    RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 }
}

#[derive(Default)]
struct Stats {
    frames: u32,
    draw: Duration,
    /// Scenario drawn -> presenter ready: time spent behind the previous flip.
    wait: Duration,
    present: Duration,
    present_max: Duration,
}

pub fn run(args: Args) -> Result<()> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .context("SetProcessDpiAwarenessContext")?;
    let mon = select_monitor(&args.monitor)?;
    let r = mon.rect;
    let (w, h) = ((r.right - r.left) as usize, (r.bottom - r.top) as usize);

    let hinst = unsafe { GetModuleHandleW(None) }?;
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_OWNDC,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        lpszClassName: w!("rt-testapp"),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        bail!("RegisterClassExW: {}", std::io::Error::last_os_error());
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            w!("rt-testapp"),
            w!("rt-testapp"),
            WS_POPUP,
            r.left,
            r.top,
            w as i32,
            h as i32,
            None,
            None,
            Some(hinst.into()),
            None,
        )
    }
    .context("CreateWindowExW")?;
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }?;
    if (client.right as usize, client.bottom as usize) != (w, h) {
        bail!("window client is {}x{}, monitor is {w}x{h}", client.right, client.bottom);
    }

    let mut dib = Dib::new(w, h)?;
    let mut presenter = match args.present {
        PresentKind::Dxgi => Presenter::Dxgi(DxgiPresenter::new(hwnd, mon.handle, w, h)?),
        PresentKind::Gdi => Presenter::Gdi(GdiPresenter { window_dc: unsafe { GetDC(Some(hwnd)) }, mem_dc: dib.dc }),
    };
    let method = match &presenter {
        Presenter::Dxgi(d) if d.tearing() => "dxgi flip, tearing",
        Presenter::Dxgi(_) => "dxgi flip, no tearing support",
        Presenter::Gdi(_) => "gdi",
    };
    let mut c = Canvas { px: dib.pixels(), w, h };

    // Shown without activation so the user's foreground window keeps focus;
    // clicking the window activates it, after which Esc closes it.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    let timer = unsafe {
        Owned::new(
            CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
                .context("CreateWaitableTimerExW")?,
        )
    };

    let full = rect(0, 0, w, h);
    let bar = rect(0, 0, BAR_W.min(w), BAR_H);
    c.text(0);
    c.barcode(MARKER.get());
    // The first present is unthrottled: the latency object starts signalled.
    while !presenter.ready() {
        unsafe { WaitForSingleObjectEx(presenter.ready_handle().unwrap_or_default(), INFINITE, false) };
    }
    presenter.present(c.px, &[full])?;
    REPAINT.set(false);

    let plasma = Plasma::new();
    let interval = Duration::from_secs_f64(1.0 / args.fps);
    let mut next = Instant::now();
    let mut frame: usize = 0;
    let mut staged: Option<([RECT; 2], usize, Instant)> = None;
    let (mut box_x, mut box_dx) = (0usize, 8isize);
    let mut stats = Stats::default();
    let mut stats_at = Instant::now();
    eprintln!(
        "rt-testapp {w}x{h} on {} at {},{} ({method}) at {} fps",
        mon.device, r.left, r.top, args.fps
    );

    let mut msg = MSG::default();
    'run: loop {
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                break 'run;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let now = Instant::now();
        // The scenario is drawn as soon as the frame is due, while the
        // previous present may still be in flight; only the barcode is
        // stamped once the presenter is ready, right before presenting.
        if staged.is_none() && now >= next {
            next = if now - next > interval { now + interval } else { next + interval };
            frame += 1;
            let mut rects = [bar; 2];
            let n = match args.mode {
                Mode::Idle => 1,
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
                    let x0 = old.min(box_x);
                    rects[1] = rect(x0, by, (old.max(box_x) + 256 - x0).min(w - x0), 256);
                    2
                }
                Mode::Full => {
                    plasma.draw(&mut c, frame);
                    rects[0] = full;
                    1
                }
                Mode::Scroll => {
                    c.text(frame * 3);
                    rects[0] = full;
                    1
                }
            };
            let drawn = Instant::now();
            stats.draw += drawn - now;
            staged = Some((rects, n, drawn));
        }
        let pending = staged.is_some() || INPUT.get() || REPAINT.get();
        if pending && presenter.ready() {
            INPUT.set(false);
            let (mut rects, n, drawn) = match staged.take() {
                Some((rects, n, drawn)) => (rects, n, Some(drawn)),
                None => ([bar; 2], 1, None),
            };
            if REPAINT.replace(false) {
                rects[0] = full;
            }
            c.barcode(MARKER.get());
            let t = Instant::now();
            presenter.present(c.px, &rects[..n])?;
            let p = t.elapsed();
            if let Some(drawn) = drawn {
                stats.frames += 1;
                stats.wait += t - drawn;
                stats.present += p;
                stats.present_max = stats.present_max.max(p);
            }
            if stats_at.elapsed() >= Duration::from_secs(2) {
                let f = stats.frames.max(1) as f64;
                eprintln!(
                    "{:.1} fps, draw {:.2} ms, wait {:.2} ms, present {:.3} ms avg / {:.3} ms max",
                    stats.frames as f64 / stats_at.elapsed().as_secs_f64(),
                    stats.draw.as_secs_f64() * 1e3 / f,
                    stats.wait.as_secs_f64() * 1e3 / f,
                    stats.present.as_secs_f64() * 1e3 / f,
                    stats.present_max.as_secs_f64() * 1e3,
                );
                stats = Stats::default();
                stats_at = Instant::now();
            }
            continue;
        }
        let mut handles = [*timer, HANDLE::default()];
        let mut count = 1;
        if pending {
            if let Some(h) = presenter.ready_handle() {
                handles[1] = h;
                count = 2;
            }
        }
        if staged.is_none() {
            // Negative due time = relative, in 100 ns units.
            let due_in = -(((next - now).as_nanos() / 100).max(1) as i64);
            unsafe { SetWaitableTimer(*timer, &due_in, 0, None, None, false) }.context("SetWaitableTimer")?;
        }
        let woke = unsafe {
            MsgWaitForMultipleObjectsEx(Some(&handles[..count]), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
        if woke.0 == WAIT_OBJECT_0.0 + 1 {
            presenter.set_ready();
        }
    }
    Ok(())
}
