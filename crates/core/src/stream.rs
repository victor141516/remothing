//! Streaming endpoints: `/ws` (frames, ACKs, pings) and `/input` (input only).
//!
//! Per client: a dedicated OS thread builds and encodes frames (blocking work,
//! woken by capture / ACKs via unpark), an async writer drains its output, and
//! an async reader handles pings, ACKs, keyframe requests and stats.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::app::App;
use crate::config::constant_time_eq;
use crate::encode::EncoderRequest;
use crate::platform::Button;
use crate::proto;
use crate::shared::{ClientCodec, Pull};

/// Frames sent but not yet acknowledged by the client. Caps queueing in
/// socket buffers so latency stays bounded when the link is slower than the
/// screen changes; the pull model coalesces the skipped updates.
const MAX_INFLIGHT: i32 = 2;

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
            if let Message::Binary(b) = msg {
                if let Err(e) = handle_input(&b, &app) {
                    app.telemetry.error("input", e);
                }
            }
        }
    })
}

struct ClientLink {
    id: u64,
    inflight: AtomicI32,
    key_request: AtomicBool,
    stop: AtomicBool,
}

async fn session(socket: WebSocket, app: Arc<App>, addr: SocketAddr) {
    let (mut tx, mut rx) = socket.split();
    let link = Arc::new(ClientLink {
        id: app.shared.new_client_id(),
        inflight: AtomicI32::new(0),
        key_request: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
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

    let reader = {
        let (app, link, waker) = (app.clone(), link.clone(), waker.clone());
        async move {
            while let Some(Ok(msg)) = rx.next().await {
                let Message::Binary(b) = msg else { continue };
                match (b.first().copied(), b.len()) {
                    (Some(proto::C_PING), 16..) => {
                        let ts = f64::from_le_bytes(b[8..16].try_into().unwrap());
                        let _ = out_tx.send(proto::pong(ts));
                    }
                    (Some(proto::C_ACK), 8..) => {
                        // Signed: a client acking unknown frames only hurts its own pacing.
                        link.inflight.fetch_sub(1, Ordering::AcqRel);
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
                    _ => {
                        if let Err(e) = handle_input(&b, &app) {
                            app.telemetry.error("input", e);
                        }
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
    }
    link.stop.store(true, Ordering::Release);
    waker.unpark();
    app.shared.remove_client(link.id);
    app.telemetry.client_disconnected(link.id);
}

/// Client thread: pulls frames whenever the window allows, parks otherwise.
fn produce(app: &App, link: &ClientLink, out: &mpsc::UnboundedSender<Vec<u8>>) {
    let shared = &app.shared;
    let mut codec = ClientCodec::new(app.settings.read().unwrap().tuning);
    let mut encoder_pref = String::new();
    let mut size = (0, 0);
    let mut need_key = true;
    while !link.stop.load(Ordering::Acquire) {
        {
            let s = app.settings.read().unwrap();
            codec.tuning = s.tuning;
            if s.encoder.preferred != encoder_pref && size != (0, 0) {
                // Preference changed at runtime: rebuild the encoder.
                encoder_pref = s.encoder.preferred.clone();
                drop(s);
                create_encoder(app, link.id, &mut codec, size, &encoder_pref);
                shared.drop_video(link.id);
                need_key = true;
            }
        }
        if let Some(c) = shared.take_cursor(link.id) {
            if out.send(c.to_vec()).is_err() {
                return;
            }
        }
        let mut wake_at_us = None;
        if link.inflight.load(Ordering::Acquire) < MAX_INFLIGHT {
            need_key |= link.key_request.swap(false, Ordering::AcqRel);
            match shared.pull(link.id, &mut codec, need_key) {
                Ok(Pull::Frame { msg, key }) => {
                    need_key &= !key;
                    link.inflight.fetch_add(1, Ordering::AcqRel);
                    app.telemetry.client_sent(link.id, msg.len());
                    if out.send(msg).is_err() {
                        return;
                    }
                    continue;
                }
                Ok(Pull::Reset(g)) => {
                    size = (g.width, g.height);
                    encoder_pref = app.settings.read().unwrap().encoder.preferred.clone();
                    create_encoder(app, link.id, &mut codec, size, &encoder_pref);
                    need_key = true;
                    // Frames of the previous display will never be acked meaningfully.
                    link.inflight.store(0, Ordering::Release);
                    if out.send(proto::hello(g.width, g.height, g.tile)).is_err() {
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
        match wake_at_us {
            Some(t) => std::thread::park_timeout(Duration::from_micros(t.saturating_sub(proto::now_us()))),
            None => std::thread::park(),
        }
    }
}

fn create_encoder(app: &App, id: u64, codec: &mut ClientCodec, (width, height): (u32, u32), pref: &str) {
    codec.encoder = None;
    let req = EncoderRequest {
        width,
        height,
        preferred: (pref != "auto" && !pref.is_empty()).then(|| pref.to_string()),
        surface: app.shared.surface(),
    };
    match app.platform.encoders().create(&req) {
        Ok(e) => {
            app.telemetry.client_encoder(id, Some(e.name()), e.fallback_reason());
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
