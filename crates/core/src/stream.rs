//! Streaming endpoints: `/ws` (frames, ACKs, pings, WebRTC signaling) and
//! `/input` (input only).
//!
//! Per client: a dedicated OS thread builds and encodes frames (blocking work,
//! woken by capture / ACKs via unpark), an async writer drains its output, and
//! an async reader handles pings, ACKs, keyframe requests, stats and
//! signaling. Once the browser's peer connection is up, frames and input
//! travel over WebRTC data channels (`rtc.rs`, UDP, a direct path when one
//! exists); the WebSocket keeps control messages and is the fallback.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::app::App;
use crate::config::constant_time_eq;
use crate::encode::EncoderRequest;
use crate::pacer::Pacer;
use crate::pixels::Geometry;
use crate::platform::Button;
use crate::proto;
use crate::recovery::SentLog;
use crate::rtc::PeerEvent;
use crate::shared::{ClientCodec, Link, Pull, VIDEO_SHARE};

/// Where frames go: the WebSocket, or the WebRTC frames channel.
const PATH_WS: u8 = 0;
const PATH_RTC: u8 = 1;
/// A data-channel frame unacknowledged for this many base round trips (and
/// at least the minimum) is presumed lost with a dying channel: the client
/// cannot report a last frame that never reached it.
const LOSS_TIMEOUT_RTTS: u64 = 8;
const LOSS_TIMEOUT_MIN_US: u64 = 1_000_000;

/// Video cap changes cost an IDR with NVENC, itself a burst that can cut the
/// estimate again: only for clear shifts, at most this often, unless the link
/// collapsed (target under CAP_COLLAPSE of the cap).
const CAP_HOLD_US: u64 = 3_000_000;
const CAP_DEADBAND: (f64, f64) = (0.67, 1.5);
const CAP_COLLAPSE: f64 = 0.33;

/// Longest gap between messages from a live client (it pings every second).
const CLIENT_SILENCE: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
pub struct TokenQuery {
    #[serde(default)]
    token: String,
}

fn authorized(app: &App, q: &TokenQuery) -> bool {
    let expected = app.settings.read().unwrap().auth.stream_token.clone();
    expected.is_empty() || constant_time_eq(expected.as_bytes(), q.token.as_bytes())
}

pub async fn stream_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<TokenQuery>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app): State<Arc<App>>,
) -> Response {
    if !authorized(&app, &q) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.write_buffer_size(0).on_upgrade(move |socket| session(socket, app, addr))
}

/// Input gets its own socket so events never queue behind frame data or ACKs.
/// For the same reason its pings measure the WebSocket path's unloaded round
/// trip, which the client compares with the data channel's.
pub async fn input_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<TokenQuery>,
    State(app): State<Arc<App>>,
) -> Response {
    if !authorized(&app, &q) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |mut socket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            let Message::Binary(b) = msg else { continue };
            if b.first() == Some(&proto::C_PING) && b.len() >= 16 {
                let pong = proto::pong(f64::from_le_bytes(b[8..16].try_into().unwrap()));
                if socket.send(Message::Binary(pong.into())).await.is_err() {
                    break;
                }
            } else if let Err(e) = handle_input(&b, &app) {
                app.telemetry.error("input", e);
            }
        }
    })
}

struct ClientLink {
    id: u64,
    pacer: Mutex<Pacer>,
    /// Frames in flight with what they change (repairs after a loss).
    sent: Mutex<SentLog>,
    /// The frames channel is open (the client chooses whether to use it).
    rtc_open: AtomicBool,
    key_request: AtomicBool,
    /// The client applied the last HELLO: frames of that display may go out.
    hello_acked: AtomicBool,
    /// PATH_WS or PATH_RTC.
    path: AtomicU8,
    stop: AtomicBool,
}

async fn session(socket: WebSocket, app: Arc<App>, addr: SocketAddr) {
    let (mut tx, mut rx) = socket.split();
    let link = Arc::new(ClientLink {
        id: app.shared.new_client_id(),
        pacer: Mutex::new(Pacer::default()),
        sent: Mutex::new(SentLog::default()),
        rtc_open: AtomicBool::new(false),
        key_request: AtomicBool::new(false),
        // Open: the first pull is what produces the first HELLO.
        hello_acked: AtomicBool::new(true),
        path: AtomicU8::new(PATH_WS),
        stop: AtomicBool::new(false),
    });
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let producer = {
        let (app, link, out_tx) = (app.clone(), link.clone(), out_tx.clone());
        std::thread::Builder::new()
            .name(format!("client-{}", link.id))
            .spawn(move || produce(&app, &link, &out_tx))
            .expect("spawn client thread")
    };
    let waker = producer.thread().clone();
    app.telemetry.client_connected(link.id, addr);
    app.shared.add_client(link.id, waker.clone());
    if let Some(hub) = &app.rtc {
        // The browser offers a peer connection with these STUN servers.
        let ice: Vec<serde_json::Value> = hub.ice_servers.iter().map(|u| json!({ "urls": u })).collect();
        let _ = out_tx.send(proto::signal(&json!({ "type": "config", "iceServers": ice })));
    }

    let peer_out = out_tx.clone();
    let reader = {
        let (app, link, waker) = (app.clone(), link.clone(), waker.clone());
        async move {
            // Clients ping every second; a silent peer (half-open connection,
            // e.g. through a port forwarder) is dropped so it stops holding an encoder.
            while let Ok(Some(Ok(msg))) = tokio::time::timeout(CLIENT_SILENCE, rx.next()).await {
                let Message::Binary(b) = msg else { continue };
                match (b.first().copied(), b.len()) {
                    (Some(proto::C_PING), 16..) => {
                        let ts = f64::from_le_bytes(b[8..16].try_into().unwrap());
                        let _ = out_tx.send(proto::pong(ts));
                    }
                    (Some(proto::C_ACK), 12..) => {
                        on_ack(&b, &app, &link);
                        waker.unpark();
                    }
                    (Some(proto::C_KEYFRAME), _) => {
                        link.key_request.store(true, Ordering::Release);
                        waker.unpark();
                    }
                    (Some(proto::C_STATS), _) => {
                        if let Ok(v) = serde_json::from_slice(&b[1..]) {
                            app.telemetry.client_report(link.id, v);
                        }
                    }
                    (Some(proto::C_HELLO_ACK), _) => {
                        link.hello_acked.store(true, Ordering::Release);
                        waker.unpark();
                    }
                    (Some(proto::C_SIGNAL), _) => on_signal(&b[1..], &app, &link, &out_tx, &peer_tx).await,
                    _ => {
                        if let Err(e) = handle_input(&b, &app) {
                            app.telemetry.error("input", e);
                        }
                    }
                }
            }
        }
    };
    let peer = {
        let (app, link, waker, out_tx) = (app.clone(), link.clone(), waker.clone(), peer_out);
        async move {
            while let Some(event) = peer_rx.recv().await {
                match event {
                    PeerEvent::FramesOpen => link.rtc_open.store(true, Ordering::Release),
                    PeerEvent::Frames(b) => match (b.first().copied(), b.len()) {
                        (Some(proto::C_ACK), 12..) => {
                            on_ack(&b, &app, &link);
                            waker.unpark();
                        }
                        // Round trip of the data channel itself (path choice).
                        (Some(proto::C_PING), 16..) => {
                            if let Some(hub) = &app.rtc {
                                hub.send(link.id, proto::pong(f64::from_le_bytes(b[8..16].try_into().unwrap())));
                            }
                        }
                        _ => {}
                    },
                    PeerEvent::Input(b) => {
                        if let Err(e) = handle_input(&b, &app) {
                            app.telemetry.error("input", e);
                        }
                    }
                    PeerEvent::Closed => {
                        // Frames in flight there are reported lost by the client
                        // (or expire); new ones take the WebSocket. The browser
                        // may not have noticed yet: it must stop using the channels.
                        link.rtc_open.store(false, Ordering::Release);
                        link.path.store(PATH_WS, Ordering::Release);
                        app.telemetry.client_transport(link.id, "websocket");
                        let _ = out_tx.send(proto::signal(&json!({ "type": "closed" })));
                        waker.unpark();
                    }
                }
            }
        }
    };
    let writer = async {
        while let Some(mut m) = out_rx.recv().await {
            if m[0] == proto::S_FRAME {
                // Send time is taken here so "network" excludes the hop to this task.
                m[24..32].copy_from_slice(&proto::now_us().to_le_bytes());
            }
            if tx.send(Message::Binary(m.into())).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
        _ = peer => {}
    }
    link.stop.store(true, Ordering::Release);
    waker.unpark();
    if let Some(hub) = &app.rtc {
        hub.close(link.id);
    }
    app.shared.remove_client(link.id);
    app.telemetry.client_disconnected(link.id);
}

/// WebRTC signaling from the browser: its offer (answered here) and its ICE candidates.
async fn on_signal(
    json: &[u8],
    app: &App,
    link: &ClientLink,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    peer: &mpsc::UnboundedSender<PeerEvent>,
) {
    let Some(hub) = &app.rtc else { return };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(json) else { return };
    match v["type"].as_str() {
        Some("offer") => {
            let sdp = v["sdp"].as_str().unwrap_or_default().to_string();
            match hub.open(link.id, sdp, peer.clone()).await {
                Ok(answer) => {
                    let _ = out.send(proto::signal(&json!({ "type": "answer", "sdp": answer })));
                }
                Err(e) => app.telemetry.error("webrtc offer", e),
            }
        }
        Some("candidate") => {
            if let Some(c) = v["candidate"].as_str() {
                hub.candidate(link.id, c.to_string());
            }
        }
        // The client measured both paths and picked one for frames.
        Some("path") => {
            let rtc = v["webrtc"].as_bool() == Some(true) && link.rtc_open.load(Ordering::Acquire);
            link.path.store(if rtc { PATH_RTC } else { PATH_WS }, Ordering::Release);
            app.telemetry.client_transport(link.id, if rtc { "webrtc" } else { "websocket" });
        }
        _ => {}
    }
}

/// ACK: lost frames are repaired first, then everything up to `seq` is done.
fn on_ack(b: &[u8], app: &App, link: &ClientLink) {
    let seq = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let hold_us = u32::from_le_bytes(b[8..12].try_into().unwrap()) as u64;
    let geometry = app.shared.geometry();
    for lost in b[12..].chunks_exact(4).take(b[1] as usize) {
        repair(app, link, u32::from_le_bytes(lost.try_into().unwrap()), &geometry);
    }
    link.sent.lock().unwrap().ack(seq);
    link.pacer.lock().unwrap().on_ack(seq, proto::now_us(), hold_us);
}

/// Frame `seq` never reached the client: re-send what it touched.
fn repair(app: &App, link: &ClientLink, seq: u32, geometry: &Geometry) {
    let Some(r) = link.sent.lock().unwrap().lost(seq, geometry) else { return };
    link.pacer.lock().unwrap().on_lost(seq);
    app.shared.resync(link.id, &r.tiles);
    if r.keyframe {
        link.key_request.store(true, Ordering::Release);
    }
    app.telemetry.client_lost(link.id);
}

/// How long a data-channel frame may stay unacknowledged before it counts as lost.
fn loss_timeout(link: &ClientLink) -> u64 {
    (link.pacer.lock().unwrap().base_rtt_us() * LOSS_TIMEOUT_RTTS).max(LOSS_TIMEOUT_MIN_US)
}

fn send_frame(app: &App, link: &ClientLink, out: &mpsc::UnboundedSender<Vec<u8>>, mut msg: Vec<u8>, over_rtc: bool) -> bool {
    match (&app.rtc, over_rtc) {
        (Some(hub), true) => {
            // The WebSocket writer stamps its own frames; this path is stamped here.
            msg[24..32].copy_from_slice(&proto::now_us().to_le_bytes());
            for chunk in proto::chunks(&msg) {
                hub.send(link.id, chunk);
            }
            true
        }
        _ => out.send(msg).is_ok(),
    }
}

/// Client thread: pulls frames whenever the link window allows, parks otherwise.
fn produce(app: &App, link: &ClientLink, out: &mpsc::UnboundedSender<Vec<u8>>) {
    let shared = &app.shared;
    let mut codec = ClientCodec::new(app.settings.read().unwrap().tuning);
    let mut encoder_pref = String::new();
    let mut size = (0, 0);
    let mut need_key = true;
    let mut video_cap = VideoCap::default();
    let mut path = PATH_WS;
    while !link.stop.load(Ordering::Acquire) {
        let current = link.path.load(Ordering::Acquire);
        if current != path {
            // Another path is another link: measured afresh.
            path = current;
            *link.pacer.lock().unwrap() = Pacer::default();
        }
        // Frames left on an abandoned data channel whose ACK never came (the
        // channel died with them, the last ones revealed by no later frame).
        // On the channel in use they are only late: it is reliable.
        let abandoned = path == PATH_WS;
        if abandoned {
            let expired = link.sent.lock().unwrap().older_than(proto::now_us().saturating_sub(loss_timeout(link)));
            if !expired.is_empty() {
                let geometry = shared.geometry();
                for seq in expired {
                    repair(app, link, seq, &geometry);
                }
            }
        }
        let (can_send, model, rtt_us) = {
            let mut p = link.pacer.lock().unwrap();
            let can_send = p.can_send(proto::now_us());
            if !can_send && p.window_full() {
                // Woken (new content) while the estimate's window is full.
                p.note_limited();
            }
            (can_send, Link { rate: p.rate(), proven: p.proven() }, p.base_rtt_us())
        };
        {
            let s = app.settings.read().unwrap();
            codec.tuning = s.tuning;
            if s.encoder.preferred != encoder_pref && size != (0, 0) {
                // Preference changed at runtime: rebuild the encoder.
                encoder_pref = s.encoder.preferred.clone();
                drop(s);
                video_cap.reset(model.budget(&codec.tuning));
                create_encoder(app, link.id, &mut codec, size, &encoder_pref, video_cap.bps);
                shared.drop_video(link.id);
                need_key = true;
            }
        }
        let budget = model.budget(&codec.tuning);
        if let Some(c) = shared.take_cursor(link.id) {
            if out.send(c.to_vec()).is_err() {
                return;
            }
        }
        if let Some(e) = codec.encoder.as_mut() {
            if let Some(bps) = video_cap.update(budget, proto::now_us()) {
                e.set_max_bitrate(bps);
            }
        }
        let mut wake_at_us = None;
        // Nothing of a new display before the client applied its HELLO.
        if can_send && link.hello_acked.load(Ordering::Acquire) {
            need_key |= link.key_request.swap(false, Ordering::AcqRel);
            let pulled = shared.pull(link.id, &mut codec, need_key, model);
            if codec.paced() {
                link.pacer.lock().unwrap().note_limited();
            }
            match pulled {
                Ok(Pull::Frame { msg, key, contents }) => {
                    need_key &= !key;
                    let seq = u32::from_le_bytes(msg[4..8].try_into().unwrap());
                    let now = proto::now_us();
                    let over_rtc = path == PATH_RTC;
                    link.pacer.lock().unwrap().on_send(seq, msg.len() as u64, now);
                    link.sent.lock().unwrap().push(seq, now, over_rtc, contents);
                    app.telemetry.client_sent(link.id, msg.len());
                    app.telemetry.client_link(link.id, model, rtt_us, codec.encoder.as_ref().map(|_| video_cap.bps));
                    if !send_frame(app, link, out, msg, over_rtc) {
                        return;
                    }
                    continue;
                }
                Ok(Pull::Reset { geometry: g, first_seq }) => {
                    size = (g.width, g.height);
                    encoder_pref = app.settings.read().unwrap().encoder.preferred.clone();
                    video_cap.reset(budget);
                    create_encoder(app, link.id, &mut codec, size, &encoder_pref, video_cap.bps);
                    need_key = true;
                    // Frames of the previous display will never be acked meaningfully.
                    link.pacer.lock().unwrap().forget_inflight();
                    link.sent.lock().unwrap().clear();
                    link.hello_acked.store(false, Ordering::Release);
                    if out.send(proto::hello(g.width, g.height, g.tile, first_seq)).is_err() {
                        return;
                    }
                    continue;
                }
                Ok(Pull::Idle { wake_at_us: w }) => wake_at_us = w,
                Err(e) => {
                    app.telemetry.error("video encode (falling back to lossless)", e);
                    codec.encoder = None;
                    app.telemetry.client_encoder(link.id, None, Some("encoder failed".into()));
                    shared.drop_video(link.id);
                    continue;
                }
            }
        }
        // Wake when the oldest abandoned data-channel frame would expire.
        if let Some(oldest) = link.sent.lock().unwrap().oldest_rtc_us().filter(|_| abandoned) {
            let t = oldest + loss_timeout(link);
            wake_at_us = Some(wake_at_us.map_or(t, |w: u64| w.min(t)));
        }
        match wake_at_us {
            Some(t) => std::thread::park_timeout(Duration::from_micros(t.saturating_sub(proto::now_us()))),
            None => std::thread::park(),
        }
    }
}

/// Video bitrate cap following the link estimate, with hysteresis.
#[derive(Default)]
struct VideoCap {
    bps: u64,
    changed_us: u64,
}

impl VideoCap {
    fn target(rate: f64) -> u64 {
        (rate * 8.0 * VIDEO_SHARE) as u64
    }

    fn reset(&mut self, rate: f64) {
        self.bps = Self::target(rate);
        self.changed_us = proto::now_us();
    }

    /// New cap when the target moved out of the dead band (or collapsed).
    fn update(&mut self, rate: f64, now: u64) -> Option<u64> {
        let target = Self::target(rate);
        let ratio = target as f64 / self.bps.max(1) as f64;
        let shifted = ratio < CAP_DEADBAND.0 || ratio > CAP_DEADBAND.1;
        if ratio < CAP_COLLAPSE || (shifted && now.saturating_sub(self.changed_us) >= CAP_HOLD_US) {
            self.bps = target;
            self.changed_us = now;
            return Some(target);
        }
        None
    }
}

fn create_encoder(app: &App, id: u64, codec: &mut ClientCodec, (width, height): (u32, u32), pref: &str, max_bitrate: u64) {
    codec.encoder = None;
    app.shared.set_gpu_input(id, false);
    let req = EncoderRequest {
        width,
        height,
        preferred: (pref != "auto" && !pref.is_empty()).then(|| pref.to_string()),
        surface: app.shared.surface(),
        max_bitrate,
    };
    match app.platform.encoders().create(&req) {
        Ok(e) => {
            app.telemetry.client_encoder(id, Some(e.name()), e.fallback_reason());
            app.shared.set_gpu_input(id, e.gpu_input());
            codec.encoder = Some(e);
        }
        Err(e) => {
            app.telemetry.client_encoder(id, None, Some(format!("{e:#}")));
            app.telemetry.error("video encoder unavailable, lossless only", e);
        }
    }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn handle_input(b: &[u8], app: &App) -> Result<()> {
    let Some(input) = app.control.input() else { return Ok(()) };
    match (b.first().copied(), b.len()) {
        (Some(proto::C_MOVE), 6..) => input.motion(u16_at(b, 2) as u32, u16_at(b, 4) as u32)?,
        (Some(proto::C_BUTTON), 3..) => {
            // Wire format uses X button numbers.
            let button = match b[1] {
                1 => Button::Left,
                2 => Button::Middle,
                3 => Button::Right,
                8 => Button::Back,
                9 => Button::Forward,
                _ => return Ok(()),
            };
            input.button(button, b[2] != 0)?;
        }
        (Some(proto::C_WHEEL), 6..) => input.wheel(u16_at(b, 2) as i16 as i32, u16_at(b, 4) as i16 as i32)?,
        (Some(proto::C_KEY), 4..) => input.key(u16_at(b, 2), b[1] != 0)?,
        _ => {}
    }
    Ok(())
}
