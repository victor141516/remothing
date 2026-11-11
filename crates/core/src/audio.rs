//! Sound of the streamed session. The platform captures what the session
//! plays, one encoder serves every client that listens (Opus; AAC where the
//! FFmpeg build has no libopus) and each client's WebSocket carries the
//! packets. Nothing is captured while nobody listens. Best effort: when no
//! capture path or no encoder works, the clients are told why, the log
//! lists what was tried, and it is tried again every now and then.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{broadcast, watch};

use crate::platform::{Platform, ThreadRole, AUDIO_CHANNELS, AUDIO_RATE};
use crate::proto;
use crate::telemetry::Telemetry;

/// Packets a client's task may fall behind by before it skips ahead (about a second).
const BACKLOG: usize = 128;
/// Without sound a sound server may simply not be up yet.
const RETRY: Duration = Duration::from_secs(15);
const RETRY_STEP: Duration = Duration::from_millis(250);
/// Frames of plain zeros sent before going quiet: a paused player costs
/// nothing, and the encoder still gets to fade out.
const SILENT_FRAMES: u32 = 50;

/// A listening client: packets as S_AUDIO messages, and the S_AUDIO_INFO
/// message that describes the stream or tells why there is none (None
/// while that is being found out).
pub struct Listener {
    hub: Arc<AudioHub>,
    pub packets: broadcast::Receiver<Arc<Vec<u8>>>,
    pub info: watch::Receiver<Option<Arc<Vec<u8>>>>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.hub.state.lock().unwrap().listeners -= 1;
    }
}

#[derive(Default)]
struct State {
    listeners: usize,
    /// The capture thread is alive (it ends when the last listener leaves).
    running: bool,
    /// For the admin UI.
    status: String,
}

pub struct AudioHub {
    platform: Arc<dyn Platform>,
    telemetry: Arc<Telemetry>,
    state: Mutex<State>,
    packets: broadcast::Sender<Arc<Vec<u8>>>,
    info: watch::Sender<Option<Arc<Vec<u8>>>>,
}

impl AudioHub {
    pub fn new(platform: Arc<dyn Platform>, telemetry: Arc<Telemetry>) -> Arc<Self> {
        let state = State { status: "off (no client listens)".into(), ..State::default() };
        Arc::new(Self {
            platform,
            telemetry,
            state: Mutex::new(state),
            packets: broadcast::channel(BACKLOG).0,
            info: watch::channel(None).0,
        })
    }

    /// Starts the capture when this is the first listener.
    pub fn listen(self: &Arc<Self>) -> Listener {
        let mut state = self.state.lock().unwrap();
        state.listeners += 1;
        if !state.running {
            state.running = true;
            self.info.send_replace(None);
            let hub = self.clone();
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || {
                    // A stalled sound thread is a dropout: the client plays only 60 ms ahead.
                    hub.platform.thread_priority(ThreadRole::Stream);
                    hub.run()
                })
                .expect("spawn audio thread");
        }
        Listener { hub: self.clone(), packets: self.packets.subscribe(), info: self.info.subscribe() }
    }

    /// One line for the admin UI: what plays, or what was tried.
    pub fn status(&self) -> String {
        self.state.lock().unwrap().status.clone()
    }

    /// True (and the thread must end) once nobody listens.
    fn idle(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.listeners > 0 {
            return false;
        }
        state.running = false;
        state.status = "off (no client listens)".into();
        true
    }

    fn run(&self) {
        // Reported once per distinct outcome, not on every retry.
        let mut reported = String::new();
        loop {
            let Err(tried) = self.stream() else { return };
            let reason = tried.join("; ");
            if reason != reported {
                self.telemetry.error("sound unavailable", &reason);
                reported.clone_from(&reason);
            }
            self.state.lock().unwrap().status = format!("unavailable: {reason}");
            let reason = "the server found no way to capture or encode it";
            let info = json!({ "active": false, "reason": reason, "tried": tried });
            self.info.send_replace(Some(Arc::new(proto::text(proto::S_AUDIO_INFO, &info.to_string()))));
            let retry_at = Instant::now() + RETRY;
            while Instant::now() < retry_at {
                if self.idle() {
                    return;
                }
                std::thread::sleep(RETRY_STEP);
            }
        }
    }

    /// Captures, encodes and sends until nobody listens (Ok) or something
    /// fails (Err: everything that was tried, with its outcome).
    fn stream(&self) -> Result<(), Vec<String>> {
        let mut tried = Vec::new();
        let source = self.platform.open_audio(&mut tried);
        let encoder = self.platform.encoders().audio(&mut tried);
        let (Ok(mut source), Ok(mut encoder)) = (source, encoder) else { return Err(tried) };
        let status = format!("{} -> {}", source.name(), encoder.codec());
        eprintln!("sound: {status} (tried: {})", tried.join("; "));
        self.state.lock().unwrap().status = status;
        let description: String = encoder.description().iter().map(|b| format!("{b:02x}")).collect();
        let info = json!({
            "active": true,
            "codec": encoder.codec(),
            "rate": AUDIO_RATE,
            "channels": AUDIO_CHANNELS,
            "description": description,
            "source": source.name(),
        });
        self.info.send_replace(Some(Arc::new(proto::text(proto::S_AUDIO_INFO, &info.to_string()))));

        let frame = encoder.frame_samples() * AUDIO_CHANNELS;
        let mut pcm: Vec<i16> = Vec::new();
        let mut silent = 0;
        while !self.idle() {
            let before = pcm.len();
            if let Err(e) = source.read(&mut pcm) {
                tried.push(format!("{} stopped: {e:#}", source.name()));
                return Err(tried);
            }
            if pcm.len() == before {
                // Nothing plays: the piece of a frame left over is not the
                // start of what plays next.
                pcm.clear();
                continue;
            }
            let mut done = 0;
            for chunk in pcm.chunks_exact(frame) {
                done += frame;
                silent = if chunk.iter().all(|&s| s == 0) { silent + 1 } else { 0 };
                if silent > SILENT_FRAMES {
                    continue;
                }
                let mut message = vec![proto::S_AUDIO];
                match encoder.encode(chunk, &mut message) {
                    Ok(true) => {
                        let _ = self.packets.send(Arc::new(message));
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tried.push(format!("{} encoder failed: {e:#}", encoder.codec()));
                        return Err(tried);
                    }
                }
            }
            pcm.drain(..done);
        }
        Ok(())
    }
}
