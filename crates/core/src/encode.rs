//! Encoder contracts: video (lossy layer) and sound.

use anyhow::Result;

use crate::geom::Rect;
use crate::platform::GpuSurface;

/// Video codecs of the lossy layer. The client says which it decodes in
/// hardware; H.264 is always possible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::H264, Codec::Hevc, Codec::Av1];

    /// Name in the config, the client's offer and the admin UI.
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }

    pub fn from_name(name: &str) -> Option<Codec> {
        Codec::ALL.into_iter().find(|c| c.name() == name)
    }

    /// Its number in FRAME flags (see `proto::FLAG_CODEC_SHIFT`).
    pub fn wire(self) -> u8 {
        match self {
            Codec::H264 => 0,
            Codec::Hevc => 1,
            Codec::Av1 => 2,
        }
    }
}

pub struct EncoderRequest {
    pub codec: Codec,
    /// Input size: the display's.
    pub width: u32,
    pub height: u32,
    /// Encoded size; the input is scaled down to it when smaller (both even).
    pub output: (u32, u32),
    /// Backend to try first ("nvenc", "amf", "qsv", "x264"); others follow as fallbacks.
    pub preferred: Option<String>,
    /// Capture surface, when the capture is GPU-based; enables zero-copy input.
    pub surface: Option<GpuSurface>,
    /// Initial bitrate cap, bits/s, and the frame rate it is for (see
    /// [`VideoEncoder::set_max_bitrate`]).
    pub max_bitrate: u64,
    pub fps: u32,
    /// Two passes per frame (a quick one at reduced size to plan the real
    /// one), where the encoder offers it: steadier frame sizes for a little
    /// more encode time.
    pub two_pass: bool,
    /// The client cannot show BT.601 video in its colours: BT.709 even where
    /// the encoder would otherwise convert to BT.601 (NVENC with RGB input),
    /// at the cost of a conversion pass of its own.
    pub bt709: bool,
}

pub struct Encoded {
    /// One access unit: Annex B (H.264, HEVC) or a temporal unit of
    /// low-overhead OBUs (AV1); keyframes carry the parameter sets or the
    /// sequence header.
    pub data: Vec<u8>,
    pub key: bool,
    /// The encoder's own frame counter, from 0 for the first frame it codes.
    pub index: u64,
    /// Some(first): the first frame coded after `invalidate_from(first)` without a
    /// keyframe; it predicts only from frames before `first`.
    pub recovers: Option<u64>,
}

pub trait EncoderProvider: Send + Sync {
    /// Backends usable on this machine, in default preference order.
    fn backends(&self) -> Vec<&'static str>;
    /// Codecs some usable backend encodes.
    fn codecs(&self) -> Vec<Codec>;
    /// Backends present in the build but not usable here (name, reason), for status.
    fn unavailable(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// An encoder of `req.codec`, from the backends that encode it.
    fn create(&self, req: &EncoderRequest) -> Result<Box<dyn VideoEncoder>>;
    /// Sound encoder for the platform's audio format, the best codec this
    /// build has. Every codec tried goes to `tried` with its outcome.
    fn audio(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioEncoder>>;
    /// Decoder for Opus packets of `channels` channels (a client's microphone).
    fn opus_decoder(&self, channels: usize) -> Result<Box<dyn AudioDecoder>>;
}

pub trait VideoEncoder: Send {
    /// Backend, codec and input path, e.g. "nvenc hevc (d3d11)".
    fn name(&self) -> String;
    fn codec(&self) -> Codec;
    /// Why preferred backends were skipped, if any.
    fn fallback_reason(&self) -> Option<String>;
    /// True when input comes from the capture GPU surface (`load_gpu`).
    fn gpu_input(&self) -> bool;
    /// Copies `rects` (display coordinates) into the encoder input from a BGRX
    /// buffer whose first row is display row `src_y0` (`stride` bytes per row).
    fn load_cpu(&mut self, src: &[u8], stride: usize, src_y0: u32, rects: &[Rect]) -> Result<()>;
    /// Copies `rects` of the capture surface into the encoder input (GPU-side).
    fn load_gpu(&mut self, surface: &GpuSurface, rects: &[Rect]) -> Result<()>;
    /// Low latency, no reordering; `force_key` produces an IDR.
    fn encode(&mut self, force_key: bool) -> Result<Encoded>;
    /// Caps the bitrate (bits/s) of following frames so video fits the link;
    /// quality stays at the encoder's fixed level whenever the cap allows.
    /// `fps`: the most frames a second there will be. A frame may take
    /// `bits_per_sec / fps`, so that many fit the cap: a rate control that
    /// counts frames instead of time (NVENC) would otherwise make each as
    /// large as at its assumed rate, and frame pacing would then drop
    /// frames to fit the link. Returns false for backends that keep a
    /// fixed-quality mode.
    fn set_max_bitrate(&mut self, bits_per_sec: u64, fps: u32) -> bool;
    /// Whether it encodes in two passes: None for encoders without that
    /// setting, so asking for the other one would change nothing.
    fn two_pass(&self) -> Option<bool>;
    /// The video's YUV is BT.601 rather than BT.709: NVENC converts RGB
    /// input with that matrix (measured), the other paths convert to BT.709.
    fn bt601(&self) -> bool;
    /// Frames from `first` on (Encoded::index) never reached the client, nor did any frame that
    /// predicts from them: the next frame must not predict from any of them. True when the encoder
    /// can do that without a keyframe; false: the caller forces one.
    fn invalidate_from(&mut self, _first: u64) -> bool {
        false
    }
    /// Every frame up to `index` reached and was decoded by the client (anchors may be pinned).
    fn acknowledge(&mut self, _index: u64) {}
    /// `set_max_bitrate` changes cost no keyframe.
    fn free_rate_changes(&self) -> bool {
        false
    }
    /// Areas the client does not show from video now (it shows its own exact copy there): the
    /// encoder may code them as cheaply as it can until a later call leaves them out.
    fn set_unseen(&mut self, _rects: &[Rect]) {}
}

pub trait AudioEncoder: Send {
    /// The codec as WebCodecs names it: "opus", "mp4a.40.2".
    fn codec(&self) -> &'static str;
    /// Configuration the decoder needs besides the packets (AAC's
    /// AudioSpecificConfig); empty when the packets say it all.
    fn description(&self) -> &[u8];
    /// Sample frames (one sample per channel) each `encode` call takes.
    fn frame_samples(&self) -> usize;
    /// Encodes one frame of interleaved samples and appends the packet to
    /// `out`; false while the encoder still needs input for its first one.
    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<bool>;
}

pub trait AudioDecoder: Send {
    /// Decodes one packet and appends its samples to `out` in the
    /// platform's audio format (interleaved, `AUDIO_CHANNELS` at `AUDIO_RATE`).
    fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<()>;
}
