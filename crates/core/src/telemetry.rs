//! Process-wide status for the admin UI: recent errors, connected clients,
//! active encoders and CPU usage.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Mutex;

use serde::Serialize;

use crate::proto::now_us;
use crate::shared::Link;

const MAX_ERRORS: usize = 100;

#[derive(Clone, Serialize)]
pub struct ErrorEntry {
    pub at_ms: u64,
    pub context: String,
    pub message: String,
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
    /// Latest stats reported by the client itself (C_STATS JSON).
    pub client_report: Option<serde_json::Value>,
    #[serde(skip)]
    window_start_us: u64,
    #[serde(skip)]
    window_frames: u64,
    #[serde(skip)]
    window_bytes: u64,
}

pub struct Telemetry {
    errors: Mutex<VecDeque<ErrorEntry>>,
    clients: Mutex<HashMap<u64, ClientStatus>>,
    cpu: Mutex<sysinfo::System>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            errors: Mutex::new(VecDeque::new()),
            clients: Mutex::new(HashMap::new()),
            cpu: Mutex::new(sysinfo::System::new()),
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
        let now = now_us();
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.window_frames += 1;
            c.window_bytes += bytes as u64;
            let elapsed = now - c.window_start_us;
            if elapsed >= 1_000_000 {
                c.fps = c.window_frames as f64 * 1e6 / elapsed as f64;
                c.mbps = c.window_bytes as f64 * 8.0 / elapsed as f64;
                c.window_start_us = now;
                c.window_frames = 0;
                c.window_bytes = 0;
            }
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

    pub fn client_report(&self, id: u64, report: serde_json::Value) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.client_report = Some(report);
        }
    }

    pub fn clients(&self) -> Vec<ClientStatus> {
        let mut v: Vec<_> = self.clients.lock().unwrap().values().cloned().collect();
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
