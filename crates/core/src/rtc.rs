//! WebRTC data-channel transport beside the WebSocket.
//!
//! The browser opens the page and the WebSocket however it can reach the
//! server (directly, or through a TCP-only relay such as Tailscale Funnel) and
//! then offers a peer connection over that WebSocket. ICE tries every pair of
//! addresses (LAN, tailnet, STUN-mapped public ones; both sides send checks,
//! so NATs get punched) and, when one works, frames and input move to UDP:
//! a shorter path than any relay, and a lost packet only delays its own frame.
//! When nothing works, everything stays on the WebSocket.
//!
//! One hub thread owns all `Rtc` instances (str0m is sans-IO) and one UDP
//! socket per local IPv4 address; a reader thread per socket feeds packets in.
//! Sessions talk to it through a channel and get events back on their own.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::sync::{mpsc as tmpsc, oneshot};

use crate::config::WebRtcSettings;

/// Frames (server → client, reliable but unordered: no frame waits behind
/// another's retransmission) and ACKs/pings (client → server). The browser
/// creates the channels, so their reliability is set in the client.
const FRAMES_LABEL: &str = "frames";
/// Input (client → server, reliable and ordered like the input WebSocket).
const INPUT_LABEL: &str = "input";
/// SCTP send buffer: a few large frames in flight (the pacer bounds it anyway).
const SCTP_BUFFER: usize = 8 << 20;
/// STUN binding requests: retries while unanswered, then a periodic refresh
/// (NAT mappings change; idle ones expire).
const STUN_RETRY: Duration = Duration::from_secs(2);
const STUN_REFRESH: Duration = Duration::from_secs(300);
/// Longest wait of the hub loop without input (str0m asks for less when busy).
const IDLE_TICK: Duration = Duration::from_millis(100);

/// What a session hears from its peer connection.
#[derive(Debug)]
pub enum PeerEvent {
    /// The frames channel is open: frames may go over UDP.
    FramesOpen,
    /// ACKs and other client messages on the frames channel.
    Frames(Vec<u8>),
    /// Input events.
    Input(Vec<u8>),
    /// The peer connection is gone (ICE failed, closed): back to the WebSocket.
    Closed,
}

enum HubMsg {
    Packet { socket: usize, source: SocketAddr, data: Vec<u8>, at: Instant },
    Open { id: u64, offer: String, events: tmpsc::UnboundedSender<PeerEvent>, reply: oneshot::Sender<Result<String>> },
    Candidate { id: u64, candidate: String },
    Send { id: u64, data: Vec<u8> },
    Close { id: u64 },
}

/// Handle to the hub thread.
pub struct Hub {
    tx: mpsc::Sender<HubMsg>,
    /// `stun:` URLs for browsers.
    pub ice_servers: Vec<String>,
}

impl Hub {
    /// Binds the UDP sockets and starts the hub. `default_port` is used when
    /// the settings leave the port at 0.
    pub fn start(settings: &WebRtcSettings, default_port: u16) -> Result<Hub> {
        let port = if settings.udp_port == 0 { default_port } else { settings.udp_port };
        let mut sockets = Vec::new();
        for ip in local_ipv4()? {
            match UdpSocket::bind(SocketAddr::new(IpAddr::V4(ip), port)) {
                Ok(s) => sockets.push(s),
                // An interface we cannot use (e.g. one that went away) is not fatal.
                Err(e) => eprintln!("webrtc: cannot bind udp {ip}:{port}: {e}"),
            }
        }
        if sockets.is_empty() {
            return Err(anyhow!("no UDP socket could be bound on port {port}"));
        }
        let stun: Vec<SocketAddr> = settings
            .stun_servers
            .iter()
            .filter_map(|s| match s.to_socket_addrs() {
                Ok(mut a) => a.find(SocketAddr::is_ipv4),
                Err(e) => {
                    eprintln!("webrtc: STUN server {s}: {e}");
                    None
                }
            })
            .collect();
        // "ip" (the socket's own port) or "ip:port" (a different published port).
        let advertise: Vec<(IpAddr, Option<u16>)> = settings
            .advertise
            .iter()
            .map(|a| match a.parse::<SocketAddr>() {
                Ok(s) => Ok((s.ip(), Some(s.port()))),
                Err(_) => a.parse().map(|ip| (ip, None)).with_context(|| format!("webrtc.advertise: {a}")),
            })
            .collect::<Result<_>>()?;

        let (tx, rx) = mpsc::channel();
        for (index, socket) in sockets.iter().enumerate() {
            let (socket, tx) = (socket.try_clone()?, tx.clone());
            std::thread::Builder::new().name(format!("udp-{}", socket.local_addr()?)).spawn(move || {
                let mut buf = vec![0u8; 2048];
                // Errors here are per packet (e.g. ICMP port unreachable on Windows).
                loop {
                    if let Ok((n, source)) = socket.recv_from(&mut buf) {
                        let packet = HubMsg::Packet { socket: index, source, data: buf[..n].to_vec(), at: Instant::now() };
                        if tx.send(packet).is_err() {
                            return;
                        }
                    }
                }
            })?;
        }
        let addrs: Vec<String> = sockets.iter().filter_map(|s| s.local_addr().ok()).map(|a| a.to_string()).collect();
        eprintln!("webrtc: udp on {}", addrs.join(", "));
        let mut hub = HubThread { rx, sockets, peers: HashMap::new(), stun, stun_pending: Vec::new(), next_stun: Instant::now(), mapped: Vec::new(), advertise };
        std::thread::Builder::new().name("webrtc".into()).spawn(move || hub.run())?;
        let ice_servers = settings.stun_servers.iter().map(|s| format!("stun:{s}")).collect();
        Ok(Hub { tx, ice_servers })
    }

    /// Answers a browser offer; ICE starts immediately.
    pub async fn open(&self, id: u64, offer: String, events: tmpsc::UnboundedSender<PeerEvent>) -> Result<String> {
        let (reply, answer) = oneshot::channel();
        self.tx.send(HubMsg::Open { id, offer, events, reply }).map_err(|_| anyhow!("webrtc hub stopped"))?;
        answer.await.map_err(|_| anyhow!("webrtc hub stopped"))?
    }

    /// A remote ICE candidate (trickled by the browser).
    pub fn candidate(&self, id: u64, candidate: String) {
        let _ = self.tx.send(HubMsg::Candidate { id, candidate });
    }

    /// Queues a message on the frames channel.
    pub fn send(&self, id: u64, data: Vec<u8>) {
        let _ = self.tx.send(HubMsg::Send { id, data });
    }

    pub fn close(&self, id: u64) {
        let _ = self.tx.send(HubMsg::Close { id });
    }
}

struct Peer {
    rtc: Rtc,
    events: tmpsc::UnboundedSender<PeerEvent>,
    frames: Option<ChannelId>,
    input: Option<ChannelId>,
    /// Frames-channel messages waiting for SCTP buffer space.
    queue: VecDeque<Vec<u8>>,
    timeout: Instant,
    closed: bool,
}

struct HubThread {
    rx: mpsc::Receiver<HubMsg>,
    sockets: Vec<UdpSocket>,
    peers: HashMap<u64, Peer>,
    stun: Vec<SocketAddr>,
    /// Outstanding STUN binding requests: (transaction id, socket index).
    stun_pending: Vec<([u8; 12], usize)>,
    next_stun: Instant,
    /// Public addresses learned through STUN: (mapped, socket index).
    mapped: Vec<(SocketAddr, usize)>,
    advertise: Vec<(IpAddr, Option<u16>)>,
}

impl HubThread {
    fn run(&mut self) {
        loop {
            let now = Instant::now();
            if now >= self.next_stun {
                self.query_stun(now);
            }
            let deadline = self.peers.values().map(|p| p.timeout).min().unwrap_or(now + IDLE_TICK).min(self.next_stun);
            match self.rx.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(msg) => self.handle(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            let due: Vec<u64> = self.peers.iter().filter(|(_, p)| p.timeout <= now).map(|(&id, _)| id).collect();
            for id in due {
                self.mutate(id, |rtc| rtc.handle_input(Input::Timeout(now)).map_err(Into::into));
            }
            self.peers.retain(|_, p| {
                let alive = p.rtc.is_alive() && !p.closed;
                if !alive {
                    let _ = p.events.send(PeerEvent::Closed);
                }
                alive
            });
        }
    }

    fn handle(&mut self, msg: HubMsg) {
        match msg {
            HubMsg::Packet { socket, source, data, at } => self.packet(socket, source, &data, at),
            HubMsg::Open { id, offer, events, reply } => {
                let _ = reply.send(self.open(id, &offer, events));
            }
            HubMsg::Candidate { id, candidate } => {
                // mDNS (".local") host candidates of browsers cannot be resolved
                // here; the peer-reflexive address from their checks replaces them.
                if let Ok(c) = Candidate::from_sdp_string(candidate.trim_start_matches("a=")) {
                    self.mutate(id, |rtc| {
                        rtc.add_remote_candidate(c);
                        Ok(())
                    });
                }
            }
            HubMsg::Send { id, data } => {
                if let Some(p) = self.peers.get_mut(&id) {
                    p.queue.push_back(data);
                    self.flush(id);
                }
            }
            HubMsg::Close { id } => {
                if let Some(mut p) = self.peers.remove(&id) {
                    p.rtc.disconnect();
                }
            }
        }
    }

    fn open(&mut self, id: u64, offer: &str, events: tmpsc::UnboundedSender<PeerEvent>) -> Result<String> {
        let offer = SdpOffer::from_sdp_string(offer).context("parse SDP offer")?;
        let mut rtc = Rtc::builder().set_sctp_max_buffered_amount(SCTP_BUFFER).build(Instant::now());
        for c in self.candidates() {
            rtc.add_local_candidate(c);
        }
        let answer = rtc.sdp_api().accept_offer(offer).context("accept SDP offer")?;
        // A reconnecting session replaces its previous peer.
        if let Some(mut old) = self.peers.remove(&id) {
            old.rtc.disconnect();
        }
        self.peers.insert(id, Peer { rtc, events, frames: None, input: None, queue: VecDeque::new(), timeout: Instant::now(), closed: false });
        self.drain(id);
        Ok(answer.to_sdp_string())
    }

    /// Host candidates for every socket, STUN-mapped and advertised addresses.
    fn candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for s in &self.sockets {
            let Ok(local) = s.local_addr() else { continue };
            if let Ok(c) = Candidate::host(local, "udp") {
                out.push(c);
            }
        }
        for &(mapped, index) in &self.mapped {
            if let Ok(base) = self.sockets[index].local_addr() {
                if mapped != base {
                    out.extend(Candidate::server_reflexive(mapped, base, "udp").ok());
                }
            }
        }
        // Advertised addresses forward to the first non-loopback socket (a
        // container's published port lands on its outward interface).
        let outward = self.sockets.iter().filter_map(|s| s.local_addr().ok()).find(|a| !a.ip().is_loopback());
        if let Some(base) = outward {
            for &(ip, port) in &self.advertise {
                let addr = SocketAddr::new(ip, port.unwrap_or(base.port()));
                out.extend(Candidate::server_reflexive(addr, base, "udp").ok());
            }
        }
        out
    }

    fn packet(&mut self, socket: usize, source: SocketAddr, data: &[u8], at: Instant) {
        if self.stun_response(socket, data) {
            return;
        }
        let Ok(destination) = self.sockets[socket].local_addr() else { return };
        let Ok(contents) = data.try_into() else { return };
        let input = Input::Receive(at, Receive { proto: Protocol::Udp, source, destination, contents });
        let Some(id) = self.peers.iter().find(|(_, p)| p.rtc.accepts(&input)).map(|(&id, _)| id) else { return };
        if let Some(p) = self.peers.get_mut(&id) {
            if let Err(e) = p.rtc.handle_input(input) {
                eprintln!("webrtc: client {id}: {e}");
                p.closed = true;
            }
        }
        self.drain(id);
        self.flush(id);
    }

    /// One mutation followed by a full drain (str0m's contract).
    fn mutate(&mut self, id: u64, f: impl FnOnce(&mut Rtc) -> Result<()>) {
        let Some(p) = self.peers.get_mut(&id) else { return };
        if let Err(e) = f(&mut p.rtc) {
            eprintln!("webrtc: client {id}: {e:#}");
            p.closed = true;
        }
        self.drain(id);
        self.flush(id);
    }

    /// Writes queued frames-channel messages while the SCTP buffer has room.
    fn flush(&mut self, id: u64) {
        loop {
            let Some(p) = self.peers.get_mut(&id) else { return };
            let (Some(cid), Some(data)) = (p.frames, p.queue.front()) else { return };
            let Some(mut channel) = p.rtc.channel(cid) else { return };
            match channel.write(true, data) {
                Ok(true) => {
                    p.queue.pop_front();
                }
                // Full: retried on ChannelBufferedAmountLow.
                Ok(false) => return,
                Err(e) => {
                    eprintln!("webrtc: client {id}: write: {e}");
                    p.closed = true;
                    return;
                }
            }
            self.drain(id);
        }
    }

    fn drain(&mut self, id: u64) {
        let Some(p) = self.peers.get_mut(&id) else { return };
        loop {
            let output = match p.rtc.poll_output() {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("webrtc: client {id}: {e}");
                    p.closed = true;
                    return;
                }
            };
            match output {
                Output::Timeout(t) => {
                    p.timeout = t;
                    return;
                }
                Output::Transmit(t) => {
                    if let Some(s) = self.sockets.iter().find(|s| s.local_addr().is_ok_and(|a| a == t.source)) {
                        let _ = s.send_to(&t.contents, t.destination);
                    }
                }
                Output::Event(e) => match e {
                    Event::ChannelOpen(cid, label) => {
                        if label == FRAMES_LABEL {
                            p.frames = Some(cid);
                            if let Some(mut c) = p.rtc.channel(cid) {
                                c.set_buffered_amount_low_threshold(SCTP_BUFFER / 4);
                            }
                            let _ = p.events.send(PeerEvent::FramesOpen);
                        } else if label == INPUT_LABEL {
                            p.input = Some(cid);
                        }
                    }
                    Event::ChannelData(d) => {
                        let event = if Some(d.id) == p.frames {
                            PeerEvent::Frames(d.data)
                        } else if Some(d.id) == p.input {
                            PeerEvent::Input(d.data)
                        } else {
                            continue;
                        };
                        let _ = p.events.send(event);
                    }
                    Event::ChannelClose(cid) if Some(cid) == p.frames => p.closed = true,
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => p.closed = true,
                    _ => {}
                },
            }
        }
    }

    fn query_stun(&mut self, now: Instant) {
        // Unanswered requests are simply re-sent: a lost one costs a retry.
        self.stun_pending.clear();
        for (index, socket) in self.sockets.iter().enumerate() {
            let Ok(local) = socket.local_addr() else { continue };
            if local.ip().is_loopback() {
                continue;
            }
            for &server in &self.stun {
                let txid: [u8; 12] = rand::random();
                let mut req = Vec::with_capacity(20);
                req.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xA4, 0x42]);
                req.extend_from_slice(&txid);
                if socket.send_to(&req, server).is_ok() {
                    self.stun_pending.push((txid, index));
                }
            }
        }
        let answered = self.stun.is_empty() || self.sockets.iter().enumerate().all(|(i, s)| {
            s.local_addr().is_ok_and(|a| a.ip().is_loopback()) || self.mapped.iter().any(|&(_, j)| j == i)
        });
        self.next_stun = now + if answered { STUN_REFRESH } else { STUN_RETRY };
    }

    /// Handles a STUN binding success response to one of our requests.
    fn stun_response(&mut self, socket: usize, data: &[u8]) -> bool {
        if data.len() < 20 || data[0..2] != [0x01, 0x01] || data[4..8] != [0x21, 0x12, 0xA4, 0x42] {
            return false;
        }
        let Some(pos) = self.stun_pending.iter().position(|(t, i)| *t == data[8..20] && *i == socket) else {
            return false;
        };
        self.stun_pending.swap_remove(pos);
        if let Some(mapped) = xor_mapped_address(data) {
            if !self.mapped.contains(&(mapped, socket)) {
                eprintln!("webrtc: public address {mapped} (STUN)");
                self.mapped.retain(|&(_, i)| i != socket);
                self.mapped.push((mapped, socket));
            }
        }
        true
    }
}

/// XOR-MAPPED-ADDRESS (IPv4) of a STUN binding response (RFC 5389 §15.2).
fn xor_mapped_address(msg: &[u8]) -> Option<SocketAddr> {
    let mut at = 20;
    while at + 4 <= msg.len() {
        let kind = u16::from_be_bytes([msg[at], msg[at + 1]]);
        let len = u16::from_be_bytes([msg[at + 2], msg[at + 3]]) as usize;
        let value = msg.get(at + 4..at + 4 + len)?;
        if kind == 0x0020 && len >= 8 && value[1] == 0x01 {
            let port = u16::from_be_bytes([value[2], value[3]]) ^ 0x2112;
            let ip = Ipv4Addr::new(value[4] ^ 0x21, value[5] ^ 0x12, value[6] ^ 0xA4, value[7] ^ 0x42);
            return Some(SocketAddr::new(IpAddr::V4(ip), port));
        }
        // Attributes are padded to 4 bytes.
        at += 4 + len.div_ceil(4) * 4;
    }
    None
}

/// IPv4 addresses of the up interfaces, loopback included (local browsers).
pub(crate) fn local_ipv4() -> Result<Vec<Ipv4Addr>> {
    let mut ips: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .context("list network interfaces")?
        .into_iter()
        .filter_map(|i| match i.ip() {
            // Link-local (169.254/16) addresses come from interfaces without a
            // working network; nothing can reach them.
            IpAddr::V4(ip) if !ip.is_link_local() && !ip.is_unspecified() => Some(ip),
            _ => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    Ok(ips)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xor_mapped_address() {
        // RFC 5769 §2.2 sample response, IPv4 192.0.2.1:32853.
        let mut msg = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
        msg.extend_from_slice(&[0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae]);
        msg.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43]);
        assert_eq!(xor_mapped_address(&msg), Some("192.0.2.1:32853".parse().unwrap()));
    }
}
