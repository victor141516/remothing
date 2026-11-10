//! Process wiring: capture thread, streaming listener and admin listener.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use axum::http::{HeaderName, HeaderValue};
use axum::routing::get;
use axum::Router;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::assets;
use crate::capture::{CaptureLoop, DisplayControl};
use crate::config::{ConfigFile, Settings};
use crate::platform::Platform;
use crate::shared::Shared;
use crate::stream;
use crate::telemetry::Telemetry;

pub struct App {
    pub platform: Arc<dyn Platform>,
    pub shared: Arc<Shared>,
    pub control: Arc<DisplayControl>,
    pub telemetry: Arc<Telemetry>,
    pub settings: RwLock<Settings>,
    pub config: ConfigFile,
    pub keyboard_layout: Arc<RwLock<String>>,
    pub admin_sessions: Mutex<HashSet<String>>,
    pub tls_enabled: bool,
}

impl App {
    /// Applies a change to the settings and persists the config file.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut s = self.settings.write().unwrap();
        f(&mut s);
        self.config.save(&s)
    }
}

/// Runs the server until the process exits.
pub fn run(platform: Arc<dyn Platform>, config: ConfigFile, settings: Settings) -> Result<()> {
    let tls = (!settings.tls.cert.is_empty() && !settings.tls.key.is_empty())
        .then(|| (settings.tls.cert.clone(), settings.tls.key.clone()));
    let telemetry = Arc::new(Telemetry::default());
    let app = Arc::new(App {
        platform: platform.clone(),
        shared: Arc::new(Shared::new(settings.server.tile)),
        control: Arc::new(DisplayControl::default()),
        telemetry: telemetry.clone(),
        keyboard_layout: Arc::new(RwLock::new(settings.input.keyboard_layout.clone())),
        settings: RwLock::new(settings.clone()),
        config,
        admin_sessions: Mutex::new(HashSet::new()),
        tls_enabled: tls.is_some(),
    });

    let capture = CaptureLoop {
        platform,
        shared: app.shared.clone(),
        control: app.control.clone(),
        telemetry,
        keyboard_layout: app.keyboard_layout.clone(),
    };
    let initial = settings.server.display.clone();
    std::thread::Builder::new().name("capture".into()).spawn(move || capture.run(initial))?;

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

    let stream_addr: SocketAddr = settings.server.listen.parse().context("server.listen")?;
    let admin_addr: SocketAddr = settings.server.admin_listen.parse().context("server.admin_listen")?;
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?.block_on(async move {
        let rustls = match &tls {
            Some((cert, key)) => {
                let _ = rustls::crypto::ring::default_provider().install_default();
                Some(
                    axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                        .await
                        .context("load TLS certificate")?,
                )
            }
            None => None,
        };
        let scheme = if rustls.is_some() { "https" } else { "http" };
        eprintln!("stream client on {scheme}://{stream_addr}, admin UI on {scheme}://{admin_addr}");
        tokio::try_join!(
            serve(stream_addr, stream_router, rustls.clone()),
            serve(admin_addr, admin_router, rustls)
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
