//! Provider contracts. Everything OS-specific lives behind these traits; the
//! core (tiles, policy, protocol, transport, admin) only talks to them.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::encode::EncoderProvider;
use crate::geom::{MovedRect, Rect};

/// Whole-platform entry point, one implementation per OS (`cfg(target_os)`).
pub trait Platform: Send + Sync + 'static {
    /// "linux", "windows".
    fn name(&self) -> &'static str;
    fn displays(&self) -> &dyn DisplayProvider;
    fn encoders(&self) -> &dyn EncoderProvider;
    fn monitor(&self) -> &dyn SystemMonitor;
    /// Opens the capture of what the session plays. Every path tried goes
    /// to `tried` with its outcome, so the log tells why there is no sound.
    fn open_audio(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioSource>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayKind {
    /// A physical/attached display.
    Real,
    /// Simulated display created by this process (Linux Xvfb).
    Virtual,
}

#[derive(Clone, Debug, Serialize)]
pub struct DisplayInfo {
    /// Stable id used to select the display (e.g. "xvfb:1", "x11:0:HDMI-1", "dxgi:0:1").
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
    /// Position in the platform's virtual desktop.
    pub x: i32,
    pub y: i32,
    pub primary: bool,
    pub kind: DisplayKind,
}

pub trait DisplayProvider: Send + Sync {
    /// Human-readable provider name for status ("xvfb + x11", "dxgi duplication").
    fn provider_name(&self) -> String;
    fn list(&self) -> Result<Vec<DisplayInfo>>;
    fn open(&self, id: &str) -> Result<OpenedDisplay>;
    /// Present only where simulated displays can be created (Linux).
    fn virtual_displays(&self) -> Option<&dyn VirtualDisplays> {
        None
    }
}

pub trait VirtualDisplays: Send + Sync {
    fn create(&self, width: u32, height: u32) -> Result<DisplayInfo>;
    fn destroy(&self, id: &str) -> Result<()>;
}

pub struct OpenedDisplay {
    pub info: DisplayInfo,
    pub capture: Box<dyn CaptureSession>,
    pub input: Arc<dyn InputProvider>,
    /// None where the session's clipboard cannot be reached.
    pub clipboard: Option<Arc<dyn Clipboard>>,
}

/// Receives the clipboard text each time it changes.
pub type ClipboardSink = Box<dyn Fn(String) + Send + Sync>;

/// Text clipboard of the session a display belongs to.
pub trait Clipboard: Send + Sync {
    fn set_text(&self, text: &str) -> Result<()>;
    /// Reports the text every time something else puts text on the
    /// clipboard, from now on (called once).
    fn watch(&self, on_change: ClipboardSink);
}

/// Opaque GPU surface handle passed from a capture session to an encoder of
/// the same platform (downcast on the encoder side, e.g. to a D3D11 frame).
pub type GpuSurface = Arc<dyn Any + Send + Sync>;

pub struct FrameInfo {
    /// Wall-clock microseconds (UNIX epoch) when the change was observed.
    pub capture_us: u64,
    /// Areas whose pixels may have changed (may over-report; the core diffs tiles).
    pub dirty: Vec<Rect>,
    /// Optional move/scroll hints; their destinations are also reported dirty.
    pub moved: Vec<MovedRect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// Straight-alpha RGBA, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorState {
    Hidden,
    Visible(Arc<CursorShape>),
}

pub trait CursorProvider {
    /// Cursor visibility/shape if it changed since the previous call.
    fn cursor_change(&mut self) -> Result<Option<CursorState>>;
}

/// One open display capture. Driven from a single capture thread.
pub trait CaptureSession: CursorProvider + Send {
    fn size(&self) -> (u32, u32);
    /// Waits up to `timeout` for the screen to change. The first call reports
    /// the whole display dirty.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<FrameInfo>>;
    /// Copies `rects` of the latest frame as BGRX into `dst`, a full-display
    /// buffer with `dst_stride` bytes per row (pixels land at their own coordinates).
    fn read(&mut self, rects: &[Rect], dst: &mut [u8], dst_stride: usize) -> Result<()>;
    /// Direct read-only view of the latest frame (BGRX, stride) when the
    /// pixels live in mappable memory; lets the core diff without `read` copies.
    fn mapped(&self) -> Option<(&[u8], usize)> {
        None
    }
    /// GPU surface always holding the latest frame (BGRA), for zero-copy encoding.
    fn gpu_surface(&self) -> Option<GpuSurface> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// Input injection into one display. Coordinates are display-relative pixels.
pub trait InputProvider: Send + Sync {
    fn motion(&self, x: u32, y: u32) -> Result<()>;
    fn button(&self, button: Button, down: bool) -> Result<()>;
    /// Wheel steps (notches); positive = down / right.
    fn wheel(&self, dx: i32, dy: i32) -> Result<()>;
    /// Physical key as a Linux evdev code (wire format), mapped per platform.
    fn key(&self, evdev: u16, down: bool) -> Result<()>;
    /// Types characters, whatever the keyboard layout. `\n` is the Enter key.
    fn text(&self, text: &str) -> Result<()>;
    fn set_keyboard_layout(&self, _layout: &str) -> Result<()> {
        bail!("keyboard layout is controlled by the host OS on this platform")
    }
}

/// Sample rate and channels of every [`AudioSource`].
pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;

/// What the session plays (every program's sound, mixed) as interleaved
/// signed 16-bit samples, [`AUDIO_CHANNELS`] at [`AUDIO_RATE`].
pub trait AudioSource: Send {
    /// The capture path in use, for the log and the clients.
    fn name(&self) -> String;
    /// Appends what was captured since the last call, waiting about a tenth
    /// of a second when there is nothing: some paths deliver nothing while
    /// nothing plays.
    fn read(&mut self, out: &mut Vec<i16>) -> Result<()>;
}

#[derive(Clone, Debug, Serialize)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: String,
    /// Utilization percent when the platform can measure it.
    pub utilization: Option<f32>,
}

pub trait SystemMonitor: Send + Sync {
    fn gpus(&self) -> Vec<GpuInfo>;
}
