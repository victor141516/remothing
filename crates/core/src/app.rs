//! Process wiring: display captures, streaming listener and admin listener.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{bail, Context, Result};
use axum::http::{HeaderName, HeaderValue};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::assets;
use crate::audio::AudioHub;
use crate::capture::{DisplayStream, Streams};
use crate::config::{ConfigFile, Settings};
use crate::mic::MicHub;
use crate::platform::{DisplayMode, Platform, ThreadRole};
use crate::stream;
use crate::telemetry::Telemetry;

pub struct App {
    pub platform: Arc<dyn Platform>,
    /// The displays being captured, one per display some client watches.
    pub streams: Arc<Streams>,
    /// Client ids, unique across displays.
    pub next_client: AtomicU64,
    /// The display each page's input socket acts on, by the page's session
    /// key (`/ws` and `/input` give the same one).
    pub input_routes: Mutex<HashMap<String, Watching>>,
    pub telemetry: Arc<Telemetry>,
    pub audio: Arc<AudioHub>,
    pub mic: Arc<MicHub>,
    pub settings: RwLock<Settings>,
    pub config: ConfigFile,
    pub keyboard_layout: Arc<RwLock<String>>,
    pub admin_sessions: Mutex<HashSet<String>>,
    pub tls_enabled: bool,
    /// WebRTC transport, when enabled and a UDP socket could be bound.
    pub rtc: Option<crate::rtc::Hub>,
    /// WebTransport, when enabled and its UDP port could be bound.
    pub wt: Option<Arc<crate::wt::WtHub>>,
    /// Clients connected, and what the OS was asked for while they stream.
    pub streaming: Mutex<Streaming>,
}

/// The display a client streams, shared by its stream and input sockets;
/// replaced when the client switches.
pub type Watching = Arc<RwLock<Arc<DisplayStream>>>;

#[derive(Default)]
pub struct Streaming {
    clients: usize,
    /// One line per setting, for the admin status.
    pub notes: Vec<String>,
}

/// A client's or the admin UI's virtual display request (`proto::C_VIRTUAL`).
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum VirtualRequest {
    Install,
    Create { mode: DisplayMode },
    Set { id: String, mode: DisplayMode },
    Destroy { id: String },
}

impl App {
    /// Applies a change to the settings and persists the config file.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut s = self.settings.write().unwrap();
        f(&mut s);
        self.config.save(&s)
    }

    /// Makes display `id` the one new clients start on, also after a restart.
    /// Clients streaming another display stay there.
    pub fn select_display(&self, id: &str) -> Result<()> {
        if !self.platform.displays().list()?.iter().any(|d| d.id == id) {
            bail!("unknown display {id}");
        }
        self.update_settings(|s| s.server.display = id.to_string())
    }

    /// The display a client starts on: `wanted` when it is listed, else the
    /// default (`server.display`), else the primary, else the first.
    pub fn resolve_display(&self, wanted: Option<&str>) -> Result<String> {
        let list = self.platform.displays().list()?;
        let default = self.settings.read().unwrap().server.display.clone();
        let listed = |id: &str| list.iter().any(|d| d.id == id);
        let chosen = wanted.filter(|w| listed(w)).or(Some(default.as_str()).filter(|d| listed(d)));
        chosen
            .map(String::from)
            .or_else(|| list.iter().find(|d| d.primary).or(list.first()).map(|d| d.id.clone()))
            .context("no display available")
    }

    /// The display list as clients and the admin UI get it (see
    /// `proto::S_DISPLAYS`): `current` is the display the asking client
    /// streams; `default` the one new clients start on; `streamed` the
    /// displays being captured, with how many clients watch each.
    pub fn displays_json(&self, current: Option<&str>) -> Result<serde_json::Value> {
        let d = self.platform.displays();
        let list = d.list()?;
        let virt = d.virtual_displays().map(|v| json!({ "setup": v.setup(), "capabilities": v.capabilities() }));
        let streamed: serde_json::Map<String, serde_json::Value> =
            self.streams.active().iter().map(|s| (s.id.clone(), json!(s.watchers()))).collect();
        let default = self.resolve_display(None).ok();
        Ok(json!({ "displays": list, "current": current, "default": default, "streamed": streamed, "virtual": virt }))
    }

    /// Carries out a virtual display request. Returns a new display, which
    /// the asking client streams from then on.
    pub fn virtual_request(&self, req: VirtualRequest) -> Result<Option<String>> {
        let v = self.platform.displays().virtual_displays().context("virtual displays are not supported here")?;
        match req {
            VirtualRequest::Install => v.install().map(|()| None),
            VirtualRequest::Create { mode } => {
                mode.check(&v.capabilities())?;
                Ok(Some(v.create(mode)?.id))
            }
            VirtualRequest::Set { id, mode } => {
                let caps = v.capabilities();
                if !caps.resize {
                    bail!("virtual displays keep their size here");
                }
                mode.check(&caps)?;
                v.set_mode(&id, mode)?;
                // Opened again at its new size, if it is streamed.
                self.streams.reopen(&id);
                Ok(None)
            }
            VirtualRequest::Destroy { id } => {
                v.destroy(&id)?;
                self.streams.gone(&id);
                Ok(None)
            }
        }
    }

    /// The first client in asks the OS to favour the stream (see `Platform::streaming`).
    pub fn client_joined(&self) {
        let mut s = self.streaming.lock().unwrap();
        s.clients += 1;
        if s.clients == 1 {
            s.notes = self.platform.streaming(true);
            eprintln!("scheduling while streaming: {}", s.notes.join("; "));
        }
    }

    /// The last client out lets go of it.
    pub fn client_left(&self) {
        let mut s = self.streaming.lock().unwrap();
        s.clients -= 1;
        if s.clients == 0 {
            s.notes = self.platform.streaming(false);
            eprintln!("scheduling without clients: {}", s.notes.join("; "));
        }
    }
}

/// Runs the server until the process exits.
pub fn run(platform: Arc<dyn Platform>, config: ConfigFile, settings: Settings) -> Result<()> {
    let config_dir = config.path.parent().map(Path::to_path_buf).unwrap_or_default();
    let pems = crate::tls::load(&settings.tls, &config_dir)?;
    // An extra HTTPS listener for the client leaves the others plain.
    let https_addr: Option<SocketAddr> =
        (!settings.tls.listen.is_empty()).then(|| settings.tls.listen.parse()).transpose().context("tls.listen")?;
    let telemetry = Arc::new(Telemetry::default());
    let stream_addr: SocketAddr = settings.server.listen.parse().context("server.listen")?;
    let admin_addr: SocketAddr = settings.server.admin_listen.parse().context("server.admin_listen")?;
    let rtc = match settings.webrtc.enabled {
        // Without it everything still works over the WebSocket.
        true => crate::rtc::Hub::start(&settings.webrtc, stream_addr.port(), platform.clone())
            .inspect_err(|e| telemetry.error("webrtc disabled", e))
            .ok(),
        false => None,
    };
    // One rustls backend for HTTPS and WebTransport.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Network threads carry every frame: above normal like the producers.
    let runtime = {
        let platform = platform.clone();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .on_thread_start(move || platform.thread_priority(ThreadRole::Stream))
            .enable_all()
            .build()?
    };
    let wt = match settings.webtransport.enabled {
        // Without it frames take the data channel or the WebSocket.
        true => runtime
            .block_on(crate::wt::WtHub::start(&settings.webtransport, &settings.tls, stream_addr.port(), telemetry.clone()))
            .inspect_err(|e| telemetry.error("webtransport disabled", e))
            .ok(),
        false => None,
    };
    let keyboard_layout = Arc::new(RwLock::new(settings.input.keyboard_layout.clone()));
    let app = Arc::new(App {
        platform: platform.clone(),
        streams: Streams::new(platform.clone(), telemetry.clone(), keyboard_layout.clone(), settings.server.tile),
        next_client: AtomicU64::new(1),
        input_routes: Mutex::default(),
        telemetry: telemetry.clone(),
        audio: AudioHub::new(platform.clone(), telemetry.clone()),
        mic: MicHub::new(platform, telemetry),
        keyboard_layout,
        settings: RwLock::new(settings.clone()),
        config,
        admin_sessions: Mutex::new(HashSet::new()),
        tls_enabled: pems.is_some() && https_addr.is_none(),
        rtc,
        wt,
        streaming: Mutex::default(),
    });

    // Cross-origin isolation gives the client 5 µs timer resolution instead of 100 µs.
    let stream_router = Router::new()
        .route("/ws", get(stream::stream_handler))
        .route("/input", get(stream::input_handler))
        .fallback(assets::stream_asset)
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-embedder-policy"),
            HeaderValue::from_static("require-corp"),
        ))
        .with_state(app.clone());
    let admin_router = crate::admin::router().fallback(assets::admin_asset).with_state(app.clone());

    runtime.block_on(async move {
        let rustls = match pems {
            Some((cert, key)) => {
                Some(
                    axum_server::tls_rustls::RustlsConfig::from_pem(cert, key)
                        .await
                        .context("load TLS certificate")?,
                )
            }
            None => None,
        };
        let (plain_tls, https) = match https_addr {
            Some(addr) => (None, rustls.map(|cfg| (addr, cfg))),
            None => (rustls, None),
        };
        let scheme = if plain_tls.is_some() { "https" } else { "http" };
        let extra = https.as_ref().map(|(addr, _)| format!(" and https://{addr}")).unwrap_or_default();
        eprintln!("stream client on {scheme}://{stream_addr}{extra}, admin UI on {scheme}://{admin_addr}");
        let https_router = stream_router.clone();
        let https = async move {
            match https {
                Some((addr, cfg)) => serve(addr, https_router, Some(cfg)).await,
                None => Ok(()),
            }
        };
        tokio::try_join!(
            serve(stream_addr, stream_router, plain_tls.clone()),
            https,
            serve(admin_addr, admin_router, plain_tls)
        )?;
        Ok(())
    })
}

async fn serve(
    addr: SocketAddr,
    router: Router,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
) -> Result<()> {
    let listener = std::net::TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
    listener.set_nonblocking(true)?;
    let service = router.into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        Some(cfg) => {
            let acceptor = axum_server::tls_rustls::RustlsAcceptor::new(cfg).acceptor(NoDelay);
            axum_server::from_tcp(listener).acceptor(acceptor).serve(service).await?
        }
        None => axum_server::from_tcp(listener).acceptor(NoDelay).serve(service).await?,
    }
    Ok(())
}

/// Disables Nagle on accepted sockets: small frames and ACKs must leave immediately.
#[derive(Clone)]
struct NoDelay;

impl<S: Send + 'static> axum_server::accept::Accept<tokio::net::TcpStream, S> for NoDelay {
    type Stream = tokio::net::TcpStream;
    type Service = S;
    type Future = std::future::Ready<std::io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        let _ = stream.set_nodelay(true);
        std::future::ready(Ok((stream, service)))
    }
}
