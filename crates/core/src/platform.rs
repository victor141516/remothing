//! Provider contracts. Everything OS-specific lives behind these traits; the
//! core (tiles, policy, protocol, transport, admin) only talks to them.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::encode::EncoderProvider;
use crate::geom::{MovedRect, Rect};

/// What a thread does, for its scheduling priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadRole {
    /// The capture loop: everything waits for it.
    Capture,
    /// Frame building and encoding for one client, the network runtime,
    /// the WebRTC hub, sound capture.
    Stream,
}
/// Whole-platform entry point, one implementation per OS (`cfg(target_os)`).
pub trait Platform: Send + Sync + 'static {
    /// "linux", "windows".
    fn name(&self) -> &'static str;
    fn displays(&self) -> &dyn DisplayProvider;
    fn encoders(&self) -> &dyn EncoderProvider;
    fn monitor(&self) -> &dyn SystemMonitor;
    /// Asks the OS to favour the calling thread for its `role`. Best effort:
    /// the stream works without it.
    fn thread_priority(&self, _role: ThreadRole) {}
    /// Clients are connected (`true`) or none is left (`false`): the OS is
    /// asked to favour this process while it streams and to let go
    /// afterwards. Best effort; returns what was done and what failed, for
    /// the log and the admin status.
    fn streaming(&self, _active: bool) -> Vec<String> {
        Vec::new()
    }
    /// Opens the capture of what the session plays. Every path tried goes
    /// to `tried` with its outcome, so the log tells why there is no sound.
    fn open_audio(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioSource>>;
    /// Opens the session's virtual microphone: what is written to it is
    /// what programs recording from the microphone hear. `tried` as above.
    fn open_microphone(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioSink>, MicUnavailable>;
    /// Installs the component a [`MicUnavailable::Install`] named. The
    /// system asks whoever is at the server for consent where it needs one.
    fn install_microphone(&self) -> Result<()> {
        bail!("nothing to install on this platform")
    }
    /// Restarts the computer, after an installation that needs it.
    fn restart(&self) -> Result<()> {
        bail!("restarting is not supported on this platform")
    }
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
    /// Opens display `id`; `hdr`: deliver an HDR picture if the display is
    /// in HDR (see [`Picture`]), else an SDR one.
    fn open(&self, id: &str, hdr: bool) -> Result<OpenedDisplay>;
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

/// What the pixels of a capture session mean.
///
/// SDR: 8-bit sRGB BGRX, as always. HDR: each pixel is a little-endian u32
/// with 10 bits per channel (red in the low bits, then green, blue, and 2
/// bits of alpha: `R10G10B10A2`), and the encoder's GPU surface holds the
/// same picture in 8 bits. Both carry `proto::hdr_encode` of the linear
/// value relative to the host's SDR white, up to `hdr_max` times it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Picture {
    /// The display is in HDR, so an HDR picture could be asked for.
    pub hdr_display: bool,
    /// Brightest value of an HDR picture in multiples of SDR white; 0: SDR.
    pub hdr_max: f32,
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
    /// Where the cursor's hot spot is (display pixels, clamped to the
    /// display) if it moved since the previous call. Asked only while a
    /// client has the mouse captured.
    fn cursor_position(&mut self) -> Result<Option<(u32, u32)>>;
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
    /// The picture delivered. A session whose picture would change (the
    /// display switched HDR on or off, its brightness levels changed) fails
    /// instead, so the display is opened again.
    fn picture(&self) -> Picture {
        Picture::default()
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
    /// Moves the pointer by `dx`, `dy` mouse counts, as a mouse does (a game
    /// reads them as they are). The pointer stays on this display: a move
    /// that would leave it ends on its edge.
    fn motion_relative(&self, dx: i32, dy: i32) -> Result<()>;
    fn button(&self, button: Button, down: bool) -> Result<()>;
    /// Scroll in wheel units (`proto::WHEEL_NOTCH` per notch, fractions of
    /// one from trackpads and touch); positive = down / right.
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

/// The session's virtual microphone, fed in the format of [`AudioSource`].
pub trait AudioSink: Send {
    /// Where the samples go, for the log and the client.
    fn name(&self) -> String;
    /// Plays samples at once. Called in real time; what does not fit is dropped.
    fn write(&mut self, pcm: &[i16]) -> Result<()>;
}

/// Why there is no virtual microphone.
pub enum MicUnavailable {
    /// A component is missing that [`Platform::install_microphone`] installs.
    Install(MicComponent),
    /// The component is installed but only works after a restart.
    Restart,
    /// Nothing worked; `tried` tells what.
    Failed,
}

/// Something the virtual microphone needs installed on the server.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MicComponent {
    pub name: &'static str,
    /// Whose it is and on what terms, for whoever decides to install it.
    pub notice: &'static str,
    /// Installing it may need the computer restarted.
    pub restart: bool,
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
