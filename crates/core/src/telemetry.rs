//! Process-wide status for the admin UI: recent errors, connected clients,
//! active encoders and CPU usage.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Mutex;

use serde::Serialize;

use crate::proto::now_us;
use crate::shared::Link;

const MAX_ERRORS: usize = 100;
const MAX_EVENTS: usize = 200;
/// Longest client-supplied event text kept (a WebTransport failure lists
/// every address tried).
const MAX_EVENT_LEN: usize = 500;
/// A client sends a trace batch every 2 s: two minutes of each.
const MAX_TRACE_BATCHES: usize = 60;
/// Traces of this many clients (the newest) are kept.
const MAX_TRACED_CLIENTS: usize = 8;

#[derive(Clone, Serialize)]
pub struct ErrorEntry {
    pub at_ms: u64,
    pub context: String,
    pub message: String,
}

/// Something that happened to one client's stream (path switch, estimate
/// cut, lost frames, a freeze the client saw): what explains a stutter
/// after the fact.
#[derive(Clone, Serialize)]
pub struct EventEntry {
    pub at_ms: u64,
    pub client: u64,
    pub text: String,
}

#[derive(Clone, Default, Serialize)]
pub struct ClientStatus {
    pub id: u64,
    pub addr: String,
    pub connected_ms: u64,
    pub encoder: Option<String>,
    pub encoder_fallback: Option<String>,
    /// Server-side send rate over the last second.
    pub fps: f64,
    pub mbps: f64,
    /// Server's estimate of the client link: bandwidth (a lower bound until
    /// `link_proven`), round trip, and the video bitrate cap derived from it.
    pub link_mbps: f64,
    pub link_proven: bool,
    pub rtt_ms: f64,
    pub video_cap_mbps: Option<f64>,
    /// "websocket" or "webrtc" (frames over the UDP data channel).
    pub transport: String,
    /// Data-channel frames lost and repaired.
    pub lost_frames: u64,
    /// The client's tab is hidden: nothing is sent until it shows again.
    pub paused: bool,
    /// Latest stats reported by the client itself (C_STATS JSON).
    pub client_report: Option<serde_json::Value>,
    #[serde(skip)]
    window_start_us: u64,
    #[serde(skip)]
    window_frames: u64,
    #[serde(skip)]
    window_bytes: u64,
}

impl ClientStatus {
    /// Closes the rate window once it is a second old.
    fn roll_window(&mut self, now: u64) {
        let elapsed = now - self.window_start_us;
        if elapsed >= 1_000_000 {
            self.fps = self.window_frames as f64 * 1e6 / elapsed as f64;
            self.mbps = self.window_bytes as f64 * 8.0 / elapsed as f64;
            self.window_start_us = now;
            self.window_frames = 0;
            self.window_bytes = 0;
        }
    }
}

pub struct Telemetry {
    errors: Mutex<VecDeque<ErrorEntry>>,
    events: Mutex<VecDeque<EventEntry>>,
    clients: Mutex<HashMap<u64, ClientStatus>>,
    /// Frame trace batches per client (`[debug] frame_trace`).
    traces: Mutex<HashMap<u64, VecDeque<serde_json::Value>>>,
    cpu: Mutex<sysinfo::System>,
}

impl Default for Telemetry {
    fn default() -> Self {
        let mut cpu = sysinfo::System::new();
        // Usage is measured between refreshes: without this one the first
        // status request would report garbage.
        cpu.refresh_cpu_usage();
        Self {
            errors: Mutex::new(VecDeque::new()),
            events: Mutex::new(VecDeque::new()),
            clients: Mutex::new(HashMap::new()),
            traces: Mutex::new(HashMap::new()),
            cpu: Mutex::new(cpu),
        }
    }
}

impl Telemetry {
    /// Logs to stderr and keeps the error for the admin UI.
    pub fn error(&self, context: &str, err: impl std::fmt::Display) {
        let message = format!("{err:#}");
        eprintln!("error [{context}]: {message}");
        let mut q = self.errors.lock().unwrap();
        if q.len() == MAX_ERRORS {
            q.pop_front();
        }
        q.push_back(ErrorEntry { at_ms: now_us() / 1000, context: context.into(), message });
    }

    pub fn errors(&self) -> Vec<ErrorEntry> {
        self.errors.lock().unwrap().iter().cloned().collect()
    }

    /// Logs to stderr and keeps the event for the admin UI.
    pub fn event(&self, client: u64, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).take(MAX_EVENT_LEN).collect();
        eprintln!("client {client}: {text}");
        let mut q = self.events.lock().unwrap();
        if q.len() == MAX_EVENTS {
            q.pop_front();
        }
        q.push_back(EventEntry { at_ms: now_us() / 1000, client, text });
    }

    pub fn events(&self) -> Vec<EventEntry> {
        self.events.lock().unwrap().iter().cloned().collect()
    }

    pub fn client_connected(&self, id: u64, addr: SocketAddr) {
        let now = now_us();
        self.clients.lock().unwrap().insert(
            id,
            ClientStatus {
                id,
                addr: addr.to_string(),
                connected_ms: now / 1000,
                transport: "websocket".into(),
                window_start_us: now,
                ..Default::default()
            },
        );
    }

    pub fn client_disconnected(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
    }

    pub fn client_encoder(&self, id: u64, name: Option<String>, fallback: Option<String>) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.encoder = name;
            c.encoder_fallback = fallback;
        }
    }

    pub fn client_sent(&self, id: u64, bytes: usize) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.window_frames += 1;
            c.window_bytes += bytes as u64;
            c.roll_window(now_us());
        }
    }

    pub fn client_link(&self, id: u64, link: Link, rtt_us: u64, video_cap_bps: Option<u64>) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.link_mbps = link.rate * 8.0 / 1e6;
            c.link_proven = link.proven;
            c.rtt_ms = rtt_us as f64 / 1e3;
            c.video_cap_mbps = video_cap_bps.map(|b| b as f64 / 1e6);
        }
    }

    pub fn client_transport(&self, id: u64, transport: &str) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.transport = transport.to_string();
        }
    }

    pub fn client_lost(&self, id: u64) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.lost_frames += 1;
        }
    }

    pub fn client_paused(&self, id: u64, paused: bool) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.paused = paused;
        }
    }

    pub fn client_report(&self, id: u64, report: serde_json::Value) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.client_report = Some(report);
        }
    }

    /// One batch of a client's frame trace (C_TRACE). Kept after it leaves,
    /// so a session can be read once it has ended.
    pub fn client_trace(&self, id: u64, batch: serde_json::Value) {
        let mut traces = self.traces.lock().unwrap();
        if !traces.contains_key(&id) && traces.len() == MAX_TRACED_CLIENTS {
            let oldest = *traces.keys().min().expect("full map has keys");
            traces.remove(&oldest);
        }
        let q = traces.entry(id).or_default();
        if q.len() == MAX_TRACE_BATCHES {
            q.pop_front();
        }
        q.push_back(batch);
    }

    /// The kept batches of one client's trace, oldest first.
    pub fn trace(&self, id: u64) -> Option<Vec<serde_json::Value>> {
        self.traces.lock().unwrap().get(&id).map(|q| q.iter().cloned().collect())
    }

    /// Clients with a kept trace and how many batches each.
    pub fn traced(&self) -> Vec<(u64, usize)> {
        let mut v: Vec<_> = self.traces.lock().unwrap().iter().map(|(&id, q)| (id, q.len())).collect();
        v.sort_unstable();
        v
    }

    pub fn clients(&self) -> Vec<ClientStatus> {
        let now = now_us();
        let mut clients = self.clients.lock().unwrap();
        // An idle or paused client sends nothing, so nothing else would
        // bring its rate down.
        let mut v: Vec<_> = clients
            .values_mut()
            .map(|c| {
                c.roll_window(now);
                c.clone()
            })
            .collect();
        v.sort_by_key(|c| c.id);
        v
    }

    /// Global CPU usage since the previous call, percent.
    pub fn cpu_usage(&self) -> f32 {
        let mut s = self.cpu.lock().unwrap();
        s.refresh_cpu_usage();
        s.global_cpu_usage()
    }
}
