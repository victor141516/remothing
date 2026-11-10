//! Video encoder contracts (lossy layer).

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
}

pub struct Encoded {
    /// H.264 Annex B access unit.
    pub data: Vec<u8>,
    pub key: bool,
}

pub trait EncoderProvider: Send + Sync {
    /// Backends compiled in, in default preference order.
    fn backends(&self) -> Vec<&'static str>;
    fn create(&self, req: &EncoderRequest) -> Result<Box<dyn VideoEncoder>>;
}

pub trait VideoEncoder: Send {
    /// Backend and input path, e.g. "nvenc (d3d11)".
    fn name(&self) -> String;
    /// Why preferred backends were skipped, if any.
    fn fallback_reason(&self) -> Option<String>;
    /// True when input comes from the capture GPU surface (`load_gpu`).
    fn gpu_input(&self) -> bool;
    /// Copies `rects` of a BGRX frame (`stride` bytes per row) into the encoder input.
    fn load_cpu(&mut self, src: &[u8], stride: usize, rects: &[Rect]) -> Result<()>;
    /// Copies `rects` of the capture surface into the encoder input (GPU-side).
    fn load_gpu(&mut self, surface: &GpuSurface, rects: &[Rect]) -> Result<()>;
    /// Low-latency H.264, no reordering; `force_key` produces an IDR.
    fn encode(&mut self, force_key: bool) -> Result<Encoded>;
}
