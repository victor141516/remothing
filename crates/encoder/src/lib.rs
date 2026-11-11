//! FFmpeg-based `EncoderProvider`: NVENC, AMF, QuickSync and x264 behind one
//! low-latency configuration, with zero-copy GPU input where the platform
//! offers it (CUDA on Linux, D3D11 on Windows) and system memory otherwise;
//! H.264 everywhere, HEVC and AV1 with NVENC; Opus or AAC for sound.

mod audio;

#[cfg(target_os = "linux")]
mod cuda;
#[cfg(windows)]
mod d3d11;
mod ffmpeg;

use anyhow::{anyhow, Result};
use ffmpeg_next as ff;
use remothing_core::encode::{AudioDecoder, AudioEncoder, Codec, EncoderProvider, EncoderRequest, VideoEncoder};

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

    /// The FFmpeg encoder for `codec`, None when this backend does not
    /// offer it here. HEVC and AV1 only with NVENC: AMF and QuickSync have
    /// them too, but their no-reorder output and rate control were never
    /// checked (no such GPU here); x264 is H.264 only.
    pub(crate) fn ffmpeg_name(self, codec: Codec) -> Option<&'static str> {
        match (self, codec) {
            (Backend::Nvenc, Codec::H264) => Some("h264_nvenc"),
            (Backend::Nvenc, Codec::Hevc) => Some("hevc_nvenc"),
            (Backend::Nvenc, Codec::Av1) => Some("av1_nvenc"),
            (Backend::Amf, Codec::H264) => Some("h264_amf"),
            (Backend::Qsv, Codec::H264) => Some("h264_qsv"),
            (Backend::X264, Codec::H264) => Some("libx264"),
            _ => None,
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

    /// Rate control follows a bitrate cap (`set_max_bitrate`). AMF and
    /// QuickSync keep constant QP: their capped modes are untested here.
    pub(crate) fn caps_bitrate(self) -> bool {
        matches!(self, Backend::Nvenc | Backend::X264)
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

/// FFmpeg encoder options (name, value).
type Options = Vec<(String, String)>;

pub struct FfmpegEncoders {
    /// Backends and codecs that passed the startup probe, in preference order.
    available: Vec<(Backend, Codec)>,
    /// Compiled-in backends and codecs that failed the startup probe, with the reason.
    unavailable: Vec<(String, String)>,
    /// Option overrides applied over a backend's defaults for one codec.
    overrides: Vec<(Backend, Codec, Options)>,
}

impl FfmpegEncoders {
    /// Probes every compiled-in backend and codec once (small host-memory
    /// encoder), so status and fallback choices reflect what this machine
    /// can actually run (AV1 needs a recent GPU, for one).
    pub fn new() -> Result<Self> {
        ff::init()?;
        // Missing vendor runtimes are expected while probing; the reason is kept per backend.
        ff::util::log::set_level(ff::util::log::Level::Quiet);
        let mut available = Vec::new();
        let mut unavailable = Vec::new();
        for b in ALL {
            for codec in Codec::ALL {
                let Some(name) = b.ffmpeg_name(codec) else { continue };
                if ff::encoder::find_by_name(name).is_none() {
                    continue;
                }
                match FfmpegEncoder::open(b, codec, InputKind::Host, (256, 256), (256, 256), None, 1_000_000, false, &[]) {
                    Ok(_) => available.push((b, codec)),
                    Err(e) => unavailable.push((format!("{} {}", b.name(), codec.name()), format!("{e:#}"))),
                }
            }
        }
        ff::util::log::set_level(ff::util::log::Level::Error);
        Ok(Self { available, unavailable, overrides: Vec::new() })
    }

    /// Replaces the option overrides of `backend` ("nvenc", "amf", "qsv",
    /// "x264") for `codec`, for encoders created afterwards: FFmpeg encoder
    /// options set over the low-latency defaults (tuning experiments,
    /// benchmarks).
    pub fn set_options(&mut self, backend: &str, codec: Codec, opts: &[(String, String)]) -> Result<()> {
        let b = ALL.into_iter().find(|b| b.name() == backend).ok_or_else(|| anyhow!("unknown backend {backend}"))?;
        let name = b.ffmpeg_name(codec).ok_or_else(|| anyhow!("{backend} has no {} encoder", codec.name()))?;
        if let Some(encoder) = ff::encoder::find_by_name(name) {
            // FFmpeg silently ignores unknown options.
            if let Some((k, _)) = opts.iter().find(|(k, _)| !ffmpeg::has_option(encoder, k)) {
                return Err(anyhow!("{name} has no option {k:?}"));
            }
        }
        self.overrides.retain(|(o, c, _)| (*o, *c) != (b, codec));
        self.overrides.push((b, codec, opts.to_vec()));
        Ok(())
    }
}

impl EncoderProvider for FfmpegEncoders {
    fn backends(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = Vec::new();
        for (b, _) in &self.available {
            if !names.contains(&b.name()) {
                names.push(b.name());
            }
        }
        names
    }

    fn codecs(&self) -> Vec<Codec> {
        Codec::ALL.into_iter().filter(|c| self.available.iter().any(|(_, a)| a == c)).collect()
    }

    fn unavailable(&self) -> Vec<(String, String)> {
        self.unavailable.clone()
    }

    fn create(&self, req: &EncoderRequest) -> Result<Box<dyn VideoEncoder>> {
        let mut order: Vec<Backend> = self.available.iter().filter(|(_, c)| *c == req.codec).map(|(b, _)| *b).collect();
        if let Some(p) = req.preferred.as_deref() {
            if let Some(i) = order.iter().position(|b| b.name() == p) {
                let b = order.remove(i);
                order.insert(0, b);
            }
        }
        let mut skipped = Vec::new();
        for backend in order {
            let opts = self.overrides.iter().find(|(b, c, _)| (*b, *c) == (backend, req.codec)).map_or(&[][..], |(_, _, o)| o);
            for kind in backend.inputs(req.surface.is_some()) {
                let (input, surface) = ((req.width, req.height), req.surface.as_ref());
                let (bitrate, two_pass) = (req.max_bitrate, req.two_pass);
                match FfmpegEncoder::open(backend, req.codec, kind, input, req.output, surface, bitrate, two_pass, opts) {
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
        Err(anyhow!("no {} encoder available: {}", req.codec.name(), skipped.join("; ")))
    }

    fn audio(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioEncoder>> {
        for codec in audio::CODECS {
            match audio::FfmpegAudio::open(codec) {
                Ok(encoder) => {
                    tried.push(format!("encoder {}: ok", codec.ffmpeg_name()));
                    return Ok(Box::new(encoder));
                }
                Err(e) => tried.push(format!("encoder {}: {e:#}", codec.ffmpeg_name())),
            }
        }
        Err(anyhow!("no sound encoder in this FFmpeg build"))
    }

    fn opus_decoder(&self, channels: usize) -> Result<Box<dyn AudioDecoder>> {
        Ok(Box::new(audio::FfmpegOpusDecoder::open(channels)?))
    }
}
