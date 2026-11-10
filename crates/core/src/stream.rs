//! Streaming endpoints: `/ws` (frames, ACKs, pings, WebRTC signaling) and
//! `/input` (input only).
//!
//! Per client: a dedicated OS thread builds and encodes frames (blocking work,
//! woken by capture / ACKs via unpark), an async writer drains its output, and
//! an async reader handles pings, ACKs, keyframe requests, stats and
//! signaling. Once the browser's peer connection is up, frames and input
//! travel over WebRTC data channels (`rtc.rs`, UDP, a direct path when one
//! exists); the WebSocket keeps control messages and is the fallback.

use std::collections::BTreeSet;
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
/// The data channel is given up when its estimate falls below this share of
/// what the WebSocket path carried before it: SCTP congestion control
/// collapses on random loss that TCP shrugs off, without ever stalling long
/// enough for the client to notice.
const RTC_SLOWER: f64 = 0.5;

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
    /// Sequence of the last keyframe sent.
    last_key: Mutex<Option<u32>>,
    /// The client's tab is hidden: nothing is sent until it shows again.
    paused: AtomicBool,
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
        last_key: Mutex::new(None),
        paused: AtomicBool::new(false),
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
    app.telemetry.event(link.id, &format!("connected from {addr}"));
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
                    (Some(proto::C_KEYFRAME), 8..) => {
                        // Sent for every video frame the client cannot decode:
                        // all but the first are answered by the keyframe that
                        // is already on its way, and each one more would be
                        // another burst.
                        let seen = u32::from_le_bytes(b[4..8].try_into().unwrap());
                        let answered = link.last_key.lock().unwrap().is_some_and(|k| (k.wrapping_sub(seen) as i32) > 0);
                        if !answered && !link.key_request.swap(true, Ordering::AcqRel) {
                            app.telemetry.event(link.id, "keyframe requested by the client");
                            waker.unpark();
                        }
                    }
                    (Some(proto::C_VISIBLE), 2..) => {
                        let paused = b[1] == 0;
                        if link.paused.swap(paused, Ordering::AcqRel) != paused {
                            app.telemetry.client_paused(link.id, paused);
                            app.telemetry.event(link.id, if paused { "paused (tab hidden)" } else { "resumed (tab visible)" });
                            waker.unpark();
                        }
                    }
                    (Some(proto::C_EVENT), _) => app.telemetry.event(link.id, &String::from_utf8_lossy(&b[1..])),
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
                        // New frames take the WebSocket; those in flight
                        // there are gone. The browser may not have noticed
                        // yet: it must stop using the channels.
                        link.rtc_open.store(false, Ordering::Release);
                        if link.path.swap(PATH_WS, Ordering::AcqRel) != PATH_WS {
                            app.telemetry.event(link.id, "path: websocket (data channel closed)");
                        }
                        app.telemetry.client_transport(link.id, "websocket");
                        let unacked = link.sent.lock().unwrap().older_than(u64::MAX);
                        repair(&app, &link, &unacked);
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
    app.telemetry.event(link.id, "disconnected");
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
        // The client measured both paths and picked one for frames; `dead`
        // when it gave up on the data channel (stalled, closed): what is
        // still unacknowledged there is repaired now instead of waiting for
        // the client to report it.
        Some("path") => {
            let rtc = v["webrtc"].as_bool() == Some(true) && link.rtc_open.load(Ordering::Acquire);
            let path = if rtc { PATH_RTC } else { PATH_WS };
            if link.path.swap(path, Ordering::AcqRel) != path {
                app.telemetry.event(link.id, if rtc { "path: webrtc" } else { "path: websocket" });
            }
            app.telemetry.client_transport(link.id, if rtc { "webrtc" } else { "websocket" });
            if !rtc && v["dead"].as_bool() == Some(true) {
                let unacked = link.sent.lock().unwrap().older_than(u64::MAX);
                repair(app, link, &unacked);
            }
        }
        _ => {}
    }
}

/// ACK: lost frames are repaired first, then everything up to `seq` is done.
fn on_ack(b: &[u8], app: &App, link: &ClientLink) {
    let seq = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let hold_us = u32::from_le_bytes(b[8..12].try_into().unwrap()) as u64;
    let lost: Vec<u32> =
        b[12..].chunks_exact(4).take(b[1] as usize).map(|s| u32::from_le_bytes(s.try_into().unwrap())).collect();
    repair(app, link, &lost);
    link.sent.lock().unwrap().ack(seq);
    let cut = {
        let mut p = link.pacer.lock().unwrap();
        p.on_ack(seq, proto::now_us(), hold_us);
        p.take_cut()
    };
    if let Some((rate, queue_us)) = cut {
        let text = format!("link estimate cut to {:.1} Mbit/s (queue {} ms)", rate * 8.0 / 1e6, queue_us / 1000);
        app.telemetry.event(link.id, &text);
    }
}

/// Frames `seqs` never reached the client: re-send what they touched.
/// Frames already repaired (reported twice) are skipped.
fn repair(app: &App, link: &ClientLink, seqs: &[u32]) {
    if seqs.is_empty() {
        return;
    }
    let geometry = app.shared.geometry();
    let (mut frames, mut tiles, mut layers, mut keyframe) = (0, BTreeSet::new(), BTreeSet::new(), false);
    for &seq in seqs {
        let Some(r) = link.sent.lock().unwrap().lost(seq, &geometry) else { continue };
        link.pacer.lock().unwrap().on_lost(seq);
        app.telemetry.client_lost(link.id);
        frames += 1;
        tiles.extend(r.tiles);
        layers.extend(r.layers);
        keyframe |= r.keyframe;
    }
    if frames == 0 {
        return;
    }
    let (tiles, layers): (Vec<u32>, Vec<u32>) = (tiles.into_iter().collect(), layers.into_iter().collect());
    app.shared.resync(link.id, &tiles, &layers);
    if keyframe {
        link.key_request.store(true, Ordering::Release);
    }
    let key = if keyframe { ", keyframe" } else { "" };
    let lost = if frames == 1 { "1 frame lost".to_string() } else { format!("{frames} frames lost") };
    app.telemetry.event(link.id, &format!("{lost}: {} tiles to repair{key}", tiles.len()));
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
    // Estimate on the WebSocket path when frames moved to the data channel
    // (0: not there, or already found slower).
    let mut ws_rate = 0.0;
    while !link.stop.load(Ordering::Acquire) {
        let current = link.path.load(Ordering::Acquire);
        if current != path {
            // Another path is another link, measured afresh from what the
            // last one showed. Falling back to the WebSocket keeps content
            // within that; the data channel is chosen for being shorter.
            path = current;
            let mut p = link.pacer.lock().unwrap();
            ws_rate = if path == PATH_RTC { p.rate() } else { 0.0 };
            *p = p.for_new_path(path == PATH_WS);
        }
        // Frames left on an abandoned data channel whose ACK never came (the
        // channel died with them, the last ones revealed by no later frame).
        // On the channel in use they are only late: it is reliable.
        let abandoned = path == PATH_WS;
        if abandoned {
            let expired = link.sent.lock().unwrap().older_than(proto::now_us().saturating_sub(loss_timeout(link)));
            repair(app, link, &expired);
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
        if model.proven && model.rate < ws_rate * RTC_SLOWER {
            let text = format!(
                "data channel carries {:.1} Mbit/s, the websocket carried {:.1}: asking the client to go back",
                model.rate * 8.0 / 1e6,
                ws_rate * 8.0 / 1e6
            );
            app.telemetry.event(link.id, &text);
            ws_rate = 0.0;
            if out.send(proto::signal(&json!({ "type": "slow" }))).is_err() {
                return;
            }
        }
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
            let before = video_cap.bps;
            if let Some(bps) = video_cap.update(budget, proto::now_us()) {
                // A keyframe with NVENC (FFmpeg resets the encoder).
                e.set_max_bitrate(bps);
                let text = format!("video cap {:.1} -> {:.1} Mbit/s", before as f64 / 1e6, bps as f64 / 1e6);
                app.telemetry.event(link.id, &text);
            }
        }
        let mut wake_at_us = None;
        // Nothing of a new display before the client applied its HELLO, and
        // nothing at all for a hidden tab: changes keep accumulating and go
        // out as one frame when it shows again.
        if can_send && link.hello_acked.load(Ordering::Acquire) && !link.paused.load(Ordering::Acquire) {
            need_key |= link.key_request.swap(false, Ordering::AcqRel);
            let pulled = shared.pull(link.id, &mut codec, need_key, model);
            if codec.paced() {
                link.pacer.lock().unwrap().note_limited();
            }
            match pulled {
                Ok(Pull::Frame { msg, key, contents }) => {
                    need_key &= !key;
                    let seq = u32::from_le_bytes(msg[4..8].try_into().unwrap());
                    if key {
                        // Requests made before this one are answered by it.
                        *link.last_key.lock().unwrap() = Some(seq);
                        link.key_request.store(false, Ordering::Release);
                        app.telemetry.event(link.id, &format!("keyframe sent ({} KB)", msg.len() / 1024));
                    }
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
