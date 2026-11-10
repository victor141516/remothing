//! Zero-copy D3D11 encoder input (Windows). FFmpeg gets a D3D11VA device
//! context wrapping the capture device itself, so the encoder input texture
//! lives on the same GPU device as the captured desktop: each frame only the
//! changed regions are copied GPU-to-GPU, and NVENC/AMF read that texture
//! directly.

use std::ffi::{c_int, c_void};
use std::ptr;

use anyhow::{bail, Context, Result};
use ff::ffi;
use ffmpeg_next as ff;
use gpu_interop::d3d11::D3D11Frame;
use parking_lot::lock_api::RawMutex as _;
use remothing_core::geom::Rect;
use remothing_core::platform::GpuSurface;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
};

/// Leading fields of FFmpeg's AVD3D11VADeviceContext (hwcontext_d3d11va.h).
/// Mirrored rather than taken from the bindings, which only exist when bindgen
/// found the Windows SDK headers; the struct only grows at its end.
#[repr(C)]
struct AvD3D11VaDeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
}

/// Leading fields of FFmpeg's AVD3D11VAFramesContext.
#[repr(C)]
struct AvD3D11VaFramesContext {
    texture: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
}

/// FFmpeg's device-context lock: the capture frame's `context_lock`, so FFmpeg
/// and the capture thread never use the shared immediate context concurrently.
unsafe extern "C" fn lock(ctx: *mut c_void) {
    (*(ctx as *const parking_lot::RawMutex)).lock();
}

unsafe extern "C" fn unlock(ctx: *mut c_void) {
    (*(ctx as *const parking_lot::RawMutex)).unlock();
}

pub struct D3D11Input {
    device: *mut ffi::AVBufferRef,
    frames: *mut ffi::AVBufferRef,
    /// The one persistent encoder input frame (data[0] = texture, data[1] = array index).
    frame: ff::frame::Video,
    /// Pool texture behind `frame`.
    texture: ID3D11Texture2D,
    subresource: u32,
    width: u32,
    height: u32,
    /// Keeps the capture frame alive: FFmpeg's lock callbacks point at its
    /// `context_lock` and the device context references its device. Dropped last.
    surface: GpuSurface,
}

// SAFETY: the FFmpeg contexts and COM pointers are only used by the owning
// encoder thread; shared immediate-context use is serialized by `context_lock`.
unsafe impl Send for D3D11Input {}

impl D3D11Input {
    pub fn new(surface: &GpuSurface, width: u32, height: u32) -> Result<Self> {
        let capture = surface.downcast_ref::<D3D11Frame>().context("no D3D11 capture surface")?;
        unsafe {
            let mut device = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if device.is_null() {
                bail!("av_hwdevice_ctx_alloc(d3d11va) failed");
            }
            let dc = &mut *((*device).data as *mut ffi::AVHWDeviceContext);
            let hw = &mut *(dc.hwctx as *mut AvD3D11VaDeviceContext);
            // FFmpeg releases these on free, so it gets its own references.
            hw.device = capture.device.clone().into_raw();
            hw.device_context = capture.context.clone().into_raw();
            hw.lock = Some(lock);
            hw.unlock = Some(unlock);
            hw.lock_ctx = &capture.context_lock as *const parking_lot::RawMutex as *mut c_void;
            let r = ffi::av_hwdevice_ctx_init(device);
            if r < 0 {
                ffi::av_buffer_unref(&mut device);
                bail!("av_hwdevice_ctx_init(d3d11va) failed: {}", ff::Error::from(r));
            }

            let mut frames = ffi::av_hwframe_ctx_alloc(device);
            if frames.is_null() {
                ffi::av_buffer_unref(&mut device);
                bail!("av_hwframe_ctx_alloc failed");
            }
            let fc = &mut *((*frames).data as *mut ffi::AVHWFramesContext);
            fc.format = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
            fc.sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA;
            fc.width = width as c_int;
            fc.height = height as c_int;
            // A single persistent texture, updated in place with dirty regions.
            fc.initial_pool_size = 1;
            let fh = &mut *(fc.hwctx as *mut AvD3D11VaFramesContext);
            // AMF converts BGRA to NV12 with a shader (needs SRV); render
            // target lets NVENC/AMF register the texture as an input resource.
            fh.bind_flags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
            let r = ffi::av_hwframe_ctx_init(frames);
            if r < 0 {
                ffi::av_buffer_unref(&mut frames);
                ffi::av_buffer_unref(&mut device);
                bail!("av_hwframe_ctx_init(d3d11 bgra) failed: {}", ff::Error::from(r));
            }

            let mut frame = ff::frame::Video::empty();
            let r = ffi::av_hwframe_get_buffer(frames, frame.as_mut_ptr(), 0);
            if r < 0 {
                ffi::av_buffer_unref(&mut frames);
                ffi::av_buffer_unref(&mut device);
                bail!("av_hwframe_get_buffer failed: {}", ff::Error::from(r));
            }
            let f = &*frame.as_ptr();
            let raw = f.data[0] as *mut c_void;
            let texture = ID3D11Texture2D::from_raw_borrowed(&raw)
                .cloned()
                .context("D3D11 frame without texture")?;
            // Array slice of a single-mip texture == subresource index.
            let subresource = f.data[1] as usize as u32;
            Ok(Self { device, frames, frame, texture, subresource, width, height, surface: surface.clone() })
        }
    }

    /// New reference to the frames context, for `AVCodecContext.hw_frames_ctx`.
    pub fn frames_ref(&self) -> *mut ffi::AVBufferRef {
        unsafe { ffi::av_buffer_ref(self.frames) }
    }

    /// Copies `rects` of the capture texture into the encoder input texture.
    pub fn copy(&mut self, capture: &D3D11Frame, rects: &[Rect]) -> Result<()> {
        // The pool texture belongs to the device of the surface bound at
        // creation; a different capture surface needs a new encoder.
        if !ptr::eq(capture as *const D3D11Frame as *const (), std::sync::Arc::as_ptr(&self.surface) as *const ()) {
            bail!("capture surface changed; D3D11 encoder input must be recreated");
        }
        let (w, h) = (self.width.min(capture.width), self.height.min(capture.height));
        capture.with_context(|ctx| {
            for r in rects.iter().filter_map(|r| r.clip(w, h)) {
                let b = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
                unsafe {
                    ctx.CopySubresourceRegion(&self.texture, self.subresource, r.x, r.y, 0, &capture.texture, 0, Some(&b))
                };
            }
        });
        Ok(())
    }

    /// The encoder input frame.
    pub fn frame(&mut self) -> &mut ff::frame::Video {
        &mut self.frame
    }
}

impl Drop for D3D11Input {
    fn drop(&mut self) {
        // Release every FFmpeg reference before `surface` (whose lock FFmpeg's
        // callbacks use) can go away.
        unsafe {
            ffi::av_frame_unref(self.frame.as_mut_ptr());
            ffi::av_buffer_unref(&mut self.frames);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}
