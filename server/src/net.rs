//! HTTP (static client) + WebSocket session transport.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::serve::ListenerExt;
use axum::Router;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Notify};
use axum::http::{HeaderName, HeaderValue};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::input::Injector;
use crate::proto;
use crate::shared::Shared;

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

async fn session(socket: WebSocket, app: Arc<App>) {
    let (mut tx, rx) = socket.split();
    let g = app.shared.geometry();
    if tx.send(Message::Binary(proto::hello(g.width, g.height, g.tile).into())).await.is_err() {
        return;
    }
    let notify = Arc::new(Notify::new());
    let inflight = Arc::new(AtomicI32::new(0));
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let id = app.shared.add_client(notify.clone());
    let reader = tokio::spawn(read_loop(rx, app.clone(), notify.clone(), inflight.clone(), reply_tx));
    eprintln!("client {id} connected");

    'outer: loop {
        tokio::select! {
            biased;
            reply = reply_rx.recv() => match reply {
                Some(m) => if tx.send(Message::Binary(m.into())).await.is_err() { break 'outer },
                None => break 'outer,
            },
            _ = notify.notified() => {}
        }
        if let Some(c) = app.shared.take_cursor(id) {
            if tx.send(Message::Binary(Bytes::copy_from_slice(&c))).await.is_err() {
                break;
            }
        }
        if inflight.load(Ordering::Acquire) < MAX_INFLIGHT {
            if let Some(frame) = app.shared.take_frame(id) {
                inflight.fetch_add(1, Ordering::AcqRel);
                if tx.send(Message::Binary(frame.into())).await.is_err() {
                    break;
                }
            }
        }
    }
    reader.abort();
    app.shared.remove_client(id);
    eprintln!("client {id} disconnected");
}

async fn read_loop(
    mut rx: futures_util::stream::SplitStream<WebSocket>,
    app: Arc<App>,
    notify: Arc<Notify>,
    inflight: Arc<AtomicI32>,
    reply_tx: mpsc::UnboundedSender<Vec<u8>>,
) {
    while let Some(Ok(msg)) = rx.next().await {
        let Message::Binary(b) = msg else { continue };
        match (b.first().copied(), b.len()) {
            (Some(proto::C_PING), 16..) => {
                let ts = f64::from_le_bytes(b[8..16].try_into().unwrap());
                let _ = reply_tx.send(proto::pong(ts));
            }
            (Some(proto::C_ACK), 8..) => {
                // Signed: a client acking unknown frames only hurts its own pacing.
                inflight.fetch_sub(1, Ordering::AcqRel);
                notify.notify_one();
            }
            _ => {
                if let Err(e) = handle_input(&b, &app.injector) {
                    eprintln!("input error: {e:#}");
                }
            }
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
