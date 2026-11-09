//! Low-latency H.264 encoder for the lossy (moving) screen layer.
//!
//! Preferred path: NVENC reading a persistent CUDA surface that only receives
//! the changed screen regions each frame. Fallbacks: NVENC from host memory,
//! then libx264 on the CPU (needs a YUV420 conversion).

use anyhow::{anyhow, Context, Result};
use ff::format::Pixel;
use ff::software::scaling;
use ffmpeg_next as ff;

use crate::cuda::CudaSurface;

enum Input {
    Cuda(CudaSurface),
    Host {
        bgrx: ff::frame::Video,
        /// BGRX -> YUV420 for encoders without RGB input.
        convert: Option<(scaling::Context, ff::frame::Video)>,
    },
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    NvencCuda,
    NvencHost,
    X264,
}

pub struct VideoEncoder {
    enc: ff::encoder::video::Encoder,
    input: Input,
    packet: ff::Packet,
    pts: i64,
}

pub struct Encoded {
    pub data: Vec<u8>,
    pub key: bool,
}

impl VideoEncoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        ff::init()?;
        ff::util::log::set_level(ff::util::log::Level::Error);
        let mut errors = Vec::new();
        for kind in [Kind::NvencCuda, Kind::NvencHost, Kind::X264] {
            match Self::open(kind, width, height) {
                Ok(e) => {
                    eprintln!("video encoder: {kind:?} {width}x{height}");
                    return Ok(e);
                }
                Err(e) => errors.push(format!("{kind:?}: {e:#}")),
            }
        }
        Err(anyhow!("no H.264 encoder available: {}", errors.join("; ")))
    }

    fn open(kind: Kind, width: u32, height: u32) -> Result<Self> {
        let name = match kind {
            Kind::NvencCuda | Kind::NvencHost => "h264_nvenc",
            Kind::X264 => "libx264",
        };
        let codec = ff::encoder::find_by_name(name).ok_or_else(|| anyhow!("{name} not compiled in"))?;
        let mut enc = ff::codec::context::Context::new_with_codec(codec).encoder().video()?;
        enc.set_width(width);
        enc.set_height(height);
        enc.set_time_base(ff::Rational::new(1, 1000));
        enc.set_frame_rate(Some(ff::Rational::new(240, 1)));
        enc.set_max_b_frames(0);
        // Keyframes only on demand (new client / decoder error).
        enc.set_gop(100_000);
        enc.set_colorspace(ff::color::Space::BT709);
        enc.set_color_range(ff::color::Range::MPEG);
        enc.set_flags(ff::codec::Flags::LOW_DELAY);

        let input = match kind {
            Kind::NvencCuda => {
                let surface = CudaSurface::new(width, height)?;
                enc.set_format(Pixel::CUDA);
                unsafe { (*enc.as_mut_ptr()).hw_frames_ctx = surface.frames_ref() };
                Input::Cuda(surface)
            }
            Kind::NvencHost => {
                enc.set_format(Pixel::BGRZ);
                Input::Host { bgrx: ff::frame::Video::new(Pixel::BGRZ, width, height), convert: None }
            }
            Kind::X264 => {
                enc.set_format(Pixel::YUV420P);
                let sws = scaling::Context::get(
                    Pixel::BGRZ,
                    width,
                    height,
                    Pixel::YUV420P,
                    width,
                    height,
                    scaling::Flags::POINT,
                )?;
                Input::Host {
                    bgrx: ff::frame::Video::new(Pixel::BGRZ, width, height),
                    convert: Some((sws, ff::frame::Video::new(Pixel::YUV420P, width, height))),
                }
            }
        };

        let mut opts = ff::Dictionary::new();
        // High profile: required for the constraint_set3 no-reorder signal (see mark_no_reorder).
        opts.set("profile", "high");
        opts.set("forced-idr", "1");
        match kind {
            Kind::NvencCuda | Kind::NvencHost => {
                opts.set("preset", "p1");
                opts.set("tune", "ull");
                opts.set("zerolatency", "1");
                opts.set("delay", "0");
                opts.set("rc", "constqp");
                opts.set("qp", "20");
            }
            Kind::X264 => {
                opts.set("preset", "ultrafast");
                opts.set("tune", "zerolatency");
                opts.set("crf", "20");
            }
        }
        let enc = enc.open_as_with(codec, opts).with_context(|| format!("open {name}"))?;
        Ok(Self { enc, input, packet: ff::Packet::empty(), pts: 0 })
    }

    /// Copies a pixel rectangle of a BGRX image (`stride` bytes per row) into
    /// the encoder input. Untouched areas keep their previous contents.
    pub fn load_rect(&mut self, bgrx: &[u8], stride: usize, x: u32, y: u32, w: u32, h: u32) -> Result<()> {
        match &mut self.input {
            Input::Cuda(s) => s.upload(bgrx, stride, x, y, w, h),
            Input::Host { bgrx: frame, .. } => {
                let dst_stride = frame.stride(0);
                let dst = frame.data_mut(0);
                let (xb, wb) = (x as usize * 4, w as usize * 4);
                for row in y as usize..(y + h) as usize {
                    dst[row * dst_stride + xb..row * dst_stride + xb + wb]
                        .copy_from_slice(&bgrx[row * stride + xb..row * stride + xb + wb]);
                }
                Ok(())
            }
        }
    }

    pub fn encode(&mut self, force_key: bool) -> Result<Encoded> {
        self.pts += 1;
        let frame = match &mut self.input {
            Input::Cuda(s) => &mut s.frame,
            Input::Host { bgrx, convert: Some((sws, yuv)) } => {
                sws.run(bgrx, yuv)?;
                yuv
            }
            Input::Host { bgrx, convert: None } => bgrx,
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
