//! DXGI Desktop Duplication: output enumeration and capture sessions.
//!
//! Each session owns a D3D11 device on the output's adapter and a persistent
//! BGRA texture (the `D3D11Frame` shared with encoders) kept equal to the
//! desktop image by copying only the changed areas of every acquired frame.
//! The DXGI dirty areas are narrowed to the tiles that really changed on the
//! GPU (`gpu_diff`) before they are reported.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use gpu_interop::d3d11::D3D11Frame;
use remothing_core::geom::{MovedRect, Rect};
use remothing_core::platform::{
    CaptureSession, CursorProvider, CursorState, DisplayInfo, DisplayKind, FrameInfo, GpuSurface,
};
use remothing_core::proto;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HANDLE, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_MODE_ROTATION_IDENTITY,
    DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
    TIMER_ALL_ACCESS,
};

use crate::cursor::{self, CursorTracker};
use crate::desktop;
use crate::gpu_diff::{GpuDiff, GpuFence};

/// Minimum delay between re-duplication attempts while the desktop is not capturable.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// Delay between non-blocking AcquireNextFrame polls (see `next_frame`).
const POLL_INTERVAL: Duration = Duration::from_micros(250);

/// One attached output of one adapter.
pub struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub info: DisplayInfo,
}

/// Every output attached to the desktop, on every adapter.
pub fn outputs() -> Result<Vec<Output>> {
    // SAFETY: DXGI enumeration; descriptors are plain data.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let friendly = friendly_names();
        let mut out = Vec::new();
        for a in 0.. {
            let adapter = match factory.EnumAdapters1(a) {
                Ok(adapter) => adapter,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e).context("EnumAdapters1"),
            };
            for o in 0.. {
                let output = match adapter.EnumOutputs(o) {
                    Ok(output) => output,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e).context("EnumOutputs"),
                };
                let desc = output.GetDesc().context("IDXGIOutput::GetDesc")?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let device = wide_str(&desc.DeviceName);
                let r = desc.DesktopCoordinates;
                let name = match friendly.get(device) {
                    Some(f) => format!("{} ({f})", String::from_utf16_lossy(device)),
                    None => String::from_utf16_lossy(device),
                };
                let info = DisplayInfo {
                    id: format!("dxgi:{a}:{o}"),
                    name,
                    width: (r.right - r.left) as u32,
                    height: (r.bottom - r.top) as u32,
                    refresh_hz: refresh_hz(&desc.DeviceName),
                    x: r.left,
                    y: r.top,
                    primary: r.left == 0 && r.top == 0,
                    kind: DisplayKind::Real,
                };
                out.push(Output { adapter: adapter.clone(), output, info });
            }
        }
        Ok(out)
    }
}

/// NUL-terminated UTF-16 buffer → slice without the terminator.
fn wide_str(buf: &[u16]) -> &[u16] {
    &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]
}

fn refresh_hz(device: &[u16; 32]) -> f64 {
    let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    // SAFETY: `device` is NUL-terminated (DXGI_OUTPUT_DESC::DeviceName); `mode.dmSize` is set.
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };
    if ok.as_bool() {
        f64::from(mode.dmDisplayFrequency)
    } else {
        0.0
    }
}

/// GDI device name (`\\.\DISPLAYn`) → monitor friendly name, from the active display paths.
fn friendly_names() -> HashMap<Vec<u16>, String> {
    let mut names = HashMap::new();
    // SAFETY: buffers sized by GetDisplayConfigBufferSizes; request packets carry their own size.
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return names;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
            return names;
        }
        for path in &paths[..np as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 || DisplayConfigGetDeviceInfo(&mut target.header) != 0 {
                continue;
            }
            let friendly = wide_str(&target.monitorFriendlyDeviceName);
            if !friendly.is_empty() {
                names.entry(wide_str(&source.viewGdiDeviceName).to_vec()).or_insert_with(|| String::from_utf16_lossy(friendly));
            }
        }
    }
    names
}

fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let create = |flags: D3D11_CREATE_DEVICE_FLAG| -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let (mut device, mut context) = (None, None);
        // SAFETY: out pointers are valid locals; the adapter outlives the call.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        Ok((device.expect("D3D11CreateDevice returned no device"), context.expect("D3D11CreateDevice returned no context")))
    };
    // Video support lets hardware encoders (QSV in particular) use this device
    // directly; drivers without a video engine still get a capture device.
    let (device, context) = create(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT)
        .or_else(|_| create(D3D11_CREATE_DEVICE_BGRA_SUPPORT))
        .context("D3D11CreateDevice")?;
    // Encoders use the immediate context from their own threads.
    let mt: ID3D11Multithread = context.cast().context("ID3D11Multithread")?;
    // SAFETY: plain setter on a live device.
    let _ = unsafe { mt.SetMultithreadProtected(true) };
    Ok((device, context))
}

/// Errors after which duplication may become possible again without
/// reopening the display (desktop switch, secure desktop, mode change in progress).
fn transient(code: windows::core::HRESULT) -> bool {
    [
        E_ACCESSDENIED,
        DXGI_ERROR_ACCESS_LOST,
        DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
        DXGI_ERROR_SESSION_DISCONNECTED,
        DXGI_ERROR_UNSUPPORTED,
    ]
    .contains(&code)
}

fn duplicate(output: &IDXGIOutput1, device: &ID3D11Device) -> windows::core::Result<IDXGIOutputDuplication> {
    // SAFETY: COM calls on live objects.
    unsafe {
        if let Ok(output5) = output.cast::<IDXGIOutput5>() {
            match output5.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) {
                Ok(d) => return Ok(d),
                Err(e) if e.code() == E_ACCESSDENIED || e.code() == DXGI_ERROR_SESSION_DISCONNECTED => return Err(e),
                // Older drivers/OS paths: fall back to the desktop's own format.
                Err(_) => {}
            }
        }
        output.DuplicateOutput(device)
    }
}

pub struct DxgiSession {
    output: IDXGIOutput1,
    frame: Arc<D3D11Frame>,
    diff: GpuDiff,
    dupl: Option<IDXGIOutputDuplication>,
    /// CPU-readable copy target for `read`, created on first use.
    staging: Option<ID3D11Texture2D>,
    /// Signals when the staging copies are done, so `read` waits without the context lock.
    copied: GpuFence,
    width: u32,
    height: u32,
    /// Next frame copies and reports the whole display (first frame, re-duplication).
    full_pending: bool,
    next_retry: Instant,
    /// Duplication currently lost and not yet re-established (logged once per episode).
    unavailable: bool,
    dirty_buf: Vec<RECT>,
    move_buf: Vec<DXGI_OUTDUPL_MOVE_RECT>,
    shape_buf: Vec<u8>,
    cursor: CursorTracker,
    poll_timer: PollTimer,
}

// SAFETY: D3D11/DXGI objects are free-threaded (the device is multithread
// protected); the session is driven by one thread at a time and all
// immediate-context use goes through `D3D11Frame::context_lock`.
unsafe impl Send for DxgiSession {}

impl DxgiSession {
    pub fn open(out: &Output) -> Result<Self> {
        let output: IDXGIOutput1 = out.output.cast().context("IDXGIOutput1 (Windows 8+ required)")?;
        desktop::follow_input_desktop(true);
        let (device, context) = create_device(&out.adapter)?;
        let dupl = duplicate(&output, &device).context("DuplicateOutput")?;
        let (width, height) = check_mode(&dupl)?;
        if (width, height) != (out.info.width, out.info.height) {
            bail!("duplication is {width}x{height} but the output is {}x{}", out.info.width, out.info.height);
        }
        let texture = create_texture(&device, width, height, D3D11_USAGE_DEFAULT.0, D3D11_BIND_SHADER_RESOURCE.0 as u32, 0)?;
        let frame = Arc::new(D3D11Frame::new(device, context, texture, width, height));
        Ok(Self {
            output,
            diff: GpuDiff::new(&frame).context("GPU tile diff")?,
            copied: GpuFence::new(&frame)?,
            frame,
            dupl: Some(dupl),
            staging: None,
            width,
            height,
            full_pending: true,
            next_retry: Instant::now(),
            unavailable: false,
            dirty_buf: Vec::new(),
            move_buf: Vec::new(),
            shape_buf: Vec::new(),
            cursor: CursorTracker::default(),
            poll_timer: PollTimer::new()?,
        })
    }

    /// Re-duplicates after a desktop switch/mode change. `Ok(false)`: not
    /// possible right now (secure desktop as a normal user, transition in
    /// progress); `Err`: the display must be reopened (size changed, device lost).
    fn reopen(&mut self) -> Result<bool> {
        self.next_retry = Instant::now() + RETRY_INTERVAL;
        desktop::follow_input_desktop(true);
        match duplicate(&self.output, &self.frame.device) {
            Ok(dupl) => {
                let size = check_mode(&dupl)?;
                if size != (self.width, self.height) {
                    bail!("display mode changed from {}x{} to {}x{}", self.width, self.height, size.0, size.1);
                }
                if self.unavailable {
                    eprintln!("desktop duplication resumed");
                    self.unavailable = false;
                }
                self.dupl = Some(dupl);
                self.full_pending = true;
                Ok(true)
            }
            Err(e) if transient(e.code()) => {
                if !self.unavailable {
                    eprintln!("desktop duplication unavailable, retrying every {RETRY_INTERVAL:?}: {e}");
                    self.unavailable = true;
                }
                Ok(false)
            }
            Err(e) => Err(e).context("DuplicateOutput"),
        }
    }

    fn lost(&mut self, why: &windows::core::Error) {
        if !self.unavailable {
            eprintln!("desktop duplication lost (HRESULT {:#010X}), re-duplicating", why.code().0 as u32);
        }
        self.dupl = None;
        self.next_retry = Instant::now();
    }

    /// Copies the changed areas of the acquired image into the persistent
    /// texture and describes them. Runs between Acquire and ReleaseFrame.
    /// `true` with the frame: its dirty areas are still being narrowed on the
    /// GPU and must be collected with [`GpuDiff::collect`].
    fn process(
        &mut self,
        dupl: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
        capture_us: u64,
    ) -> Result<Option<(FrameInfo, bool)>> {
        if info.LastMouseUpdateTime != 0 {
            self.cursor.set_visible(info.PointerPosition.Visible.as_bool());
        }
        if info.PointerShapeBufferSize > 0 {
            self.read_pointer_shape(dupl, info.PointerShapeBufferSize)?;
        }
        // Pointer-only update: the desktop image did not change.
        if info.LastPresentTime == 0 {
            return Ok(None);
        }
        let Some(resource) = resource else { return Ok(None) };
        let image: ID3D11Texture2D = resource.cast().context("desktop image is not a texture")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid out pointer.
        unsafe { image.GetDesc(&mut desc) };
        if (desc.Width, desc.Height) != (self.width, self.height) {
            bail!("display size changed to {}x{}", desc.Width, desc.Height);
        }
        // DuplicateOutput1 converts HDR desktops to BGRA; the legacy fallback does not.
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB {
            bail!("desktop image format {} is not BGRA (HDR desktop without DuplicateOutput1?)", desc.Format.0);
        }

        let (w, h) = (self.width, self.height);
        let mut dirty = Vec::new();
        let mut moved = Vec::new();
        if self.full_pending {
            self.full_pending = false;
            let (frame, diff) = (&self.frame, &self.diff);
            frame.with_context(|ctx| {
                // SAFETY: same size and format family (checked above).
                unsafe { ctx.CopyResource(&frame.texture, &image) };
                diff.reset(ctx, &frame.texture);
            });
            dirty.push(Rect::new(0, 0, w, h));
            return Ok(Some((FrameInfo { capture_us, dirty, moved }, false)));
        }
        if info.TotalMetadataBufferSize == 0 {
            return Ok(None);
        }
        self.read_metadata(dupl, info.TotalMetadataBufferSize as usize)?;
        for m in &self.move_buf {
            let d = m.DestinationRect;
            let Some(dst) = clip(&d, w, h) else { continue };
            let (sx, sy) = (m.SourcePoint.x + (dst.x as i32 - d.left), m.SourcePoint.y + (dst.y as i32 - d.top));
            if sx < 0 || sy < 0 {
                // Source outside the display: report as plain damage.
                dirty.push(dst);
                continue;
            }
            moved.push(MovedRect { src_x: sx as u32, src_y: sy as u32, dst });
        }
        dirty.extend(self.dirty_buf.iter().filter_map(|r| clip(r, w, h)));
        if !self.diff.select(dirty.iter().chain(moved.iter().map(|m| &m.dst))) {
            return Ok(None);
        }
        // The acquired image already is the complete new desktop, so moved
        // areas are copied from their destination like any damage: no
        // overlapping self-copies in the persistent texture.
        let (frame, diff) = (&self.frame, &mut self.diff);
        let compared = frame.with_context(|ctx| {
            for r in dirty.iter().chain(moved.iter().map(|m| &m.dst)) {
                let b = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
                // SAFETY: box clipped to both textures (same size).
                unsafe { ctx.CopySubresourceRegion(&frame.texture, 0, r.x, r.y, 0, &image, 0, Some(&b)) };
            }
            diff.submit(ctx, &frame.texture)
        })?;
        if compared {
            // Replaced by the tiles that really changed; move destinations stay reported through `moved`.
            dirty.clear();
        }
        Ok(Some((FrameInfo { capture_us, dirty, moved }, compared)))
    }

    fn read_metadata(&mut self, dupl: &IDXGIOutputDuplication, total: usize) -> Result<()> {
        // The metadata total bounds both arrays.
        self.move_buf.resize(total / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1, Default::default());
        self.dirty_buf.resize(total / std::mem::size_of::<RECT>() + 1, Default::default());
        let mut used = 0u32;
        // SAFETY: buffers are sized as passed (bytes).
        unsafe {
            dupl.GetFrameMoveRects(
                std::mem::size_of_val(self.move_buf.as_slice()) as u32,
                self.move_buf.as_mut_ptr(),
                &mut used,
            )
            .context("GetFrameMoveRects")?;
            self.move_buf.truncate(used as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>());
            dupl.GetFrameDirtyRects(
                std::mem::size_of_val(self.dirty_buf.as_slice()) as u32,
                self.dirty_buf.as_mut_ptr(),
                &mut used,
            )
            .context("GetFrameDirtyRects")?;
            self.dirty_buf.truncate(used as usize / std::mem::size_of::<RECT>());
        }
        Ok(())
    }

    fn read_pointer_shape(&mut self, dupl: &IDXGIOutputDuplication, size: u32) -> Result<()> {
        self.shape_buf.resize(size as usize, 0);
        let mut used = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: buffer of `size` bytes.
        unsafe { dupl.GetFramePointerShape(size, self.shape_buf.as_mut_ptr().cast(), &mut used, &mut info) }
            .context("GetFramePointerShape")?;
        if let Some(shape) = cursor::convert(&info, &self.shape_buf[..used as usize]) {
            self.cursor.set_shape(shape);
        }
        Ok(())
    }

    fn staging(&mut self) -> Result<ID3D11Texture2D> {
        if let Some(t) = &self.staging {
            return Ok(t.clone());
        }
        let t = create_texture(&self.frame.device, self.width, self.height, D3D11_USAGE_STAGING.0, 0, D3D11_CPU_ACCESS_READ.0 as u32)?;
        self.staging = Some(t.clone());
        Ok(t)
    }
}

/// Validates a fresh duplication and returns its size. The mode format is not
/// checked: it stays the desktop's (FP16 on HDR) even when DuplicateOutput1
/// delivers BGRA images.
fn check_mode(dupl: &IDXGIOutputDuplication) -> Result<(u32, u32)> {
    // SAFETY: plain getter.
    let desc = unsafe { dupl.GetDesc() };
    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
        bail!("rotated displays are not supported (rotation {})", desc.Rotation.0);
    }
    Ok((desc.ModeDesc.Width, desc.ModeDesc.Height))
}

pub(crate) fn create_texture(device: &ID3D11Device, width: u32, height: u32, usage: i32, bind: u32, cpu: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE(usage),
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: valid descriptor and out pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.context("CreateTexture2D")?;
    texture.context("CreateTexture2D returned no texture")
}

/// Desktop RECT → display rect clipped to `w` x `h`.
fn clip(r: &RECT, w: u32, h: u32) -> Option<Rect> {
    let l = r.left.max(0) as u32;
    let t = r.top.max(0) as u32;
    let rr = (r.right.max(0) as u32).min(w);
    let b = (r.bottom.max(0) as u32).min(h);
    (l < rr && t < b).then(|| Rect::new(l, t, rr - l, b - t))
}

/// Sub-millisecond sleeps between AcquireNextFrame polls (`Sleep` rounds up
/// to the system timer resolution, typically 1–15.6 ms).
struct PollTimer(HANDLE);

impl PollTimer {
    fn new() -> Result<Self> {
        // SAFETY: unnamed timer, default security. High-resolution timers need
        // Windows 10 1803+; older systems get a coarser plain timer.
        let timer = unsafe {
            CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
                .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
        }
        .context("CreateWaitableTimerEx")?;
        Ok(Self(timer))
    }

    fn sleep(&self, d: Duration) -> Result<()> {
        // Negative due time: relative, in 100 ns units.
        let due = -((d.as_nanos() / 100).max(1) as i64);
        // SAFETY: timer owned by self; no completion routine.
        unsafe {
            SetWaitableTimer(self.0, &due, 0, None, None, false).context("SetWaitableTimer")?;
            WaitForSingleObject(self.0, INFINITE);
        }
        Ok(())
    }
}

impl Drop for PollTimer {
    fn drop(&mut self) {
        // SAFETY: handle created in `new`, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl CursorProvider for DxgiSession {
    fn cursor_change(&mut self) -> Result<Option<CursorState>> {
        Ok(self.cursor.take_change())
    }
}

impl CaptureSession for DxgiSession {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<FrameInfo>> {
        if self.dupl.is_none() {
            let now = Instant::now();
            if now < self.next_retry {
                std::thread::sleep(timeout.min(self.next_retry - now));
                return Ok(None);
            }
            if !self.reopen()? {
                std::thread::sleep(timeout.min(RETRY_INTERVAL));
                return Ok(None);
            }
        }
        let dupl = self.dupl.clone().expect("duplication present");
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        let deadline = Instant::now() + timeout;
        loop {
            // Never waits inside AcquireNextFrame: it would hold the
            // multithread-protected device lock for the whole wait and stall
            // the encoder's D3D11 calls (NVENC, FFmpeg) on this device.
            // SAFETY: valid out pointers; frames are always released below.
            match unsafe { dupl.AcquireNextFrame(0, &mut info, &mut resource) } {
                Ok(()) => break,
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(None);
                    }
                    self.poll_timer.sleep(POLL_INTERVAL.min(deadline - now))?;
                }
                Err(e) if transient(e.code()) => {
                    self.lost(&e);
                    return Ok(None);
                }
                Err(e) => return Err(e).context("AcquireNextFrame"),
            }
        }
        let capture_us = proto::now_us();
        let result = self.process(&dupl, &info, resource, capture_us);
        // SAFETY: a frame is held (acquired above); GPU copies are already queued.
        if let Err(e) = unsafe { dupl.ReleaseFrame() } {
            if transient(e.code()) {
                self.lost(&e);
            } else {
                return Err(e).context("ReleaseFrame");
            }
        }
        // The tile diff is collected after the release: DXGI can compose the next frame meanwhile.
        let Some((mut frame, pending)) = result? else { return Ok(None) };
        if pending {
            self.diff.collect(&self.frame, &mut frame.dirty)?;
            if frame.dirty.is_empty() && frame.moved.is_empty() {
                return Ok(None);
            }
        }
        Ok(Some(frame))
    }

    fn read(&mut self, rects: &[Rect], dst: &mut [u8], dst_stride: usize) -> Result<()> {
        let (w, h) = (self.width, self.height);
        let clipped = || rects.iter().filter_map(move |r| r.clip(w, h));
        let Some(end) = clipped().map(|r| (r.y + r.h - 1) as usize * dst_stride + (r.x + r.w) as usize * 4).max() else {
            return Ok(());
        };
        if end > dst.len() {
            bail!("read: destination too small ({} < {end} bytes)", dst.len());
        }
        let staging = self.staging()?;
        let (frame, copied) = (&self.frame, &mut self.copied);
        frame.with_context(|ctx| {
            for r in clipped() {
                let b = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
                // SAFETY: rects clipped to the (equal) texture sizes.
                unsafe { ctx.CopySubresourceRegion(&staging, 0, r.x, r.y, 0, &frame.texture, 0, Some(&b)) };
            }
            copied.signal(ctx)
        })?;
        // Wait for the GPU with the context unlocked: encoders keep submitting
        // meanwhile (a blocking Map would hold the lock for the whole copy).
        copied.wait().context("wait for staging copy")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        frame
            // SAFETY: staging texture with CPU read access; the copies are complete.
            .with_context(|ctx| unsafe { ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) })
            .context("map staging texture")?;
        // Rows are copied without the context lock: a mapped resource stays
        // valid while encoders use the context.
        let pitch = mapped.RowPitch as usize;
        let src = mapped.pData as *const u8;
        for r in clipped() {
            let (x, len) = (r.x as usize * 4, r.w as usize * 4);
            for y in r.y as usize..(r.y + r.h) as usize {
                // SAFETY: the mapping covers `height` rows of `pitch` bytes; x+len <= width*4 <= pitch.
                let row = unsafe { std::slice::from_raw_parts(src.add(y * pitch + x), len) };
                dst[y * dst_stride + x..][..len].copy_from_slice(row);
            }
        }
        // SAFETY: mapped above.
        frame.with_context(|ctx| unsafe { ctx.Unmap(&staging, 0) });
        Ok(())
    }

    fn gpu_surface(&self) -> Option<GpuSurface> {
        Some(self.frame.clone())
    }
}
