//! FFmpeg-based `EncoderProvider`: NVENC, AMF, QuickSync and x264 behind one
//! low-latency configuration, with zero-copy GPU input where the platform
//! offers it (CUDA on Linux, D3D11 on Windows) and system memory otherwise.

#[cfg(target_os = "linux")]
mod cuda;
#[cfg(windows)]
mod d3d11;
mod ffmpeg;

use anyhow::{anyhow, Result};
use ffmpeg_next as ff;
use remothing_core::encode::{EncoderProvider, EncoderRequest, VideoEncoder};

use crate::ffmpeg::{FfmpegEncoder, InputKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    Nvenc,
    Amf,
    Qsv,
    X264,
}

const ALL: [Backend; 4] = [Backend::Nvenc, Backend::Amf, Backend::Qsv, Backend::X264];

impl Backend {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Backend::Nvenc => "nvenc",
            Backend::Amf => "amf",
            Backend::Qsv => "qsv",
            Backend::X264 => "x264",
        }
    }

    pub(crate) fn ffmpeg_name(self) -> &'static str {
        match self {
            Backend::Nvenc => "h264_nvenc",
            Backend::Amf => "h264_amf",
            Backend::Qsv => "h264_qsv",
            Backend::X264 => "libx264",
        }
    }

    /// System-memory pixel format the encoder accepts.
    pub(crate) fn host_format(self) -> ff::format::Pixel {
        match self {
            Backend::Nvenc => ff::format::Pixel::BGRZ,
            Backend::Amf | Backend::Qsv => ff::format::Pixel::NV12,
            Backend::X264 => ff::format::Pixel::YUV420P,
        }
    }

    /// Input paths to try, best first.
    fn inputs(self, has_surface: bool) -> Vec<InputKind> {
        let mut v = Vec::new();
        #[cfg(target_os = "linux")]
        if self == Backend::Nvenc {
            v.push(InputKind::Cuda);
        }
        // h264_qsv only takes NV12/QSV surfaces (FFmpeg rejects D3D11 frames),
        // so QuickSync uses system-memory NV12; x264 is CPU-only.
        #[cfg(windows)]
        if has_surface && matches!(self, Backend::Nvenc | Backend::Amf) {
            v.push(InputKind::D3D11);
        }
        let _ = has_surface;
        v.push(InputKind::Host);
        v
    }
}

pub struct FfmpegEncoders {
    available: Vec<Backend>,
    /// Compiled-in backends that failed the startup probe, with the reason.
    unavailable: Vec<(String, String)>,
    /// Option overrides applied over a backend's defaults.
    overrides: Vec<(Backend, Vec<(String, String)>)>,
}

impl FfmpegEncoders {
    /// Probes every compiled-in backend once (small host-memory encoder), so
    /// status and fallback choices reflect what this machine can actually run.
    pub fn new() -> Result<Self> {
        ff::init()?;
        ff::util::log::set_level(ff::util::log::Level::Error);
        let mut available = Vec::new();
        let mut unavailable = Vec::new();
        for b in ALL {
            if ff::encoder::find_by_name(b.ffmpeg_name()).is_none() {
                continue;
            }
            match FfmpegEncoder::open(b, InputKind::Host, 256, 256, None, &[]) {
                Ok(_) => available.push(b),
                Err(e) => unavailable.push((b.name().to_string(), format!("{e:#}"))),
            }
        }
        Ok(Self { available, unavailable, overrides: Vec::new() })
    }

    /// Replaces the option overrides of `backend` ("nvenc", "amf", "qsv",
    /// "x264") for encoders created afterwards: FFmpeg encoder options set
    /// over the low-latency defaults (tuning experiments, benchmarks).
    pub fn set_options(&mut self, backend: &str, opts: &[(String, String)]) -> Result<()> {
        let b = ALL.into_iter().find(|b| b.name() == backend).ok_or_else(|| anyhow!("unknown backend {backend}"))?;
        if let Some(codec) = ff::encoder::find_by_name(b.ffmpeg_name()) {
            // FFmpeg silently ignores unknown options.
            if let Some((k, _)) = opts.iter().find(|(k, _)| !ffmpeg::has_option(codec, k)) {
                return Err(anyhow!("{} has no option {k:?}", b.ffmpeg_name()));
            }
        }
        self.overrides.retain(|(o, _)| *o != b);
        self.overrides.push((b, opts.to_vec()));
        Ok(())
    }
}

impl EncoderProvider for FfmpegEncoders {
    fn backends(&self) -> Vec<&'static str> {
        self.available.iter().map(|b| b.name()).collect()
    }

    fn unavailable(&self) -> Vec<(String, String)> {
        self.unavailable.clone()
    }

    fn create(&self, req: &EncoderRequest) -> Result<Box<dyn VideoEncoder>> {
        let mut order = self.available.clone();
        if let Some(p) = req.preferred.as_deref() {
            if let Some(i) = order.iter().position(|b| b.name() == p) {
                let b = order.remove(i);
                order.insert(0, b);
            }
        }
        let mut skipped = Vec::new();
        for backend in order {
            let opts = self.overrides.iter().find(|(b, _)| *b == backend).map_or(&[][..], |(_, o)| o);
            for kind in backend.inputs(req.surface.is_some()) {
                match FfmpegEncoder::open(backend, kind, req.width, req.height, req.surface.as_ref(), opts) {
                    Ok(mut e) => {
                        if !skipped.is_empty() {
                            e.fallback = Some(skipped.join("; "));
                        }
                        eprintln!("video encoder: {}", e.name());
                        return Ok(Box::new(e));
                    }
                    Err(err) => skipped.push(format!("{} ({kind:?}): {err:#}", backend.name())),
                }
            }
        }
        Err(anyhow!("no H.264 encoder available: {}", skipped.join("; ")))
    }
}
