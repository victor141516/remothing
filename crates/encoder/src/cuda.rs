//! Persistent CUDA input surface for NVENC. Only changed screen regions are
//! copied to the GPU each frame; NVENC then reads device memory directly.

use std::ffi::{c_int, c_void, CStr};
use std::ptr;

use anyhow::{bail, Result};
use ffmpeg_next as ff;
use ff::ffi;
use remothing_core::geom::Rect;

type CuResult = c_int;

#[repr(C)]
struct CudaMemcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: u64,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: u64,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

const CU_MEMORYTYPE_HOST: u32 = 1;
const CU_MEMORYTYPE_DEVICE: u32 = 2;

/// Leading fields of ffmpeg's AVCUDADeviceContext (hwcontext_cuda.h).
#[repr(C)]
struct AvCudaDeviceContext {
    cuda_ctx: *mut c_void,
    stream: *mut c_void,
}

struct Api {
    push: unsafe extern "C" fn(*mut c_void) -> CuResult,
    pop: unsafe extern "C" fn(*mut *mut c_void) -> CuResult,
    memcpy2d: unsafe extern "C" fn(*const CudaMemcpy2D) -> CuResult,
}

impl Api {
    fn load() -> Result<Self> {
        unsafe {
            let lib = libc::dlopen(c"libcuda.so.1".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                bail!("libcuda.so.1 not found");
            }
            let sym = |name: &CStr| -> Result<*mut c_void> {
                let p = libc::dlsym(lib, name.as_ptr());
                if p.is_null() {
                    bail!("missing CUDA symbol {name:?}");
                }
                Ok(p)
            };
            Ok(Self {
                push: std::mem::transmute(sym(c"cuCtxPushCurrent_v2")?),
                pop: std::mem::transmute(sym(c"cuCtxPopCurrent_v2")?),
                memcpy2d: std::mem::transmute(sym(c"cuMemcpy2D_v2")?),
            })
        }
    }
}

pub struct CudaSurface {
    api: Api,
    device: *mut ffi::AVBufferRef,
    frames: *mut ffi::AVBufferRef,
    pub frame: ff::frame::Video,
    ctx: *mut c_void,
}

unsafe impl Send for CudaSurface {}

impl CudaSurface {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let api = Api::load()?;
        unsafe {
            let mut device = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create(
                &mut device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                bail!("av_hwdevice_ctx_create(cuda) failed: {}", ff::Error::from(r));
            }
            let frames = ffi::av_hwframe_ctx_alloc(device);
            let fc = &mut *((*frames).data as *mut ffi::AVHWFramesContext);
            fc.format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
            fc.sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGR0;
            fc.width = width as c_int;
            fc.height = height as c_int;
            fc.initial_pool_size = 1;
            let mut s = Self {
                api,
                device,
                frames,
                frame: ff::frame::Video::empty(),
                ctx: ptr::null_mut(),
            };
            let r = ffi::av_hwframe_ctx_init(frames);
            if r < 0 {
                bail!("av_hwframe_ctx_init failed: {}", ff::Error::from(r));
            }
            let r = ffi::av_hwframe_get_buffer(frames, s.frame.as_mut_ptr(), 0);
            if r < 0 {
                bail!("av_hwframe_get_buffer failed: {}", ff::Error::from(r));
            }
            let dc = &*((*device).data as *const ffi::AVHWDeviceContext);
            s.ctx = (*(dc.hwctx as *const AvCudaDeviceContext)).cuda_ctx;
            Ok(s)
        }
    }

    /// New reference to the frames context, for `AVCodecContext.hw_frames_ctx`.
    pub fn frames_ref(&self) -> *mut ffi::AVBufferRef {
        unsafe { ffi::av_buffer_ref(self.frames) }
    }

    /// The CUDA context the surface lives in, for an NVENC session of its own.
    pub fn context(&self) -> *mut c_void {
        self.ctx
    }

    /// Device address and pitch (bytes) of the surface's BGR0 picture.
    pub fn device_ptr(&self) -> (u64, u32) {
        // SAFETY: the frame was allocated in `new` and lives as long as `self`.
        let f = unsafe { &*self.frame.as_ptr() };
        (f.data[0] as u64, f.linesize[0] as u32)
    }

    /// Makes the surface's context current on this thread until the guard drops.
    pub fn current(&self) -> Result<Current> {
        // SAFETY: a valid context, popped by the guard.
        let r = unsafe { (self.api.push)(self.ctx) };
        if r != 0 {
            bail!("cuCtxPushCurrent failed: {r}");
        }
        Ok(Current(self.api.pop))
    }

    /// Copies pixel rect `r` (display coordinates) of a host BGRX image whose
    /// first row is display row `src_y0` into the same place of the surface.
    pub fn upload(&mut self, src: &[u8], src_stride: usize, src_y0: u32, r: Rect) -> Result<()> {
        let Rect { x, y, w, h } = r;
        let (dptr, pitch) = unsafe {
            let f = &*self.frame.as_ptr();
            (f.data[0] as u64, f.linesize[0] as usize)
        };
        let xb = x as usize * 4;
        let copy = CudaMemcpy2D {
            src_x_in_bytes: xb,
            src_y: (y - src_y0) as usize,
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: src.as_ptr() as *const c_void,
            src_device: 0,
            src_array: ptr::null_mut(),
            src_pitch: src_stride,
            dst_x_in_bytes: xb,
            dst_y: y as usize,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_host: ptr::null_mut(),
            dst_device: dptr,
            dst_array: ptr::null_mut(),
            dst_pitch: pitch,
            width_in_bytes: w as usize * 4,
            height: h as usize,
        };
        assert!((y - src_y0 + h) as usize * src_stride <= src.len());
        unsafe {
            let r = (self.api.push)(self.ctx);
            if r != 0 {
                bail!("cuCtxPushCurrent failed: {r}");
            }
            let r = (self.api.memcpy2d)(&copy);
            let mut old = ptr::null_mut();
            (self.api.pop)(&mut old);
            if r != 0 {
                bail!("cuMemcpy2D failed: {r}");
            }
        }
        Ok(())
    }
}

impl Drop for CudaSurface {
    fn drop(&mut self) {
        unsafe {
            ffi::av_buffer_unref(&mut self.frames);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}

/// A CUDA context made current on this thread (`CudaSurface::current`) while it lives.
pub struct Current(unsafe extern "C" fn(*mut *mut c_void) -> CuResult);

impl Drop for Current {
    fn drop(&mut self) {
        let mut old = ptr::null_mut();
        // SAFETY: pushed in `CudaSurface::current` on this thread.
        unsafe { (self.0)(&mut old) };
    }
}
