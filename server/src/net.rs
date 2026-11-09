//! HTTP (static client) + WebSocket session transport.
//!
//! Per client: a dedicated OS thread builds and encodes frames (blocking work,
//! woken by capture / ACKs via unpark), an async writer drains its output, and
//! an async reader handles pings, ACKs and input.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue};
use axum::response::Response;
use axum::routing::get;
use axum::serve::ListenerExt;
use axum::Router;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::input::Injector;
use crate::proto;
use crate::shared::{Pull, Shared};
use crate::video::VideoEncoder;

/// Frames sent but not yet acknowledged by the client. Caps queueing in
/// socket buffers so latency stays bounded when the link is slower than the
/// screen changes; the pull model coalesces the skipped updates.
const MAX_INFLIGHT: i32 = 2;

pub struct App {
    pub shared: Arc<Shared>,
    pub injector: Injector,
}

pub async fn serve(addr: SocketAddr, web_dir: PathBuf, app: Arc<App>) -> Result<()> {
    // Cross-origin isolation gives the client 5 µs timer resolution instead of 100 µs.
    let router = Router::new()
        .route("/ws", get(stream_handler))
        .route("/input", get(input_handler))
        .fallback_service(ServeDir::new(web_dir))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-embedder-policy"),
            HeaderValue::from_static("require-corp"),
        ))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(addr).await?.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    eprintln!("listening on http://{addr}");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn stream_handler(ws: WebSocketUpgrade, State(app): State<Arc<App>>) -> Response {
    ws.write_buffer_size(0).on_upgrade(move |socket| session(socket, app))
}

/// Input gets its own socket so events never queue behind frame data or ACKs.
async fn input_handler(ws: WebSocketUpgrade, State(app): State<Arc<App>>) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if let Message::Binary(b) = msg {
                if let Err(e) = handle_input(&b, &app.injector) {
                    eprintln!("input error: {e:#}");
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

async fn session(socket: WebSocket, app: Arc<App>) {
    let (mut tx, mut rx) = socket.split();
    let g = app.shared.geometry();
    if tx.send(Message::Binary(proto::hello(g.width, g.height, g.tile).into())).await.is_err() {
        return;
    }
    let link = Arc::new(ClientLink {
        id: app.shared.new_client_id(),
        inflight: AtomicI32::new(0),
        key_request: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Bytes>();
    let producer = {
        let (shared, link, out_tx) = (app.shared.clone(), link.clone(), out_tx.clone());
        std::thread::Builder::new()
            .name(format!("client-{}", link.id))
            .spawn(move || produce(&shared, &link, &out_tx))
            .expect("spawn client thread")
    };
    let waker = producer.thread().clone();
    app.shared.add_client(link.id, waker.clone());
    eprintln!("client {} connected", link.id);

    let reader = {
        let (app, link, waker) = (app.clone(), link.clone(), waker.clone());
        async move {
            while let Some(Ok(msg)) = rx.next().await {
                let Message::Binary(b) = msg else { continue };
                match (b.first().copied(), b.len()) {
                    (Some(proto::C_PING), 16..) => {
                        let ts = f64::from_le_bytes(b[8..16].try_into().unwrap());
                        let _ = out_tx.send(proto::pong(ts).into());
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
                    _ => {
                        if let Err(e) = handle_input(&b, &app.injector) {
                            eprintln!("input error: {e:#}");
                        }
                    }
                }
            }
        }
    };
    let writer = async {
        while let Some(m) = out_rx.recv().await {
            if tx.send(Message::Binary(m)).await.is_err() {
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
    eprintln!("client {} disconnected", link.id);
}

/// Client thread: pulls frames whenever the window allows, parks otherwise.
fn produce(shared: &Shared, link: &ClientLink, out: &mpsc::UnboundedSender<Bytes>) {
    let g = shared.geometry();
    let mut enc = match VideoEncoder::new(g.width, g.height) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("video layer disabled: {e:#}");
            None
        }
    };
    let mut need_key = true;
    while !link.stop.load(Ordering::Acquire) {
        if let Some(c) = shared.take_cursor(link.id) {
            if out.send(Bytes::copy_from_slice(&c)).is_err() {
                return;
            }
        }
        let mut wake_at_us = None;
        if link.inflight.load(Ordering::Acquire) < MAX_INFLIGHT {
            need_key |= link.key_request.swap(false, Ordering::AcqRel);
            match shared.pull(link.id, enc.as_mut(), need_key) {
                Ok(Pull::Frame { msg, key }) => {
                    need_key &= !key;
                    link.inflight.fetch_add(1, Ordering::AcqRel);
                    if out.send(msg.into()).is_err() {
                        return;
                    }
                    continue;
                }
                Ok(Pull::Idle { wake_at_us: w }) => wake_at_us = w,
                Err(e) => {
                    eprintln!("video encode failed, lossless only from now: {e:#}");
                    enc = None;
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

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn handle_input(b: &[u8], inj: &Injector) -> Result<()> {
    match (b.first().copied(), b.len()) {
        (Some(proto::C_MOVE), 6..) => inj.motion(u16_at(b, 2), u16_at(b, 4))?,
        (Some(proto::C_BUTTON), 3..) => inj.button(b[1], b[2] != 0)?,
        (Some(proto::C_WHEEL), 6..) => {
            let dx = u16_at(b, 2) as i16;
            let dy = u16_at(b, 4) as i16;
            for (delta, neg, pos) in [(dy, 4, 5), (dx, 6, 7)] {
                let button = if delta < 0 { neg } else { pos };
                for _ in 0..delta.unsigned_abs() {
                    inj.button(button, true)?;
                    inj.button(button, false)?;
                }
            }
        }
        (Some(proto::C_KEY), 4..) => inj.key(u16_at(b, 2), b[1] != 0)?,
        _ => {}
    }
    Ok(())
}
