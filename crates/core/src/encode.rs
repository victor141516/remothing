//! Encoder contracts: video (lossy layer) and sound.

use anyhow::Result;

use crate::geom::Rect;
use crate::platform::GpuSurface;

pub struct EncoderRequest {
    pub width: u32,
    pub height: u32,
    /// Backend to try first ("nvenc", "amf", "qsv", "x264"); others follow as fallbacks.
    pub preferred: Option<String>,
    /// Capture surface, when the capture is GPU-based; enables zero-copy input.
    pub surface: Option<GpuSurface>,
    /// Initial bitrate cap, bits/s (see [`VideoEncoder::set_max_bitrate`]).
    pub max_bitrate: u64,
}

pub struct Encoded {
    /// H.264 Annex B access unit.
    pub data: Vec<u8>,
    pub key: bool,
}

pub trait EncoderProvider: Send + Sync {
    /// Backends usable on this machine, in default preference order.
    fn backends(&self) -> Vec<&'static str>;
    /// Backends present in the build but not usable here (name, reason), for status.
    fn unavailable(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn create(&self, req: &EncoderRequest) -> Result<Box<dyn VideoEncoder>>;
    /// Sound encoder for the platform's audio format, the best codec this
    /// build has. Every codec tried goes to `tried` with its outcome.
    fn audio(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioEncoder>>;
}

pub trait VideoEncoder: Send {
    /// Backend and input path, e.g. "nvenc (d3d11)".
    fn name(&self) -> String;
    /// Why preferred backends were skipped, if any.
    fn fallback_reason(&self) -> Option<String>;
    /// True when input comes from the capture GPU surface (`load_gpu`).
    fn gpu_input(&self) -> bool;
    /// Copies `rects` (display coordinates) into the encoder input from a BGRX
    /// buffer whose first row is display row `src_y0` (`stride` bytes per row).
    fn load_cpu(&mut self, src: &[u8], stride: usize, src_y0: u32, rects: &[Rect]) -> Result<()>;
    /// Copies `rects` of the capture surface into the encoder input (GPU-side).
    fn load_gpu(&mut self, surface: &GpuSurface, rects: &[Rect]) -> Result<()>;
    /// Low-latency H.264, no reordering; `force_key` produces an IDR.
    fn encode(&mut self, force_key: bool) -> Result<Encoded>;
    /// Caps the bitrate (bits/s) of following frames so video fits the link;
    /// quality stays at the encoder's fixed level whenever the cap allows.
    /// Returns false for backends that keep a fixed-quality mode.
    fn set_max_bitrate(&mut self, bits_per_sec: u64) -> bool;
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
