//! Streaming endpoints: `/ws` (frames, sound, ACKs, pings, signaling) and
//! `/input` (input only).
//!
//! Per client: a dedicated OS thread builds and encodes frames (blocking work,
//! woken by capture / ACKs via unpark), an async writer drains its output, and
//! an async reader handles pings, ACKs, keyframe requests, stats and
//! signaling. Frames and input take the first UDP path that comes up:
//! WebTransport datagrams (`wt.rs`), else a WebRTC data channel (`rtc.rs`)
//! when its round trip is clearly shorter; the WebSocket keeps control
//! messages and is the fallback.

use std::collections::{BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc, watch};

use crate::app::{App, VirtualRequest, Watching};
use crate::capture::{Clip, DisplayStream};
use crate::config::constant_time_eq;
use crate::datagrams::{self, Datagram, Fec, Kept};
use crate::encode::{Codec, EncoderRequest, VideoEncoder};
use crate::mic;
use crate::pacer::{Notice, Pacer};
use crate::platform::{Button, ThreadRole, MAX_IMAGE};
use crate::proto;
use crate::recovery::SentLog;
use crate::rtc::PeerEvent;
use crate::shared::{ClientCodec, Link, Pull, Shared, Tuning, VIDEO_SHARE};

/// Where frames go: the WebSocket, the WebRTC frames channel, or
/// WebTransport datagrams.
const PATH_WS: u8 = 0;
const PATH_RTC: u8 = 1;
const PATH_WT: u8 = 2;
/// evdev code of V, which C_PASTE presses.
const KEY_V: u16 = 47;
/// Frames kept for pieces the client asks for again (WebTransport); older
/// ones are past saving and the client gives them up.
const WT_KEEP: usize = 256;
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
/// collapsed (target under CAP_COLLAPSE of the cap). A target several times
/// the cap (the estimate during slow start, which begins low) is worth the
/// keyframe sooner.
const CAP_HOLD_US: u64 = 3_000_000;
const CAP_DEADBAND: (f64, f64) = (0.67, 1.5);
const CAP_COLLAPSE: f64 = 0.33;
const CAP_JUMP: f64 = 2.5;
const CAP_JUMP_HOLD_US: u64 = 500_000;
/// During slow start the cap is this many times the video share: frame
/// pacing keeps the bytes within the share (fewer, better pictures), and
/// video that fills it is what lets the estimate grow; capped at the share
/// it could never show that the link carries more.
const CAP_RAMP: f64 = 2.0;

/// Longest gap between messages from a live client (it pings every second).
const CLIENT_SILENCE: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
pub struct TokenQuery {
    #[serde(default)]
    token: String,
    /// Stream socket: codecs besides H.264 the client decodes in hardware,
    /// comma-separated ("hevc,av1").
    #[serde(default)]
    codecs: String,
    /// Stream socket: the display to start on (the page's address keeps it).
    #[serde(default)]
    display: String,
    /// The page's key on both sockets: input goes to the display its stream
    /// socket shows.
    #[serde(default)]
    session: String,
}

/// C_MIC_INFO.
#[derive(Deserialize)]
struct MicRequest {
    on: Option<bool>,
    #[serde(default)]
    install: bool,
    #[serde(default)]
    restart: bool,
    #[serde(default)]
    codec: String,
    #[serde(default)]
    rate: u32,
    #[serde(default)]
    channels: usize,
}

/// The stream token (any page's own requests carry it).
pub(crate) fn token_ok(app: &App, token: &str) -> bool {
    let expected = app.settings.read().unwrap().auth.stream_token.clone();
    expected.is_empty() || constant_time_eq(expected.as_bytes(), token.as_bytes())
}

/// Answers a wrong token: the upgrade goes through only to carry the close
/// code, which is all a browser lets the page see; nothing else is sent.
fn refuse(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket| async move {
        let close = CloseFrame { code: proto::CLOSE_UNAUTHORIZED, reason: "".into() };
        let _ = socket.send(Message::Close(Some(close))).await;
    })
}

pub async fn stream_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<TokenQuery>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app): State<Arc<App>>,
) -> Response {
    if !token_ok(&app, &q.token) {
        eprintln!("refused a stream connection from {addr}: wrong token");
        return refuse(ws);
    }
    let mut codecs = vec![Codec::H264];
    codecs.extend(q.codecs.split(',').filter_map(Codec::from_name).filter(|c| *c != Codec::H264));
    // A clipboard picture comes as one message, which a browser sends as one frame.
    ws.write_buffer_size(0)
        .max_frame_size(2 + MAX_IMAGE)
        .on_upgrade(move |socket| session(socket, app, addr, codecs, q.display, q.session))
}

/// Input gets its own socket so events never queue behind frame data or ACKs.
/// For the same reason its pings measure the WebSocket path's unloaded round
/// trip, which the client compares with the data channel's.
pub async fn input_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<TokenQuery>,
    State(app): State<Arc<App>>,
) -> Response {
    if !token_ok(&app, &q.token) {
        return refuse(ws);
    }
    ws.on_upgrade(move |mut socket| async move {
        let mut held = Held::default();
        // The display input last went to: what is held there is let go
        // there when the page moves on or leaves.
        let mut last: Option<Arc<DisplayStream>> = None;
        // The client pings every second: silence means it is gone, whatever
        // the socket says, and what it held down must not stay down.
        while let Ok(Some(Ok(msg))) = tokio::time::timeout(CLIENT_SILENCE, socket.recv()).await {
            let Message::Binary(b) = msg else { continue };
            if b.first() == Some(&proto::C_PING) && b.len() >= 16 {
                let pong = proto::pong(f64::from_le_bytes(b[8..16].try_into().unwrap()));
                if socket.send(Message::Binary(pong.into())).await.is_err() {
                    break;
                }
                continue;
            }
            // Before the stream socket registered the page there is nowhere to send it.
            let route = app.input_routes.lock().unwrap().get(&q.session).cloned();
            let Some(stream) = route.map(|w| w.read().unwrap().clone()) else { continue };
            if let Some(previous) = last.as_ref().filter(|l| !Arc::ptr_eq(l, &stream)) {
                held.release(previous);
            }
            if let Err(e) = handle_input(&b, &stream, &mut held) {
                app.telemetry.error("input", e);
            }
            last = Some(stream);
        }
        if let Some(stream) = &last {
            held.release(stream);
        }
    })
}

/// Keys and buttons one connection holds down. A held key repeats on the
/// remote side, so it is released when the connection that pressed it ends
/// before sending the release.
#[derive(Default)]
struct Held {
    keys: Vec<u16>,
    buttons: Vec<Button>,
    /// Sequence of the last C_MOVE_LATEST applied.
    move_seq: Option<u32>,
}

impl Held {
    fn release(&mut self, stream: &DisplayStream) {
        let Some(input) = stream.control.input() else { return };
        for key in self.keys.drain(..) {
            let _ = input.key(key, false);
        }
        for button in self.buttons.drain(..) {
            let _ = input.button(button, false);
        }
    }
}

/// The client's WebTransport session: the connection, and the queue its
/// frames' datagrams are paced from (`datagrams::pace`).
#[derive(Clone)]
struct WtLink {
    conn: wtransport::Connection,
    queue: mpsc::UnboundedSender<Datagram>,
}

struct ClientLink {
    id: u64,
    /// The display it streams (shared with its input socket's route).
    watching: Watching,
    /// Bumped when it switches display (the clipboard follows).
    switched: watch::Sender<()>,
    /// Its last wishes, kept for the next display it switches to.
    hdr: AtomicBool,
    captured: AtomicBool,
    pacer: Mutex<Pacer>,
    /// Frames in flight with what they change (repairs after a loss).
    sent: Mutex<SentLog>,
    /// The frames channel is open (the client chooses whether to use it).
    rtc_open: AtomicBool,
    /// The client's WebTransport session, while open.
    wt: Mutex<Option<WtLink>>,
    /// Frames sent as datagrams and not acknowledged yet.
    wt_frames: Mutex<VecDeque<Kept>>,
    /// Parity of the frames sent as datagrams.
    fec: Mutex<Fec>,
    key_request: AtomicBool,
    /// Sequence of the last keyframe sent.
    last_key: Mutex<Option<u32>>,
    /// The client's tab is hidden: nothing is sent until it shows again.
    paused: AtomicBool,
    /// The client applied the last HELLO: frames of that display may go out.
    hello_acked: AtomicBool,
    /// PATH_WS, PATH_RTC or PATH_WT.
    path: AtomicU8,
    stop: AtomicBool,
    /// Input that came over this connection (the data channel, or the
    /// stream socket itself).
    held: Mutex<Held>,
    /// How the client shows the picture (C_VIEW).
    view: Mutex<View>,
    /// Codecs the client decodes (H.264 always; others in hardware).
    codecs: Vec<Codec>,
    /// The client cannot show BT.601 video in its colours (C_BT709).
    bt709: AtomicBool,
}

/// Moves the client to `stream`, which it already watches (`Streams::watch`):
/// its frames continue the old display's sequence, and the first pull there
/// is a HELLO.
fn switch_display(app: &App, link: &ClientLink, waker: &std::thread::Thread, stream: Arc<DisplayStream>) {
    let mut watching = link.watching.write().unwrap();
    if Arc::ptr_eq(&watching, &stream) {
        app.streams.unwatch(&stream);
        return;
    }
    let seq = watching.shared.remove_client(link.id);
    link.held.lock().unwrap().release(&watching);
    stream.shared.add_client(link.id, waker.clone(), seq);
    stream.shared.set_client_hdr(link.id, link.hdr.load(Ordering::Acquire));
    stream.shared.set_client_capture(link.id, link.captured.load(Ordering::Acquire));
    let old = std::mem::replace(&mut *watching, stream);
    drop(watching);
    app.streams.unwatch(&old);
    link.switched.send_replace(());
    app.telemetry.event(link.id, &format!("streams display {}", link.watching.read().unwrap().id));
}

/// The picture's size in the client's window, in device pixels, and the
/// client's refresh rate (0: not reported).
#[derive(Clone, Copy, Default)]
struct View {
    width: u32,
    height: u32,
    hz: u32,
}

/// `display`: the display the client asks for (else the default); `route`:
/// the page's key, which its input socket gives too.
async fn session(socket: WebSocket, app: Arc<App>, addr: SocketAddr, codecs: Vec<Codec>, display: String, route: String) {
    let (mut tx, mut rx) = socket.split();
    let display = app.resolve_display(Some(display.as_str()).filter(|d| !d.is_empty())).unwrap_or_else(|e| {
        app.telemetry.error("list displays", e);
        app.settings.read().unwrap().server.display.clone()
    });
    let link = Arc::new(ClientLink {
        id: app.next_client.fetch_add(1, Ordering::Relaxed),
        watching: Arc::new(RwLock::new(app.streams.watch(&display))),
        switched: watch::channel(()).0,
        hdr: AtomicBool::new(false),
        captured: AtomicBool::new(false),
        pacer: Mutex::new(Pacer::default()),
        sent: Mutex::new(SentLog::default()),
        rtc_open: AtomicBool::new(false),
        wt: Mutex::new(None),
        wt_frames: Mutex::new(VecDeque::new()),
        fec: Mutex::new(Fec::default()),
        key_request: AtomicBool::new(false),
        last_key: Mutex::new(None),
        paused: AtomicBool::new(false),
        // Open: the first pull is what produces the first HELLO.
        hello_acked: AtomicBool::new(true),
        path: AtomicU8::new(PATH_WS),
        stop: AtomicBool::new(false),
        held: Mutex::new(Held::default()),
        view: Mutex::new(View::default()),
        codecs,
        bt709: AtomicBool::new(false),
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
    app.client_joined();
    app.telemetry.event(link.id, &format!("connected from {addr}"));
    let stream = link.watching.read().unwrap().clone();
    stream.shared.add_client(link.id, waker.clone(), 0);
    if !route.is_empty() {
        app.input_routes.lock().unwrap().insert(route.clone(), link.watching.clone());
    }
    if let Some(hub) = &app.rtc {
        // The browser offers a peer connection with these STUN servers.
        let ice: Vec<serde_json::Value> = hub.ice_servers.iter().map(|u| json!({ "urls": u })).collect();
        let _ = out_tx.send(proto::signal(&json!({ "type": "config", "iceServers": ice })));
    }
    // Read once: the setting applies to clients connecting after a change.
    let tracing = app.settings.read().unwrap().debug.frame_trace;
    if tracing {
        let _ = out_tx.send(proto::signal(&json!({ "type": "trace" })));
    }
    // WebTransport sessions opened with this key belong to this client.
    let wt_key = crate::config::random_token();
    let (wt_tx, mut wt_rx) = mpsc::unbounded_channel::<wtransport::Connection>();
    if let Some(hub) = &app.wt {
        hub.register(&wt_key, wt_tx);
    }
    let wt_offer = {
        let (app, out_tx, key) = (app.clone(), out_tx.clone(), wt_key.clone());
        async move {
            let Some(hub) = &app.wt else { return std::future::pending().await };
            // Again after every certificate renewal.
            let mut offer = hub.offer();
            loop {
                let o = offer.borrow_and_update().clone();
                let signal = json!({ "type": "wt", "key": key, "hash": o.hash, "port": o.port, "candidates": o.candidates });
                let _ = out_tx.send(proto::signal(&signal));
                if offer.changed().await.is_err() {
                    return;
                }
            }
        }
    };
    let wt = {
        let (app, link, waker, out_tx) = (app.clone(), link.clone(), waker.clone(), out_tx.clone());
        async move {
            while let Some(conn) = wt_rx.recv().await {
                app.telemetry.event(link.id, &format!("webtransport session from {}", conn.remote_address()));
                // A new session replaces an older one the client gave up.
                let (queue, paced) = mpsc::unbounded_channel();
                let wt = WtLink { conn, queue };
                *link.wt.lock().unwrap() = Some(wt.clone());
                *link.fec.lock().unwrap() = Fec::default();
                let why = serve_wt(&wt, paced, &app, &link, &waker).await;
                let mut current = link.wt.lock().unwrap();
                if current.as_ref().is_some_and(|c| c.conn.stable_id() == wt.conn.stable_id()) {
                    *current = None;
                }
                drop(current);
                app.telemetry.event(link.id, &format!("webtransport session closed: {why}"));
                // New frames take the WebSocket; those in flight there are gone.
                if link.path.compare_exchange(PATH_WT, PATH_WS, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    app.telemetry.client_transport(link.id, "websocket");
                    let unacked = link.sent.lock().unwrap().older_than(u64::MAX);
                    repair(&app, &link, &unacked);
                    let _ = out_tx.send(proto::signal(&json!({ "type": "wtclosed" })));
                    waker.unpark();
                }
            }
            // Without a hub no session ever comes; the client stays on the others.
            std::future::pending::<()>().await
        }
    };

    let peer_out = out_tx.clone();
    let clipboard = {
        let (link, out_tx) = (link.clone(), out_tx.clone());
        let mut switched = link.switched.subscribe();
        async move {
            // The clipboard of the display the client streams: what is there
            // on connect and after a switch, then every change this client
            // did not make. Changes copied after that, by anyone, the client
            // also puts on its own computer's clipboard. Copied files go as
            // a list the client can download them from.
            loop {
                let control = link.watching.read().unwrap().control.clone();
                let (mut content, mut files) = (control.clipboard_content(), control.clipboard_files());
                let (mut first, mut send_content, mut send_files) = (true, true, true);
                loop {
                    if send_content {
                        let now = content.borrow_and_update().clone();
                        let kind = match &now.clip {
                            Clip::Text(_) => proto::S_CLIPBOARD,
                            Clip::Image(_) => proto::S_CLIPBOARD_IMAGE,
                        };
                        let bytes = now.clip.bytes();
                        if now.by != Some(link.id) && !bytes.is_empty() {
                            let flags = if now.changed && !first { proto::CLIPBOARD_CHANGED } else { 0 };
                            let mut m = Vec::with_capacity(2 + bytes.len());
                            m.extend_from_slice(&[kind, flags]);
                            m.extend_from_slice(bytes);
                            let _ = out_tx.send(m);
                        }
                        first = false;
                    }
                    if send_files {
                        let list = files.borrow_and_update().clone();
                        let listing = tokio::task::block_in_place(|| crate::files::listing(&list));
                        let _ = out_tx.send(proto::text(proto::S_CLIPBOARD_FILES, &listing.to_string()));
                    }
                    (send_content, send_files) = (false, false);
                    tokio::select! {
                        changed = content.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            send_content = true;
                        }
                        changed = files.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            send_files = true;
                        }
                        _ = switched.changed() => break,
                    }
                }
            }
        }
    };
    // Sound, while the client asks for it.
    let (audio_on, mut audio_wanted) = watch::channel(false);
    let audio = {
        let (app, out_tx) = (app.clone(), out_tx.clone());
        async move {
            while audio_wanted.wait_for(|&on| on).await.is_ok() {
                let mut listener = app.audio.listen();
                // What the stream is right now is news to this client.
                listener.info.mark_changed();
                while *audio_wanted.borrow() {
                    tokio::select! {
                        changed = audio_wanted.changed() => {
                            if changed.is_err() {
                                return;
                            }
                        }
                        changed = listener.info.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            if let Some(info) = listener.info.borrow_and_update().clone() {
                                let _ = out_tx.send(info.to_vec());
                            }
                        }
                        packet = listener.packets.recv() => match packet {
                            Ok(packet) => {
                                let _ = out_tx.send(packet.to_vec());
                            }
                            // This client fell behind: what it missed is old by now.
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => return,
                        },
                    }
                }
            }
        }
    };
    // The display list again whenever displays come, change or go (another
    // window made one, say): every window's menu lists them all.
    let display_list = {
        let (app, link, out_tx) = (app.clone(), link.clone(), out_tx.clone());
        let mut changes = app.streams.list_changes();
        async move {
            while changes.changed().await.is_ok() {
                send_displays(&app, &out_tx, &link, None);
            }
            std::future::pending::<()>().await
        }
    };
    // Whether the microphone is free or another client has it. What happens
    // to this client's own request goes to it directly (`MicHub::start`).
    let mic_state = {
        let (mut talker, out_tx, id) = (app.mic.talker(), out_tx.clone(), link.id);
        async move {
            loop {
                let state = match *talker.borrow_and_update() {
                    Some(talker) if talker == id => None,
                    Some(_) => Some("busy"),
                    None => Some("free"),
                };
                if let Some(state) = state {
                    let _ = out_tx.send(proto::text(proto::S_MIC_INFO, &json!({ "state": state }).to_string()));
                }
                if talker.changed().await.is_err() {
                    return;
                }
            }
        }
    };
    let reader = {
        let (app, link, waker) = (app.clone(), link.clone(), waker.clone());
        async move {
            // This client's hold on the session's microphone, while it talks.
            let mut microphone: Option<std::sync::mpsc::Sender<mic::Command>> = None;
            // Clients ping every second; a silent peer (half-open connection,
            // e.g. through a port forwarder) is dropped so it stops holding an encoder.
            while let Ok(Some(Ok(msg))) = tokio::time::timeout(CLIENT_SILENCE, rx.next()).await {
                let Message::Binary(b) = msg else { continue };
                match (b.first().copied(), b.len()) {
                    (Some(proto::C_PING), 16..) => {
                        let ts = f64::from_le_bytes(b[8..16].try_into().unwrap());
                        let _ = out_tx.send(proto::pong(ts));
                    }
                    (Some(proto::C_ACK), 20..) => {
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
                    (Some(proto::C_CLIPBOARD | proto::C_PASTE | proto::C_CLIPBOARD_IMAGE), _) => {
                        let (clip, paste) = received_clip(&b);
                        let stream = link.watching.read().unwrap().clone();
                        if !paste {
                            // Setting waits for the platform to have it: a moment,
                            // and in order with the next one.
                            let set = tokio::task::block_in_place(|| stream.control.set_clipboard(link.id, clip));
                            if let Err(e) = set {
                                app.telemetry.error("clipboard", e);
                            }
                            continue;
                        }
                        let (app, out, id) = (app.clone(), out_tx.clone(), link.id);
                        // Setting the clipboard waits for the platform to have
                        // it, which the reader, carrying ACKs, must not.
                        tokio::task::spawn_blocking(move || {
                            // Empty: this computer had nothing to copy; the remote
                            // clipboard is pasted as it is.
                            let set = if clip.bytes().is_empty() { Ok(()) } else { stream.control.set_clipboard(id, clip) };
                            // The client still holds Ctrl down on the remote side
                            // and waits for S_PASTED before sending more keys.
                            let pressed = set.and_then(|()| {
                                let input = stream.control.input().context("no input on this display")?;
                                input.key(KEY_V, true)?;
                                input.key(KEY_V, false)
                            });
                            if let Err(e) = pressed {
                                app.telemetry.error("paste", e);
                            }
                            let _ = out.send(vec![proto::S_PASTED]);
                        });
                    }
                    (Some(proto::C_TRACE), _) if tracing => {
                        if let Ok(v) = serde_json::from_slice(&b[1..]) {
                            app.telemetry.client_trace(link.id, v);
                        }
                    }
                    (Some(proto::C_BT709), _) => {
                        if !link.bt709.swap(true, Ordering::AcqRel) {
                            waker.unpark();
                        }
                    }
                    (Some(proto::C_AUDIO), 2..) => {
                        audio_on.send_replace(b[1] != 0);
                    }
                    (Some(proto::C_MIC_INFO), _) => {
                        let Ok(request) = serde_json::from_slice::<MicRequest>(&b[1..]) else { continue };
                        match request.on {
                            // The first to ask gets it; the others see it taken (`mic_state`).
                            // An earlier request of this client may have ended by itself
                            // (no microphone to offer): asking again starts over.
                            Some(true) if !app.mic.talks(link.id) => {
                                let format = mic::Format { codec: request.codec, rate: request.rate, channels: request.channels };
                                microphone = app.mic.start(link.id, format, out_tx.clone());
                            }
                            Some(false) => microphone = None,
                            _ => {}
                        }
                        if let Some(microphone) = &microphone {
                            if request.install {
                                let _ = microphone.send(mic::Command::Install);
                            }
                            if request.restart {
                                let _ = microphone.send(mic::Command::Restart);
                            }
                        }
                    }
                    (Some(proto::C_MIC), _) => {
                        // The sender is gone once the microphone gave up by itself.
                        if microphone.as_ref().is_some_and(|m| m.send(mic::Command::Packet(b.to_vec())).is_err()) {
                            microphone = None;
                        }
                    }
                    (Some(proto::C_DISPLAYS), _) => send_displays(&app, &out_tx, &link, None),
                    (Some(proto::C_DISPLAY), _) => {
                        let id = String::from_utf8_lossy(&b[1..]).into_owned();
                        match app.platform.displays().list() {
                            Ok(list) if list.iter().any(|d| d.id == id) => {
                                switch_display(&app, &link, &waker, app.streams.watch(&id));
                                send_displays(&app, &out_tx, &link, None);
                            }
                            Ok(_) => app.telemetry.event(link.id, &format!("asked for display {id}, which is not there")),
                            Err(e) => app.telemetry.error("list displays", e),
                        }
                    }
                    (Some(proto::C_VIRTUAL), _) => {
                        let request = serde_json::from_slice::<VirtualRequest>(&b[1..]);
                        let (app, out, link, waker) = (app.clone(), out_tx.clone(), link.clone(), waker.clone());
                        // Installing a driver or adding a display takes seconds:
                        // not on the reader, which carries ACKs and input.
                        tokio::task::spawn_blocking(move || {
                            let done = request.map_err(anyhow::Error::from).and_then(|r| {
                                app.telemetry.event(link.id, &format!("virtual display: {r:?}"));
                                app.virtual_request(r)
                            });
                            match done {
                                Ok(created) => {
                                    // A new display: this client streams it.
                                    if let Some(id) = created {
                                        switch_display(&app, &link, &waker, app.streams.watch(&id));
                                    }
                                    send_displays(&app, &out, &link, None);
                                }
                                Err(e) => {
                                    app.telemetry.event(link.id, &format!("virtual display request failed: {e:#}"));
                                    send_displays(&app, &out, &link, Some(format!("{e:#}")));
                                }
                            }
                        });
                    }
                    (Some(proto::C_STATS), _) => {
                        if let Ok(v) = serde_json::from_slice(&b[1..]) {
                            app.telemetry.client_report(link.id, v);
                        }
                    }
                    (Some(proto::C_VIEW), 8..) => {
                        let view = View { width: u16_at(&b, 2) as u32, height: u16_at(&b, 4) as u32, hz: u16_at(&b, 6) as u32 };
                        let hdr = b[1] & proto::VIEW_HDR != 0;
                        let shows = if hdr { ", HDR" } else { "" };
                        app.telemetry.event(link.id, &format!("view {}x{} at {} Hz{shows}", view.width, view.height, view.hz));
                        *link.view.lock().unwrap() = view;
                        link.hdr.store(hdr, Ordering::Release);
                        link.watching.read().unwrap().shared.set_client_hdr(link.id, hdr);
                        waker.unpark();
                    }
                    (Some(proto::C_CAPTURE), 2..) => {
                        let on = b[1] != 0;
                        app.telemetry.event(link.id, if on { "mouse captured" } else { "mouse released" });
                        link.captured.store(on, Ordering::Release);
                        link.watching.read().unwrap().shared.set_client_capture(link.id, on);
                        app.streams.set_capturing(link.id, on);
                    }
                    (Some(proto::C_HELLO_ACK), _) => {
                        link.hello_acked.store(true, Ordering::Release);
                        waker.unpark();
                    }
                    (Some(proto::C_SIGNAL), _) => on_signal(&b[1..], &app, &link, &out_tx, &peer_tx).await,
                    _ => {
                        let stream = link.watching.read().unwrap().clone();
                        if let Err(e) = handle_input(&b, &stream, &mut link.held.lock().unwrap()) {
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
                        (Some(proto::C_ACK), 20..) => {
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
                        let stream = link.watching.read().unwrap().clone();
                        if let Err(e) = handle_input(&b, &stream, &mut link.held.lock().unwrap()) {
                            app.telemetry.error("input", e);
                        }
                    }
                    PeerEvent::Closed => {
                        // New frames take the WebSocket; those in flight
                        // there are gone. The browser may not have noticed
                        // yet: it must stop using the channels.
                        link.rtc_open.store(false, Ordering::Release);
                        if link.path.compare_exchange(PATH_RTC, PATH_WS, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                            app.telemetry.event(link.id, "path: websocket (data channel closed)");
                            app.telemetry.client_transport(link.id, "websocket");
                            let unacked = link.sent.lock().unwrap().older_than(u64::MAX);
                            repair(&app, &link, &unacked);
                        }
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
        _ = clipboard => {}
        _ = audio => {}
        _ = mic_state => {}
        _ = display_list => {}
        _ = wt_offer => {}
        _ = wt => {}
    }
    link.stop.store(true, Ordering::Release);
    link.held.lock().unwrap().release(&link.watching.read().unwrap());
    waker.unpark();
    if let Some(hub) = &app.rtc {
        hub.close(link.id);
    }
    if let Some(hub) = &app.wt {
        hub.unregister(&wt_key);
    }
    if let Some(wt) = link.wt.lock().unwrap().take() {
        wt.conn.close(0u32.into(), b"session ended");
    }
    // A reconnect of the same page may have registered its route already.
    let mut routes = app.input_routes.lock().unwrap();
    if routes.get(&route).is_some_and(|w| Arc::ptr_eq(w, &link.watching)) {
        routes.remove(&route);
    }
    drop(routes);
    let stream = link.watching.read().unwrap().clone();
    stream.shared.remove_client(link.id);
    app.streams.unwatch(&stream);
    app.streams.set_capturing(link.id, false);
    app.telemetry.client_disconnected(link.id);
    app.client_left();
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
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(json) else { return };
    match v["type"].as_str() {
        Some("offer") => {
            let sdp = v["sdp"].as_str().unwrap_or_default().to_string();
            let Some(hub) = &app.rtc else { return };
            match hub.open(link.id, sdp, peer.clone()).await {
                Ok(answer) => {
                    let _ = out.send(proto::signal(&json!({ "type": "answer", "sdp": answer })));
                }
                Err(e) => app.telemetry.error("webrtc offer", e),
            }
        }
        Some("candidate") => {
            if let (Some(hub), Some(c)) = (&app.rtc, v["candidate"].as_str()) {
                hub.candidate(link.id, c.to_string());
            }
        }
        // The client picked a path for frames; `dead` when it gave up the
        // UDP path it was on (stalled, closed): what is still
        // unacknowledged there is repaired now instead of waiting for the
        // client to report it.
        Some("path") => {
            let path = match v["path"].as_str() {
                Some("webrtc") if link.rtc_open.load(Ordering::Acquire) => PATH_RTC,
                Some("wt") if link.wt.lock().unwrap().is_some() => PATH_WT,
                _ => PATH_WS,
            };
            let name = transport_name(path);
            if link.path.swap(path, Ordering::AcqRel) != path {
                app.telemetry.event(link.id, &format!("path: {name}"));
            }
            app.telemetry.client_transport(link.id, name);
            if path == PATH_WS && v["dead"].as_bool() == Some(true) {
                let unacked = link.sent.lock().unwrap().older_than(u64::MAX);
                repair(app, link, &unacked);
            }
        }
        _ => {}
    }
}

/// What a C_CLIPBOARD, C_PASTE or C_CLIPBOARD_IMAGE carries for the
/// session's clipboard, and whether it is for a Ctrl+V.
fn received_clip(b: &[u8]) -> (Clip, bool) {
    match b[0] {
        proto::C_CLIPBOARD_IMAGE => {
            let paste = b.get(1).is_some_and(|flags| flags & proto::CLIPBOARD_PASTE != 0);
            (Clip::Image(Arc::from(b.get(2..).unwrap_or_default())), paste)
        }
        tag => (Clip::Text(Arc::from(String::from_utf8_lossy(&b[1..]).as_ref())), tag == proto::C_PASTE),
    }
}

/// ACK: lost frames are repaired first, then everything up to `seq` is done.
fn on_ack(b: &[u8], app: &App, link: &ClientLink) {
    let seq = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let hold_us = u32::from_le_bytes(b[8..12].try_into().unwrap()) as u64;
    let arrived = Some(u64::from_le_bytes(b[12..20].try_into().unwrap())).filter(|&t| t > 0);
    let lost: Vec<u32> =
        b[20..].chunks_exact(4).take(b[1] as usize).map(|s| u32::from_le_bytes(s.try_into().unwrap())).collect();
    repair(app, link, &lost);
    link.sent.lock().unwrap().ack(seq);
    // Nothing up to `seq` will be asked for again.
    link.wt_frames.lock().unwrap().retain(|k| (k.seq.wrapping_sub(seq) as i32) > 0);
    let (cut, notice) = {
        let mut p = link.pacer.lock().unwrap();
        p.on_ack(seq, proto::now_us(), hold_us, b[2] != 0, arrived);
        (p.take_cut(), p.take_notice())
    };
    if let Some((rate, queue_us)) = cut {
        link.fec.lock().unwrap().on_queue(proto::now_us());
        let text = format!("link estimate cut to {:.1} Mbit/s (queue {} ms)", rate * 8.0 / 1e6, queue_us / 1000);
        app.telemetry.event(link.id, &text);
    }
    let text = match notice {
        Some(Notice::Undone { rate, path_delay_us }) => format!(
            "link estimate back to {:.1} Mbit/s: the delay stayed after the cut, the path adds {} ms whatever is sent",
            rate * 8.0 / 1e6,
            path_delay_us / 1000
        ),
        Some(Notice::Retried { rate }) => format!("link estimate retried at {:.1} Mbit/s, where it was before its cuts", rate * 8.0 / 1e6),
        None => return,
    };
    app.telemetry.event(link.id, &text);
}

/// Serves one WebTransport session until it closes; returns why it closed.
/// ACKs, pings and requests for pieces come as datagrams; input on a
/// unidirectional stream, each message after its length (u16).
async fn serve_wt(
    wt: &WtLink,
    paced: mpsc::UnboundedReceiver<Datagram>,
    app: &App,
    link: &ClientLink,
    waker: &std::thread::Thread,
) -> String {
    let conn = &wt.conn;
    let received = async {
        loop {
            let d = match conn.receive_datagram().await {
                Ok(d) => d,
                Err(e) => return e.to_string(),
            };
            match (d.first().copied(), d.len()) {
                (Some(proto::C_ACK), 20..) => {
                    on_ack(&d, app, link);
                    waker.unpark();
                }
                // Unordered moves (the ordered ones come on the input stream).
                (Some(proto::C_MOVE_LATEST), 10..) => {
                    let stream = link.watching.read().unwrap().clone();
                    if let Err(e) = handle_input(&d, &stream, &mut link.held.lock().unwrap()) {
                        app.telemetry.error("input", e);
                    }
                }
                // Round trip of this path (clock offset, health).
                (Some(proto::C_PING), 16..) => {
                    let _ = conn.send_datagram(proto::pong(f64::from_le_bytes(d[8..16].try_into().unwrap())));
                }
                (Some(proto::C_NACK), 8..) => {
                    let seq = u32::from_le_bytes(d[4..8].try_into().unwrap());
                    let count = u16::from_le_bytes([d[2], d[3]]) as usize;
                    let mut out = Vec::new();
                    let mut frames = link.wt_frames.lock().unwrap();
                    let Some(frame) = frames.iter_mut().find(|k| k.seq == seq) else { continue };
                    let asked = if count == 0 {
                        // Not one piece arrived: the client knows nothing of its size.
                        frame.repair(None, &mut out)
                    } else {
                        let mut asked = d[8..].chunks_exact(2).take(count).map(|c| u16::from_le_bytes([c[0], c[1]]) as usize);
                        frame.repair(Some(&mut asked), &mut out)
                    };
                    drop(frames);
                    link.fec.lock().unwrap().on_asked(asked);
                    for d in out {
                        let _ = wt.queue.send(d);
                    }
                }
                _ => {}
            }
        }
    };
    let input = async {
        loop {
            let mut stream = match conn.accept_uni().await {
                Ok(s) => s,
                Err(e) => return e.to_string(),
            };
            // The client opens one per session.
            let (mut len, mut msg) = ([0u8; 2], Vec::new());
            while stream.read_exact(&mut len).await.is_ok() {
                msg.resize(u16::from_le_bytes(len) as usize, 0);
                if stream.read_exact(&mut msg).await.is_err() {
                    break;
                }
                let stream = link.watching.read().unwrap().clone();
                if let Err(e) = handle_input(&msg, &stream, &mut link.held.lock().unwrap()) {
                    app.telemetry.error("input", e);
                }
            }
        }
    };
    // Ahead of the estimate (`datagrams::PACE_GAIN`); unpaced until it is
    // proven, so a keyframe never crawls at a first guess.
    let rate = || {
        let p = link.pacer.lock().unwrap();
        p.proven().then(|| p.rate() * datagrams::PACE_GAIN)
    };
    tokio::select! {
        why = received => why,
        why = input => why,
        why = datagrams::pace(conn, paced, rate) => why,
    }
}

/// Frames `seqs` never reached the client: re-send what they touched.
/// Frames already repaired (reported twice) are skipped.
fn repair(app: &App, link: &ClientLink, seqs: &[u32]) {
    if seqs.is_empty() {
        return;
    }
    let geometry = link.watching.read().unwrap().shared.geometry();
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
    link.watching.read().unwrap().shared.resync(link.id, &tiles, &layers);
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

fn send_frame(app: &App, link: &ClientLink, out: &mpsc::UnboundedSender<Vec<u8>>, mut msg: Vec<u8>, path: u8) -> bool {
    if path != PATH_WS {
        // The WebSocket writer stamps its own frames; these are stamped here.
        msg[24..32].copy_from_slice(&proto::now_us().to_le_bytes());
    }
    match (path, &app.rtc, link.wt.lock().unwrap().as_ref()) {
        (PATH_RTC, Some(hub), _) => {
            for chunk in proto::chunks(&msg) {
                hub.send(link.id, chunk);
            }
        }
        (PATH_WT, _, Some(wt)) => {
            let mut datagrams = Vec::new();
            let mut fec = link.fec.lock().unwrap();
            let kept = Kept::send(msg, fec.per(), &mut datagrams);
            let changed = fec.on_sent(datagrams.len(), proto::now_us()).map(|loss| (fec.per(), loss));
            drop(fec);
            if let Some((per, loss)) = changed {
                let text = format!("parity one per {per} pieces ({:.2} % asked for again)", loss * 100.0);
                app.telemetry.event(link.id, &text);
            }
            for d in datagrams {
                // A closed queue: the session is ending, its closing repairs this.
                let _ = wt.queue.send(d);
            }
            let mut frames = link.wt_frames.lock().unwrap();
            if frames.len() == WT_KEEP {
                frames.pop_front();
            }
            frames.push_back(kept);
        }
        // The UDP path went away meanwhile: what was sent there is repaired
        // when its closing is handled; this one goes the sure way.
        _ => return out.send(msg).is_ok(),
    }
    true
}

fn transport_name(path: u8) -> &'static str {
    match path {
        PATH_RTC => "webrtc",
        PATH_WT => "webtransport",
        _ => "websocket",
    }
}

/// Client thread: pulls frames whenever the link window allows, parks otherwise.
fn produce(app: &App, link: &ClientLink, out: &mpsc::UnboundedSender<Vec<u8>>) {
    app.platform.thread_priority(ThreadRole::Stream);
    let mut codec = ClientCodec::new(app.settings.read().unwrap().tuning);
    // Encoder settings the current encoder was made with: backend preference and codec.
    let mut encoder_conf = (String::new(), String::new());
    let mut size = (0, 0);
    let mut need_key = true;
    let mut video_cap = VideoCap::default();
    // Refresh rate of the display streamed (0: not known).
    let mut display_hz = 0;
    let mut path = PATH_WS;
    let mut motion = false;
    // Size the encoder encodes (the display's unless scaled for the
    // client), and since when a larger one has been affordable.
    let mut encoded = (0, 0);
    // A replacement encoder opening (two-pass switched), and when a failed
    // one may be tried again.
    let mut pending: Option<PendingEncoder> = None;
    // The encoder was replaced once for the client's C_BT709 (if the new
    // one still cannot convert, it is not replaced again and again).
    let mut bt709_done = false;
    let mut two_pass_retry_us = 0;
    // Estimate on the WebSocket path when frames moved to the data channel
    // (0: not there, or already found slower).
    let mut ws_rate = 0.0;
    while !link.stop.load(Ordering::Acquire) {
        let stream = link.watching.read().unwrap().clone();
        if stream.gone() {
            leave_gone_display(app, link, out);
            continue;
        }
        let shared = &*stream.shared;
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
        // Frames left on an abandoned UDP path whose ACK never came (the
        // path died with them, the last ones revealed by no later frame).
        // On the path in use the client reports what it gave up.
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
        let view = *link.view.lock().unwrap();
        let fps = video_fps(view, display_hz);
        {
            let s = app.settings.read().unwrap();
            codec.tuning = s.tuning;
            let conf = (s.encoder.preferred.clone(), s.encoder.codec.clone());
            if conf != encoder_conf && size != (0, 0) {
                // Changed at runtime: rebuild the encoder.
                encoder_conf = conf;
                drop(s);
                video_cap.reset(video_rate(model, &codec.tuning), fps);
                let two_pass = next_two_pass(&codec, video_cap.bps, rtt_us);
                create_encoder(app, shared, link, &mut codec, size, encoded, &encoder_conf, &video_cap, two_pass);
                pending = None;
                shared.drop_video(link.id);
                need_key = true;
            }
        }
        // The client's browser shows BT.601 video in the wrong colours
        // (C_BT709): a new encoder that converts to BT.709 itself.
        let bt601 = codec.encoder.as_ref().is_some_and(|e| e.bt601());
        if bt601 && !bt709_done && link.bt709.load(Ordering::Acquire) && size != (0, 0) {
            bt709_done = true;
            app.telemetry.event(link.id, "this browser shows BT.601 video in the wrong colours: the server converts to BT.709");
            pending = None;
            let two_pass = next_two_pass(&codec, video_cap.bps, rtt_us);
            create_encoder(app, shared, link, &mut codec, size, encoded, &encoder_conf, &video_cap, two_pass);
            shared.encoder_reset(link.id);
            need_key = true;
        }
        if let Some(p) = shared.take_cursor_pos(link.id) {
            if out.send(p).is_err() {
                return;
            }
        }
        if let Some(c) = shared.take_cursor(link.id) {
            if out.send(c.to_vec()).is_err() {
                return;
            }
        }
        let before = (video_cap.bps, video_cap.fps);
        if codec.encoder.is_some() && video_cap.update(video_rate(model, &codec.tuning), fps, proto::now_us()) {
            let (from, to) = (before.0 as f64 / 1e6, video_cap.bps as f64 / 1e6);
            let text = format!("video cap {from:.1} Mbit/s for {} frames a second -> {to:.1} for {fps}", before.1);
            app.telemetry.event(link.id, &text);
            if let Some(e) = codec.encoder.as_mut() {
                // A keyframe with NVENC (FFmpeg resets the encoder).
                e.set_max_bitrate(video_cap.bps, fps);
            }
        }
        // Two-pass needs another encoder: opened beside the current one,
        // which keeps encoding meanwhile.
        let two_pass = codec.encoder.as_ref().and_then(|e| e.two_pass().map(|on| (on, e.codec())));
        if let Some((on, current)) = two_pass.filter(|_| pending.is_none() && proto::now_us() >= two_pass_retry_us) {
            let want = wants_two_pass(on, video_cap.bps, rtt_us);
            if want != on {
                let state = if want { "on" } else { "off" };
                let text = format!("two-pass {state} (video cap {:.1} Mbit/s, round trip {} ms)", video_cap.bps as f64 / 1e6, rtt_us / 1000);
                app.telemetry.event(link.id, &text);
                let req = encoder_request(shared, link, current, size, encoded, &encoder_conf.0, &video_cap, want);
                pending = Some(PendingEncoder::open(app, req));
            }
        }
        if let Some((ready, cap)) = pending.as_ref().and_then(|p| p.ready().map(|r| (r, p.cap))) {
            pending = None;
            match ready {
                Ok(mut e) => {
                    if cap != (video_cap.bps, video_cap.fps) {
                        e.set_max_bitrate(video_cap.bps, video_cap.fps);
                    }
                    install_encoder(app, shared, link.id, &mut codec, e);
                    // The new encoder's input is blank: everything is copied in again.
                    shared.encoder_reset(link.id);
                    need_key = true;
                }
                Err(err) => {
                    two_pass_retry_us = proto::now_us() + TWO_PASS_RETRY_US;
                    app.telemetry.event(link.id, &format!("encoder for two-pass failed, keeping the current one: {err:#}"));
                }
            }
        }
        // Frames the client cannot show are not built.
        codec.min_frame_us = if view.hz > 0 { 1_000_000 / view.hz as u64 } else { 0 };
        if codec.encoder.is_some() && size != (0, 0) {
            let want = video_size(size, view);
            if want != encoded {
                let text = format!("video {}x{} -> {}x{}", encoded.0, encoded.1, want.0, want.1);
                app.telemetry.event(link.id, &text);
                encoded = want;
                pending = None;
                let two_pass = next_two_pass(&codec, video_cap.bps, rtt_us);
                create_encoder(app, shared, link, &mut codec, size, encoded, &encoder_conf, &video_cap, two_pass);
                // The new encoder's input is blank: everything is copied in again.
                shared.encoder_reset(link.id);
                need_key = true;
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
                    if (msg[1] & proto::FLAG_MOTION != 0) != motion {
                        motion = !motion;
                        let text = if motion {
                            "motion mode on: the screen moves as a whole, what changes goes as video"
                        } else {
                            "motion mode off: back to tiles"
                        };
                        app.telemetry.event(link.id, text);
                    }
                    let seq = u32::from_le_bytes(msg[4..8].try_into().unwrap());
                    if key {
                        // Requests made before this one are answered by it.
                        *link.last_key.lock().unwrap() = Some(seq);
                        link.key_request.store(false, Ordering::Release);
                        app.telemetry.event(link.id, &format!("keyframe sent ({} KB)", msg.len() / 1024));
                    }
                    let now = proto::now_us();
                    link.pacer.lock().unwrap().on_send(seq, msg.len() as u64, now);
                    link.sent.lock().unwrap().push(seq, now, path != PATH_WS, contents);
                    app.telemetry.client_sent(link.id, msg.len());
                    app.telemetry.client_link(link.id, model, rtt_us, codec.encoder.as_ref().map(|_| video_cap.bps));
                    if !send_frame(app, link, out, msg, path) {
                        return;
                    }
                    continue;
                }
                Ok(Pull::Reset { geometry: g, first_seq, hdr_max }) => {
                    size = (g.width, g.height);
                    encoded = size;
                    let s = app.settings.read().unwrap();
                    encoder_conf = (s.encoder.preferred.clone(), s.encoder.codec.clone());
                    drop(s);
                    display_hz = stream.control.current().map_or(0, |d| d.refresh_hz.round() as u32);
                    video_cap.reset(video_rate(model, &codec.tuning), video_fps(view, display_hz));
                    pending = None;
                    let two_pass = next_two_pass(&codec, video_cap.bps, rtt_us);
                    create_encoder(app, shared, link, &mut codec, size, encoded, &encoder_conf, &video_cap, two_pass);
                    need_key = true;
                    // Frames of the previous display will never be acked meaningfully.
                    link.pacer.lock().unwrap().forget_inflight();
                    link.sent.lock().unwrap().clear();
                    link.hello_acked.store(false, Ordering::Release);
                    if out.send(proto::hello(g.width, g.height, g.tile, first_seq, hdr_max)).is_err() {
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
        // Wake when the oldest frame of an abandoned UDP path would expire.
        if let Some(oldest) = link.sent.lock().unwrap().oldest_udp_us().filter(|_| abandoned) {
            let t = oldest + loss_timeout(link);
            wake_at_us = Some(wake_at_us.map_or(t, |w: u64| w.min(t)));
        }
        match wake_at_us {
            Some(t) => std::thread::park_timeout(Duration::from_micros(t.saturating_sub(proto::now_us()))),
            None => std::thread::park(),
        }
    }
}

/// Encoded video size: the size the client shows the display at (its view,
/// with the display's proportions), never larger than the display. More
/// pixels than the client shows cost encoding, link and above all the
/// client's decoder for nothing it can show: a phone showing a 3840x2160
/// display at 1927x1084 took 11 ms to decode each AV1 frame of it. Even
/// (4:2:0 video).
fn video_size(display: (u32, u32), view: View) -> (u32, u32) {
    if view.width == 0 || view.height == 0 {
        return display;
    }
    let (dw, dh) = (display.0 as f64, display.1 as f64);
    let scale = (view.width as f64 / dw).max(view.height as f64 / dh);
    if scale >= 1.0 {
        return display;
    }
    let even = |v: f64| ((v * scale).round() as u32 & !1).max(2);
    (even(dw), even(dh))
}

/// Video frames a second there can be: what the client shows (60 when it
/// has not said), within what the display shows.
fn video_fps(view: View, display_hz: u32) -> u32 {
    let hz = if view.hz == 0 { 60 } else { view.hz };
    if display_hz == 0 {
        hz
    } else {
        hz.min(display_hz)
    }
}

/// Video bitrate cap following the link estimate, with hysteresis. It is
/// for `fps` frames a second: a frame takes at most its share, so that
/// many fit the cap (see `VideoEncoder::set_max_bitrate`).
#[derive(Default)]
struct VideoCap {
    bps: u64,
    fps: u32,
    changed_us: u64,
}

impl VideoCap {
    fn target(rate: f64) -> u64 {
        (rate * 8.0 * VIDEO_SHARE) as u64
    }

    fn reset(&mut self, rate: f64, fps: u32) {
        self.bps = Self::target(rate);
        self.fps = fps;
        self.changed_us = proto::now_us();
    }

    /// Moves the cap when the target left the dead band (or collapsed).
    /// True when the encoder has to be told: the cap moved, or the frame
    /// rate it is for changed (the client's window went to another screen).
    fn update(&mut self, rate: f64, fps: u32, now: u64) -> bool {
        let target = Self::target(rate);
        let ratio = target as f64 / self.bps.max(1) as f64;
        let shifted = ratio < CAP_DEADBAND.0 || ratio > CAP_DEADBAND.1;
        let hold = if ratio >= CAP_JUMP { CAP_JUMP_HOLD_US } else { CAP_HOLD_US };
        let moved = ratio < CAP_COLLAPSE || (shifted && now.saturating_sub(self.changed_us) >= hold);
        if moved {
            self.bps = target;
            self.changed_us = now;
        }
        std::mem::replace(&mut self.fps, fps) != fps || moved
    }
}

/// Bytes/s the video cap follows: the content budget, with CAP_RAMP headroom
/// during slow start (within the configured bandwidth).
fn video_rate(link: Link, tuning: &Tuning) -> f64 {
    let budget = link.budget(tuning);
    if link.proven {
        budget
    } else {
        (budget * CAP_RAMP).min(tuning.bandwidth_mbps * 1e6 / 8.0)
    }
}

/// Codecs `auto` picks from, best first, among those the client decodes
/// and an encoder here encodes.
const AUTO_CODECS: [Codec; 3] = [Codec::Av1, Codec::Hevc, Codec::H264];

/// The codec for a client: the configured one when the client decodes it
/// and an encoder here encodes it, else H.264, which every client decodes;
/// `auto` (or anything else) the first usable of AUTO_CODECS.
fn pick_codec(setting: &str, decodable: &[Codec], encodable: &[Codec]) -> Codec {
    let usable = |c: &Codec| decodable.contains(c) && encodable.contains(c);
    match Codec::from_name(setting) {
        Some(c) if usable(&c) => c,
        Some(_) => Codec::H264,
        None => AUTO_CODECS.into_iter().find(usable).unwrap_or(Codec::H264),
    }
}

/// Two-pass encoding (where the encoder offers it: H.264 on NVENC) pays on
/// slow or distant links and costs frame rate on fast near ones (measured,
/// docs/RESULTS.md "Two-pass on slow links"): on when the video cap is
/// below TWO_PASS_CAP.0 or the round trip at least TWO_PASS_RTT_US.0, off
/// again only past the other end of each band, so a link near a threshold
/// does not flip between encoders. A slow link is not always seen by the
/// cap: video using less than the link leaves the estimate above it, as
/// on an 18 Mbit/s path with 80 ms of round trip.
const TWO_PASS_CAP: (u64, u64) = (20_000_000, 30_000_000);
const TWO_PASS_RTT_US: (u64, u64) = (50_000, 35_000);
/// A failed attempt to open the other encoder is not repeated sooner.
const TWO_PASS_RETRY_US: u64 = 30_000_000;

fn wants_two_pass(on: bool, cap_bps: u64, rtt_us: u64) -> bool {
    match on {
        false => cap_bps < TWO_PASS_CAP.0 || rtt_us >= TWO_PASS_RTT_US.0,
        true => cap_bps < TWO_PASS_CAP.1 || rtt_us >= TWO_PASS_RTT_US.1,
    }
}

/// What `wants_two_pass` says for the next encoder of a client, from the
/// current one's setting.
fn next_two_pass(codec: &ClientCodec, cap_bps: u64, rtt_us: u64) -> bool {
    wants_two_pass(codec.encoder.as_ref().and_then(|e| e.two_pass()).unwrap_or(false), cap_bps, rtt_us)
}

/// A new encoder for `size` (the display's), encoding `output` (the size
/// the client shows it at, see `video_size`), with the backend
/// preference and codec of `conf`. H.264 when the chosen codec cannot be
/// opened.
#[allow(clippy::too_many_arguments)]
fn create_encoder(
    app: &App,
    shared: &Shared,
    link: &ClientLink,
    codec: &mut ClientCodec,
    size: (u32, u32),
    output: (u32, u32),
    conf: &(String, String),
    cap: &VideoCap,
    two_pass: bool,
) {
    let id = link.id;
    codec.encoder = None;
    shared.set_gpu_input(id, false);
    let encoders = app.platform.encoders();
    let (pref, setting) = conf;
    let chosen = pick_codec(setting, &link.codecs, &encoders.codecs());
    let mut req = encoder_request(shared, link, chosen, size, output, pref, cap, two_pass);
    let mut created = encoders.create(&req);
    if let Err(e) = &created {
        if req.codec != Codec::H264 {
            app.telemetry.event(id, &format!("{} encoder failed, using h264: {e:#}", req.codec.name()));
            req.codec = Codec::H264;
            created = encoders.create(&req);
        }
    }
    match created {
        Ok(e) => install_encoder(app, shared, id, codec, e),
        Err(e) => {
            app.telemetry.client_encoder(id, None, Some(format!("{e:#}")));
            app.telemetry.error("video encoder unavailable, lossless only", e);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn encoder_request(
    shared: &Shared,
    link: &ClientLink,
    codec: Codec,
    size: (u32, u32),
    output: (u32, u32),
    pref: &str,
    cap: &VideoCap,
    two_pass: bool,
) -> EncoderRequest {
    EncoderRequest {
        codec,
        width: size.0,
        height: size.1,
        output,
        preferred: (pref != "auto" && !pref.is_empty()).then(|| pref.to_string()),
        surface: shared.surface(),
        max_bitrate: cap.bps,
        fps: cap.fps,
        two_pass,
        bt709: link.bt709.load(Ordering::Acquire),
    }
}

fn install_encoder(app: &App, shared: &Shared, id: u64, codec: &mut ClientCodec, e: Box<dyn VideoEncoder>) {
    app.telemetry.client_encoder(id, Some(e.name()), e.fallback_reason());
    shared.set_gpu_input(id, e.gpu_input());
    codec.encoder = Some(e);
}

/// An encoder opening on its own thread: NVENC takes about 250 ms to open,
/// which the producer would otherwise spend not sending anything. The
/// current encoder keeps encoding until this one is ready.
struct PendingEncoder {
    rx: std::sync::mpsc::Receiver<Result<Box<dyn VideoEncoder>>>,
    /// The cap it was opened with, and the frame rate that was for.
    cap: (u64, u32),
}

impl PendingEncoder {
    /// Opens an encoder for `req`; the calling (producer) thread is woken
    /// when it is ready.
    fn open(app: &App, req: EncoderRequest) -> Self {
        let cap = (req.max_bitrate, req.fps);
        let (tx, rx) = std::sync::mpsc::channel();
        let (platform, producer) = (app.platform.clone(), std::thread::current());
        std::thread::Builder::new()
            .name("encoder-open".into())
            .spawn(move || {
                let _ = tx.send(platform.encoders().create(&req));
                producer.unpark();
            })
            .expect("spawn encoder thread");
        Self { rx, cap }
    }

    /// The opened encoder (or why not) once ready.
    fn ready(&self) -> Option<Result<Box<dyn VideoEncoder>>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("encoder thread ended"))),
        }
    }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

/// `stream`: the display the input is for.
fn handle_input(b: &[u8], stream: &DisplayStream, held: &mut Held) -> Result<()> {
    let Some(input) = stream.control.input() else { return Ok(()) };
    match (b.first().copied(), b.len()) {
        (Some(proto::C_MOVE), 6..) => input.motion(u16_at(b, 2) as u32, u16_at(b, 4) as u32)?,
        (Some(proto::C_MOVE_LATEST), 10..) => {
            let seq = u32::from_le_bytes(b[6..10].try_into().unwrap());
            // Each move comes twice (unordered and ordered): the later copy is dropped.
            if held.move_seq.is_none_or(|last| seq.wrapping_sub(last) as i32 > 0) {
                held.move_seq = Some(seq);
                input.motion(u16_at(b, 2) as u32, u16_at(b, 4) as u32)?
            }
        }
        (Some(proto::C_MOVE_REL), 6..) => {
            let (dx, dy) = (u16_at(b, 2) as i16 as i32, u16_at(b, 4) as i16 as i32);
            if b[1] & proto::MOVE_AS_POSITION == 0 {
                input.motion_relative(dx, dy)?
            } else if let (Some((x, y)), Some(d)) = (input.pointer()?, stream.control.current()) {
                let x = (x as i32 + dx).clamp(0, d.width as i32 - 1) as u32;
                let y = (y as i32 + dy).clamp(0, d.height as i32 - 1) as u32;
                input.motion(x, y)?
            }
        }
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
            let down = b[2] != 0;
            held.buttons.retain(|&h| h != button);
            if down {
                held.buttons.push(button);
            }
            input.button(button, down)?;
        }
        (Some(proto::C_WHEEL), 6..) => input.wheel(u16_at(b, 2) as i16 as i32, u16_at(b, 4) as i16 as i32)?,
        (Some(proto::C_KEY), 4..) => {
            let (key, down) = (u16_at(b, 2), b[1] != 0);
            held.keys.retain(|&h| h != key);
            if down {
                held.keys.push(key);
            }
            input.key(key, down)?;
        }
        (Some(proto::C_TEXT), _) => input.text(&String::from_utf8_lossy(&b[1..]))?,
        _ => {}
    }
    Ok(())
}

/// The display list for the client's menu (`proto::S_DISPLAYS`), with the
/// display it streams; `error` the failure of its last request.
fn send_displays(app: &App, out: &mpsc::UnboundedSender<Vec<u8>>, link: &ClientLink, error: Option<String>) {
    let current = link.watching.read().unwrap().id.clone();
    match app.displays_json(Some(&current)) {
        Ok(mut v) => {
            if let Some(e) = error {
                v["error"] = json!(e);
            }
            let _ = out.send(proto::text(proto::S_DISPLAYS, &v.to_string()));
        }
        Err(e) => app.telemetry.error("list displays", e),
    }
}

/// The client's display went away: it moves to the default one.
fn leave_gone_display(app: &App, link: &ClientLink, out: &mpsc::UnboundedSender<Vec<u8>>) {
    match app.resolve_display(None) {
        Ok(id) => {
            switch_display(app, link, &std::thread::current(), app.streams.watch(&id));
            send_displays(app, out, link, None);
        }
        Err(e) => {
            app.telemetry.error("list displays", e);
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: (u32, u32) = (3840, 2160);

    fn view(width: u32, height: u32, hz: u32) -> View {
        View { width, height, hz }
    }

    #[test]
    fn video_is_encoded_at_the_size_the_client_shows_it() {
        assert_eq!(video_size(DISPLAY, view(1920, 1080, 60)), (1920, 1080));
        // A phone's window, not quite half the display.
        assert_eq!(video_size(DISPLAY, view(1927, 1084, 120)), (1926, 1084));
        // Never larger than the display, and the display's without a report.
        assert_eq!(video_size(DISPLAY, view(5120, 2880, 60)), DISPLAY);
        assert_eq!(video_size(DISPLAY, View::default()), DISPLAY);
        // A window wider in proportion than the display decides by its width.
        assert_eq!(video_size(DISPLAY, view(3000, 1000, 60)), (3000, 1688));
    }

    #[test]
    fn codec_is_the_configured_one_only_when_both_sides_have_it() {
        use Codec::*;
        let client = [H264, Hevc];
        let server = [H264, Hevc, Av1];
        // auto: the best both have, H.264 when the client offers nothing else.
        assert_eq!(pick_codec("auto", &client, &server), Hevc);
        assert_eq!(pick_codec("auto", &[H264, Av1], &[H264, Hevc]), H264);
        // Configured: kept when both have it, H.264 otherwise (not another one).
        assert_eq!(pick_codec("h264", &client, &server), H264);
        assert_eq!(pick_codec("av1", &client, &server), H264);
        assert_eq!(pick_codec("hevc", &[H264, Hevc, Av1], &[H264, Av1]), H264);
    }

    #[test]
    fn two_pass_follows_slow_or_distant_links_with_hysteresis() {
        const MBIT: u64 = 1_000_000;
        // Fast and near: off; slow or distant: on.
        assert!(!wants_two_pass(false, 100 * MBIT, 1_000));
        assert!(wants_two_pass(false, 10 * MBIT, 1_000));
        assert!(wants_two_pass(false, 100 * MBIT, 80_000));
        // Between the ends of a band the current setting stays.
        assert!(!wants_two_pass(false, 25 * MBIT, 40_000));
        assert!(wants_two_pass(true, 25 * MBIT, 1_000));
        assert!(wants_two_pass(true, 100 * MBIT, 40_000));
        // Past both far ends it goes off.
        assert!(!wants_two_pass(true, 31 * MBIT, 30_000));
    }
}
