//! One FFmpeg H.264 encoder instance configured for minimum latency:
//! no B-frames, no lookahead, keyframes only on demand, output per input frame.

use anyhow::{anyhow, Context, Result};
use ff::format::Pixel;
use ff::software::scaling;
use ffmpeg_next as ff;
use remothing_core::encode::{Encoded, VideoEncoder};
use remothing_core::geom::Rect;
use remothing_core::platform::GpuSurface;

use crate::Backend;

/// Where the encoder reads its input from.
pub(crate) enum Input {
    /// Persistent CUDA surface updated with changed regions (Linux, NVENC).
    /// Scaled output goes through system memory first: the scaled picture
    /// is uploaded whole.
    #[cfg(target_os = "linux")]
    Cuda(crate::cuda::CudaSurface, Option<HostInput>),
    /// Persistent D3D11 texture updated GPU-side from the capture surface,
    /// scaled by the GPU's video processor when smaller (Windows).
    #[cfg(windows)]
    D3D11(crate::d3d11::D3D11Input),
    Host(HostInput),
}

/// System-memory BGRX copy of the display, converted (pixel format, size)
/// when the encoder or a smaller output needs it.
pub(crate) struct HostInput {
    bgrx: ff::frame::Video,
    convert: Option<(scaling::Context, ff::frame::Video)>,
}

impl HostInput {
    fn new((iw, ih): (u32, u32), (ow, oh): (u32, u32), wants: Pixel) -> Result<Self> {
        let convert = if wants == Pixel::BGRZ && (iw, ih) == (ow, oh) {
            None
        } else {
            // Same size: only the format changes, the nearest pixel is exact.
            // Scaled: the fast bilinear filter, since the whole picture is
            // scaled for every frame on the CPU.
            let flags = if (iw, ih) == (ow, oh) { scaling::Flags::POINT } else { scaling::Flags::FAST_BILINEAR };
            let sws = scaling::Context::get(Pixel::BGRZ, iw, ih, wants, ow, oh, flags)?;
            Some((sws, ff::frame::Video::new(wants, ow, oh)))
        };
        Ok(Self { bgrx: ff::frame::Video::new(Pixel::BGRZ, iw, ih), convert })
    }

    fn load(&mut self, src: &[u8], stride: usize, src_y0: u32, rects: &[Rect]) {
        let dst_stride = self.bgrx.stride(0);
        let dst = self.bgrx.data_mut(0);
        for r in rects {
            let (xb, wb) = (r.x as usize * 4, r.w as usize * 4);
            for row in r.y as usize..(r.y + r.h) as usize {
                let s = (row - src_y0 as usize) * stride + xb;
                dst[row * dst_stride + xb..row * dst_stride + xb + wb].copy_from_slice(&src[s..s + wb]);
            }
        }
    }

    /// The picture to encode.
    fn frame(&mut self) -> Result<&mut ff::frame::Video> {
        Ok(match &mut self.convert {
            Some((sws, out)) => {
                sws.run(&self.bgrx, out)?;
                out
            }
            None => &mut self.bgrx,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputKind {
    #[cfg(target_os = "linux")]
    Cuda,
    #[cfg(windows)]
    D3D11,
    Host,
}

impl InputKind {
    fn label(self) -> &'static str {
        match self {
            #[cfg(target_os = "linux")]
            InputKind::Cuda => "cuda",
            #[cfg(windows)]
            InputKind::D3D11 => "d3d11",
            InputKind::Host => "system memory",
        }
    }
}

/// Frame rate NVENC's rate control assumes (it ignores timestamps): the
/// per-frame budget is cap / RC_FPS. Faster capture just costs more bits; the
/// link window then lowers the frame rate instead of the quality.
const RC_FPS: i32 = 60;
/// Bitrate cap range: below the floor pictures fall apart; the ceiling sits
/// within H.264 High level 5.2 and above what QP 20 needs for screen content.
const MIN_BITRATE: u64 = 500_000;
const MAX_BITRATE: u64 = 200_000_000;
/// Quality floor: the QP of the former constant-QP mode, so an unconstrained
/// cap yields the same pictures and no wasted bits.
const MIN_QP: &str = "20";

fn clamp_bitrate(bps: u64) -> u64 {
    bps.clamp(MIN_BITRATE, MAX_BITRATE)
}

/// VBV buffer: two frames at RC_FPS, so no single frame exceeds ~33 ms of link time.
fn vbv_size(bps: u64) -> i32 {
    (bps * 2 / RC_FPS as u64).min(i32::MAX as u64) as i32
}

pub(crate) struct FfmpegEncoder {
    enc: ff::encoder::video::Encoder,
    input: Input,
    packet: ff::Packet,
    pts: i64,
    started: std::time::Instant,
    bitrate: u64,
    backend: Backend,
    kind: InputKind,
    pub(crate) fallback: Option<String>,
}

// SAFETY: FFmpeg codec/scaler contexts may move between threads; each encoder
// is used by exactly one client thread at a time.
unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    pub(crate) fn open(
        backend: Backend,
        kind: InputKind,
        input: (u32, u32),
        output: (u32, u32),
        surface: Option<&GpuSurface>,
        max_bitrate: u64,
        overrides: &[(String, String)],
    ) -> Result<Self> {
        let (width, height) = output;
        let name = backend.ffmpeg_name();
        let codec = ff::encoder::find_by_name(name).ok_or_else(|| anyhow!("{name} not compiled into FFmpeg"))?;
        let mut enc = ff::codec::context::Context::new_with_codec(codec).encoder().video()?;
        enc.set_width(width);
        enc.set_height(height);
        // Microsecond timestamps of the actual capture pace: x264's VBV follows them.
        enc.set_time_base(ff::Rational::new(1, 1_000_000));
        enc.set_frame_rate(Some(ff::Rational::new(RC_FPS, 1)));
        enc.set_max_b_frames(0);
        // Keyframes only on demand (new client / decoder error).
        enc.set_gop(100_000);
        enc.set_colorspace(ff::color::Space::BT709);
        enc.set_color_range(ff::color::Range::MPEG);
        enc.set_flags(ff::codec::Flags::LOW_DELAY);
        let bitrate = clamp_bitrate(max_bitrate);
        if backend.caps_bitrate() {
            enc.set_max_bit_rate(bitrate as usize);
            // SAFETY: plain field of the not-yet-opened codec context.
            unsafe { (*enc.as_mut_ptr()).rc_buffer_size = vbv_size(bitrate) };
            if backend == Backend::Nvenc {
                enc.set_bit_rate(bitrate as usize);
            }
        }

        let input = match kind {
            #[cfg(target_os = "linux")]
            InputKind::Cuda => {
                let s = crate::cuda::CudaSurface::new(width, height)?;
                enc.set_format(Pixel::CUDA);
                unsafe { (*enc.as_mut_ptr()).hw_frames_ctx = s.frames_ref() };
                let scaler = (input != output).then(|| HostInput::new(input, output, Pixel::BGRZ)).transpose()?;
                Input::Cuda(s, scaler)
            }
            #[cfg(windows)]
            InputKind::D3D11 => {
                let i = crate::d3d11::D3D11Input::new(surface.context("no capture surface")?, input, output)?;
                enc.set_format(Pixel::D3D11);
                unsafe { (*enc.as_mut_ptr()).hw_frames_ctx = i.frames_ref() };
                Input::D3D11(i)
            }
            InputKind::Host => {
                let wants = backend.host_format();
                enc.set_format(wants);
                Input::Host(HostInput::new(input, output, wants)?)
            }
        };
        let _ = surface;

        let mut opts = ff::Dictionary::new();
        // High profile: required for the constraint_set3 no-reorder signal (see mark_no_reorder).
        opts.set("profile", "high");
        match backend {
            Backend::Nvenc => {
                // Measured (examples/d3d11_bench, 3168x1440, RTX 4080 S): p1/ull
                // run at the encoder's macroblock throughput (~3 ms per frame,
                // static or moving); p2/ll/refs/surfaces/explicit-off AQ,
                // lookahead, multipass change nothing, p3+ cost more.
                // VBR capped at the link's share with a QP floor: constant
                // quality while the link allows, bounded frame sizes otherwise.
                opts.set("preset", "p1");
                opts.set("tune", "ull");
                opts.set("zerolatency", "1");
                opts.set("delay", "0");
                opts.set("rc", "vbr");
                opts.set("qmin", MIN_QP);
                opts.set("forced-idr", "1");
            }
            Backend::Amf => {
                opts.set("usage", "ultralowlatency");
                opts.set("quality", "speed");
                opts.set("rc", "cqp");
                opts.set("qp_i", "20");
                opts.set("qp_p", "20");
                opts.set("header_spacing", "0");
            }
            Backend::Qsv => {
                opts.set("preset", "veryfast");
                opts.set("async_depth", "1");
                opts.set("low_power", "1");
                opts.set("forced_idr", "1");
                opts.set("look_ahead", "0");
            }
            Backend::X264 => {
                // Capped CRF: constant quality within the VBV limits.
                opts.set("preset", "ultrafast");
                opts.set("tune", "zerolatency");
                opts.set("crf", MIN_QP);
                opts.set("forced-idr", "1");
            }
        }
        for (k, v) in overrides {
            opts.set(k, v);
        }
        let enc = enc.open_as_with(codec, opts).with_context(|| format!("open {name}"))?;
        Ok(Self {
            enc,
            input,
            packet: ff::Packet::empty(),
            pts: -1,
            started: std::time::Instant::now(),
            bitrate,
            backend,
            kind,
            fallback: None,
        })
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn name(&self) -> String {
        format!("{} ({})", self.backend.name(), self.kind.label())
    }

    fn fallback_reason(&self) -> Option<String> {
        self.fallback.clone()
    }

    fn gpu_input(&self) -> bool {
        #[cfg(windows)]
        if matches!(self.input, Input::D3D11(_)) {
            return true;
        }
        false
    }

    fn load_cpu(&mut self, src: &[u8], stride: usize, src_y0: u32, rects: &[Rect]) -> Result<()> {
        match &mut self.input {
            Input::Host(h) => {
                h.load(src, stride, src_y0, rects);
                Ok(())
            }
            #[cfg(target_os = "linux")]
            Input::Cuda(_, Some(h)) => {
                h.load(src, stride, src_y0, rects);
                Ok(())
            }
            #[cfg(target_os = "linux")]
            Input::Cuda(s, None) => rects.iter().try_for_each(|r| s.upload(src, stride, src_y0, *r)),
            #[cfg(windows)]
            Input::D3D11(_) => Err(anyhow!("D3D11 encoder input is fed from the capture surface")),
        }
    }

    fn load_gpu(&mut self, surface: &GpuSurface, rects: &[Rect]) -> Result<()> {
        match &mut self.input {
            #[cfg(windows)]
            Input::D3D11(i) => {
                let frame = surface
                    .downcast_ref::<gpu_interop::d3d11::D3D11Frame>()
                    .context("capture surface is not D3D11")?;
                i.copy(frame, rects)
            }
            _ => {
                let _ = (surface, rects);
                Err(anyhow!("encoder input is not GPU-based"))
            }
        }
    }

    fn encode(&mut self, force_key: bool) -> Result<Encoded> {
        // Strictly increasing even if two frames share a microsecond.
        self.pts = (self.started.elapsed().as_micros() as i64).max(self.pts + 1);
        let frame = match &mut self.input {
            #[cfg(target_os = "linux")]
            Input::Cuda(s, None) => &mut s.frame,
            #[cfg(target_os = "linux")]
            Input::Cuda(s, Some(h)) => {
                let f = h.frame()?;
                let whole = Rect::new(0, 0, f.width(), f.height());
                s.upload(f.data(0), f.stride(0), 0, whole)?;
                &mut s.frame
            }
            #[cfg(windows)]
            Input::D3D11(i) => i.frame()?,
            Input::Host(h) => h.frame()?,
        };
        frame.set_pts(Some(self.pts));
        frame.set_kind(if force_key { ff::picture::Type::I } else { ff::picture::Type::None });
        self.enc.send_frame(frame)?;
        let mut out = Encoded { data: Vec::new(), key: false };
        while self.enc.receive_packet(&mut self.packet).is_ok() {
            if let Some(d) = self.packet.data() {
                out.data.extend_from_slice(d);
            }
            out.key |= self.packet.is_key();
        }
        if out.key {
            mark_no_reorder(&mut out.data);
        }
        Ok(out)
    }

    fn set_max_bitrate(&mut self, bits_per_sec: u64) -> bool {
        if !self.backend.caps_bitrate() {
            return false;
        }
        let bitrate = clamp_bitrate(bits_per_sec);
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            // The wrappers pick changed rates up on the next frame (x264
            // reconfigures in place; NVENC through FFmpeg resets with an IDR,
            // which is why callers change the cap sparingly).
            // SAFETY: plain fields of the codec context this thread owns.
            unsafe {
                let ctx = self.enc.as_mut_ptr();
                (*ctx).rc_max_rate = bitrate as i64;
                (*ctx).rc_buffer_size = vbv_size(bitrate);
                if self.backend == Backend::Nvenc {
                    (*ctx).bit_rate = bitrate as i64;
                }
            }
        }
        true
    }
}

/// Whether `codec` (or every encoder, via AVCodecContext) has option `name`.
pub(crate) fn has_option(codec: ff::Codec, name: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(name) else { return false };
    let find = |class: *const ff::ffi::AVClass| {
        // SAFETY: AV_OPT_SEARCH_FAKE_OBJ takes a pointer to a class pointer.
        !class.is_null()
            && unsafe {
                !ff::ffi::av_opt_find(
                    &class as *const _ as *mut std::ffi::c_void,
                    name.as_ptr(),
                    std::ptr::null(),
                    0,
                    ff::ffi::AV_OPT_SEARCH_FAKE_OBJ,
                )
                .is_null()
            }
    };
    // SAFETY: plain getters of static class descriptions.
    unsafe { find((*codec.as_ptr()).priv_class) || find(ff::ffi::avcodec_get_class()) }
}

/// Sets constraint_set3_flag in every SPS. For High profile without VUI
/// bitstream restrictions the spec then infers max_num_reorder_frames = 0, so
/// decoders (Chromium's included) output each picture as soon as it is decoded
/// instead of holding it in the DPB waiting for possible reordering.
fn mark_no_reorder(annexb: &mut [u8]) {
    let mut i = 0;
    while i + 4 < annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
            let nal = i + 3;
            if annexb[nal] & 0x1f == 7 && nal + 2 < annexb.len() {
                // [nal header][profile_idc][constraint flags][level]; profile_idc != 0
                // so the flag byte cannot form or break an emulation-prevention pattern.
                annexb[nal + 2] |= 0x10;
            }
            i = nal;
        } else {
            i += 1;
        }
    }
}
