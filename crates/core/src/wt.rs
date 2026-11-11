//! WebTransport (HTTP/3 over QUIC) beside the WebSocket. Frames go to the
//! client as datagrams: a lost packet costs its own frame a round trip (the
//! client asks for the missing pieces, see `proto::S_PIECE`) instead of
//! holding every later frame back, as TCP and the reliable data channel do.
//!
//! Browsers only open WebTransport to a server whose certificate they can
//! check, and offer no warning page to click through. Unless a CA-signed
//! certificate is configured (`tls.cert` / `tls.key`), the hub makes its own:
//! valid for less than two weeks (the limit browsers set for a certificate
//! pinned by hash) and pinned by its SHA-256, which the client gets over the
//! WebSocket. It is trusted exactly as much as the page that carries the
//! hash, whatever made the page trusted (a proxy's certificate in front, or
//! our self-signed one accepted once). It is renewed well before it expires
//! and the new hash goes to every client.
//!
//! QUIC's own congestion control is switched off: frames are already paced
//! by each client's link model (`pacer.rs`), and a second controller would
//! hold datagrams back (and drop the oldest) on loss it reads as congestion.
//!
//! A client's WebTransport session is tied to its WebSocket session by a
//! random key in the URL, handed out over that WebSocket (which the stream
//! token already authorized).

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use wtransport::config::QuicTransportConfig;
use wtransport::endpoint::endpoint_side::Server;
use wtransport::quinn::congestion::{Controller, ControllerFactory};
use wtransport::tls::{Certificate, CertificateChain, PrivateKey};
use wtransport::{Connection, Endpoint, Identity, ServerConfig};

use crate::config::{TlsSettings, WebTransportSettings};

/// Total validity of the generated certificate: browsers refuse a pinned one
/// valid for two weeks or more. A day of it lies in the past, for clients
/// whose clock is behind.
const CERT_DAYS: i64 = 13;
/// A new certificate this often, so a client never holds a hash about to expire.
const RENEW: Duration = Duration::from_secs(5 * 24 * 3600);
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(2);
/// Datagrams waiting to leave; the link model keeps far less in flight.
const DATAGRAM_BUFFER: usize = 16 << 20;
/// A window this large also switches quinn's pacing off (it skips windows
/// above `u32::MAX`).
const UNLIMITED: u64 = 1 << 40;

/// What a client needs to reach the hub (S_SIGNAL `wt`).
#[derive(Clone, Serialize)]
pub struct Offer {
    /// SHA-256 of the certificate, hex; None with a CA-signed one.
    pub hash: Option<String>,
    pub port: u16,
    /// Hosts ("" = the page's own) or full URLs to try, see `proto::S_SIGNAL`.
    pub candidates: Vec<String>,
}

pub struct WtHub {
    sessions: Mutex<HashMap<String, mpsc::UnboundedSender<Connection>>>,
    offer: watch::Sender<Offer>,
}

impl WtHub {
    /// Binds the UDP port and starts accepting. Runs on the tokio runtime.
    pub async fn start(settings: &WebTransportSettings, tls: &TlsSettings, listen_port: u16) -> Result<Arc<Self>> {
        let port = match settings.port {
            0 => listen_port.checked_add(1).context("no port above server.listen for webtransport")?,
            p => p,
        };
        let ca_signed = !tls.cert.is_empty() && !tls.key.is_empty();
        let (identity, hash) = if ca_signed {
            let identity = Identity::load_pemfiles(&tls.cert, &tls.key).await.context("load tls.cert / tls.key")?;
            (identity, None)
        } else {
            let (identity, hash) = self_signed()?;
            (identity, Some(hash))
        };
        let endpoint =
            Endpoint::server(server_config(port, identity)?).with_context(|| format!("bind udp port {port}"))?;
        // The page's own host first; then forwards and proxies; then this
        // machine's addresses (LAN, tailnet), for a page that came through a
        // relay carrying no UDP.
        let mut candidates = vec![String::new()];
        candidates.extend(settings.advertise.iter().cloned());
        candidates.extend(crate::rtc::local_ipv4()?.into_iter().filter(|ip| !ip.is_loopback()).map(|ip| ip.to_string()));
        let pinned = if ca_signed { "configured certificate" } else { "own certificate pinned by hash" };
        eprintln!("webtransport: udp port {port}, {pinned}");
        let hub = Arc::new(Self {
            sessions: Mutex::default(),
            offer: watch::channel(Offer { hash, port, candidates }).0,
        });
        tokio::spawn(hub.clone().run(endpoint, port, !ca_signed));
        Ok(hub)
    }

    /// Where a client may connect, followed across certificate renewals.
    pub fn offer(&self) -> watch::Receiver<Offer> {
        self.offer.subscribe()
    }

    /// Connections opened with `key` go to `sessions` until `unregister`.
    pub fn register(&self, key: &str, sessions: mpsc::UnboundedSender<Connection>) {
        self.sessions.lock().unwrap().insert(key.to_string(), sessions);
    }

    pub fn unregister(&self, key: &str) {
        self.sessions.lock().unwrap().remove(key);
    }

    async fn run(self: Arc<Self>, endpoint: Endpoint<Server>, port: u16, renew: bool) {
        let mut renewal = tokio::time::interval_at(tokio::time::Instant::now() + RENEW, RENEW);
        loop {
            tokio::select! {
                incoming = endpoint.accept() => {
                    let hub = self.clone();
                    tokio::spawn(async move {
                        // A failed handshake (a stale hash, a scan) has nobody to tell.
                        let Ok(request) = incoming.await else { return };
                        let key = request
                            .path()
                            .split_once('?')
                            .and_then(|(_, query)| query.split('&').find_map(|kv| kv.strip_prefix("k=")))
                            .unwrap_or_default()
                            .to_string();
                        let session = hub.sessions.lock().unwrap().get(&key).cloned();
                        match session {
                            Some(session) => {
                                if let Ok(connection) = request.accept().await {
                                    let _ = session.send(connection);
                                }
                            }
                            None => request.not_found().await,
                        }
                    });
                }
                _ = renewal.tick(), if renew => {
                    // Connections keep the certificate they were opened with.
                    let renewed = self_signed()
                        .and_then(|(identity, hash)| Ok((server_config(port, identity)?, hash)))
                        .and_then(|(config, hash)| endpoint.reload_config(config, false).map(|()| hash).map_err(Into::into));
                    match renewed {
                        Ok(hash) => self.offer.send_modify(|o| o.hash = Some(hash)),
                        Err(e) => eprintln!("webtransport: certificate renewal failed: {e:#}"),
                    }
                }
            }
        }
    }
}

fn server_config(port: u16, identity: Identity) -> Result<ServerConfig> {
    let mut transport = QuicTransportConfig::default();
    transport
        .max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?))
        .keep_alive_interval(Some(KEEP_ALIVE))
        .datagram_send_buffer_size(DATAGRAM_BUFFER)
        .congestion_controller_factory(Arc::new(Unlimited));
    Ok(ServerConfig::builder().with_bind_default(port).with_custom_transport(identity, transport).build())
}

/// A certificate for pinning by hash (ECDSA P-256, as browsers require for
/// those) and the hash, hex.
fn self_signed() -> Result<(Identity, String)> {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "remothing webtransport");
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(CERT_DAYS - 1);
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    let der = cert.der().to_vec();
    let hash = Sha256::digest(&der).iter().map(|b| format!("{b:02x}")).collect();
    let chain = CertificateChain::single(Certificate::from_der(der).context("generated certificate")?);
    Ok((Identity::new(chain, PrivateKey::from_der_pkcs8(key.serialize_der())), hash))
}

/// No congestion control (see the module comment).
#[derive(Clone)]
struct Unlimited;

impl Controller for Unlimited {
    fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: u64) {}

    fn on_mtu_update(&mut self, _: u16) {}

    fn window(&self) -> u64 {
        UNLIMITED
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        UNLIMITED
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl ControllerFactory for Unlimited {
    fn build(self: Arc<Self>, _: Instant, _: u16) -> Box<dyn Controller> {
        Box::new(Unlimited)
    }
}
