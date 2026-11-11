//! One FFmpeg video encoder instance configured for minimum latency:
//! no B-frames, no lookahead, keyframes only on demand, output per input frame.

use anyhow::{anyhow, Context, Result};
use ff::format::Pixel;
use ff::software::scaling;
use ffmpeg_next as ff;
use remothing_core::encode::{Codec, Encoded, VideoEncoder};
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

/// Bitrate cap range: below the floor pictures fall apart; the ceiling sits
/// within H.264 High level 5.2 and above what QP 20 needs for screen content.
const MIN_BITRATE: u64 = 500_000;
const MAX_BITRATE: u64 = 200_000_000;
/// Quality floor: the QP of the former constant-QP mode, so an unconstrained
/// cap yields the same pictures and no wasted bits. AV1's scale runs to 255:
/// measured at constant QP on recordings of rt-testapp (`full`, `scroll`),
/// AV1 at 60 matches or beats the PSNR of H.264 at 20 with 40-60 % of the
/// bytes (at 100 it lost 4.7 dB on scrolling text). HEVC at 20 matched
/// H.264 at 20 on `full` and gained 1.9 dB on `scroll`.
fn min_qp(codec: Codec) -> i32 {
    match codec {
        Codec::H264 | Codec::Hevc => 20,
        Codec::Av1 => 60,
    }
}

/// Frame rate NVENC's rate control assumes (it ignores timestamps): a frame
/// gets a sixtieth of the cap it is given, however fast frames come.
const RC_FPS: i32 = 60;

/// The cap as the backend's rate control takes it, for frames coming at up
/// to `fps` a second. NVENC gets it scaled, so that a frame's share is
/// `bps / fps`; x264 follows the timestamps and takes it as it is.
pub(crate) fn rate_control_bitrate(backend: Backend, bps: u64, fps: u32) -> u64 {
    let bps = match backend {
        Backend::Nvenc => (bps / fps.max(1) as u64).saturating_mul(RC_FPS as u64),
        _ => bps,
    };
    bps.clamp(MIN_BITRATE, MAX_BITRATE)
}

/// VBV buffer: two frames at RC_FPS (two frames for NVENC, 33 ms for x264),
/// so no single frame takes much more of the link than its share.
fn vbv_size(bps: u64) -> i32 {
    (bps * 2 / RC_FPS as u64).min(i32::MAX as u64) as i32
}

/// Two-pass at quarter resolution (NVENC `multipass=qres`) when the core
/// asks for it: only for H.264 on NVENC, where it measurably helped on slow
/// and distant links (docs/RESULTS.md, "Two-pass on slow links"); HEVC and
/// AV1 gained nothing measurable.
fn offers_two_pass(backend: Backend, codec: Codec) -> bool {
    backend == Backend::Nvenc && codec == Codec::H264
}

pub(crate) struct FfmpegEncoder {
    enc: ff::encoder::video::Encoder,
    input: Input,
    packet: ff::Packet,
    pts: i64,
    started: std::time::Instant,
    bitrate: u64,
    backend: Backend,
    codec: Codec,
    /// Runs two-pass (see `offers_two_pass`).
    two_pass: bool,
    kind: InputKind,
    pub(crate) fallback: Option<String>,
}

// SAFETY: FFmpeg codec/scaler contexts may move between threads; each encoder
// is used by exactly one client thread at a time.
unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        backend: Backend,
        codec: Codec,
        kind: InputKind,
        input: (u32, u32),
        output: (u32, u32),
        surface: Option<&GpuSurface>,
        bitrate: u64,
        two_pass: bool,
        overrides: &[(String, String)],
    ) -> Result<Self> {
        let (width, height) = output;
        let name = backend.ffmpeg_name(codec).ok_or_else(|| anyhow!("{} has no {} encoder", backend.name(), codec.name()))?;
        let encoder = ff::encoder::find_by_name(name).ok_or_else(|| anyhow!("{name} not compiled into FFmpeg"))?;
        let mut enc = ff::codec::context::Context::new_with_codec(encoder).encoder().video()?;
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
        match codec {
            // Required for the constraint_set3 no-reorder signal (see mark_no_reorder).
            Codec::H264 => opts.set("profile", "high"),
            Codec::Hevc => opts.set("profile", "main"),
            // av1_nvenc has no profile option (FFmpeg's generic one rejects
            // names); Main, 8-bit 4:2:0, is what it encodes.
            Codec::Av1 => {}
        }
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
                // The field, not the option: FFmpeg's generic qmin option
                // stops at 69, NVENC's AV1 floor goes to 255.
                // SAFETY: plain field of the not-yet-opened codec context.
                unsafe { (*enc.as_mut_ptr()).qmin = min_qp(codec) };
                opts.set("forced-idr", "1");
                if two_pass && offers_two_pass(backend, codec) {
                    opts.set("multipass", "qres");
                }
                // Split-frame encoding (HEVC and AV1; FFmpeg 7.1 or later):
                // the GPU's encoders take a strip each. Measured
                // (d3d11_bench, RTX 4080 S, 1080p to 4K): the driver's "auto"
                // splits only at 4K; forced cuts the median encode time by
                // 35-40 % and p99 by 10-25 % at every size, for 1-12 % more
                // bytes.
                if codec != Codec::H264 && has_option(encoder, "split_encode_mode") {
                    opts.set("split_encode_mode", "forced");
                }
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
                opts.set("crf", &min_qp(codec).to_string());
                opts.set("forced-idr", "1");
            }
        }
        for (k, v) in overrides {
            opts.set(k, v);
        }
        let enc = enc.open_as_with(encoder, opts).with_context(|| format!("open {name}"))?;
        Ok(Self {
            enc,
            input,
            packet: ff::Packet::empty(),
            pts: -1,
            started: std::time::Instant::now(),
            bitrate,
            backend,
            codec,
            two_pass: two_pass && offers_two_pass(backend, codec),
            kind,
            fallback: None,
        })
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn name(&self) -> String {
        let pass = if self.two_pass { " two-pass" } else { "" };
        format!("{} {}{pass} ({})", self.backend.name(), self.codec.name(), self.kind.label())
    }

    fn two_pass(&self) -> Option<bool> {
        offers_two_pass(self.backend, self.codec).then_some(self.two_pass)
    }

    fn codec(&self) -> Codec {
        self.codec
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
        // HEVC's parameter sets say how many pictures may be reordered
        // (none here, without B-frames) and AV1 has no output reordering.
        if out.key && self.codec == Codec::H264 {
            mark_no_reorder(&mut out.data);
        }
        Ok(out)
    }

    fn set_max_bitrate(&mut self, bits_per_sec: u64, fps: u32) -> bool {
        if !self.backend.caps_bitrate() {
            return false;
        }
        let bitrate = rate_control_bitrate(self.backend, bits_per_sec, fps);
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
