//! Binary wire protocol. All integers little-endian. Mirrored in `client/src/proto.ts`.
//! Identical on every platform.

use crate::platform::CursorState;

// Server -> client
/// width u32, height u32, tile u32, first_seq u32 (at offsets 4/8/12/16). Sent
/// on connect and whenever the streamed display changes; the client resets
/// its layers and decoder, drops frames before `first_seq` and answers
/// C_HELLO_ACK. No frame of the new display is sent before that answer, so
/// frames on another path (data channel) can never overtake it.
pub const S_HELLO: u8 = 1;
pub const S_FRAME: u8 = 2;
/// w u16, h u16, xhot u16, yhot u16, pad u16, RGBA. w = h = 0 hides the cursor.
pub const S_CURSOR: u8 = 3;
pub const S_PONG: u8 = 4;
/// UTF-8 JSON, WebRTC signaling over the WebSocket: `{"type":"config",
/// "iceServers":[...]}` on connect, `{"type":"answer","sdp":...}`,
/// `{"type":"closed"}` (the server dropped the peer) and `{"type":"slow"}`
/// (the data channel carries less than the WebSocket did: leave it).
pub const S_SIGNAL: u8 = 5;
/// One piece of a FRAME on the data channel: flags u8 (FLAG_VIDEO of the
/// frame), part u16, parts u16, pad u16, seq u32 (offsets 1/2/4/6/8), data.
pub const S_CHUNK: u8 = 6;
pub const CHUNK_HEADER_LEN: usize = 12;
/// Chunk payload: data channel messages must stay within 64 KiB, the size
/// every browser accepts without negotiation.
pub const CHUNK_PAYLOAD: usize = 60 * 1024;

// Client -> server
/// client_ts f64 at offset 8; answered with S_PONG on the same path (the
/// WebSocket or the data channel: the client compares their round trips).
pub const C_PING: u8 = 1;
/// Frames handled: n_lost u8, pad u16, seq u32, hold_us u32, then n_lost x
/// seq u32. `seq` is the last frame drawn; every earlier frame was drawn or
/// is listed as lost (in this ACK or an earlier one). `hold_us` is the client
/// time from receiving that frame to drawing it, excluded from the network
/// round trip.
pub const C_ACK: u8 = 2;
/// The client's decoder needs an IDR frame: pad u8 x 3, seq u32 of the
/// newest frame it received (requests older than the last keyframe sent are
/// already answered).
pub const C_KEYFRAME: u8 = 3;
/// UTF-8 JSON with the client's measured stats (shown in the admin UI).
pub const C_STATS: u8 = 4;
/// UTF-8 JSON, WebRTC signaling: `{"type":"offer","sdp":...}`,
/// `{"type":"candidate","candidate":"candidate:..."}`, and the path the
/// client wants frames on: `{"type":"path","webrtc":bool,"dead":bool}`
/// (`dead`: it gave the data channel up; frames left there are lost).
pub const C_SIGNAL: u8 = 5;
/// The client applied the last HELLO.
pub const C_HELLO_ACK: u8 = 6;
/// visible u8: the tab is shown (1) or hidden (0); a hidden tab gets no frames.
pub const C_VISIBLE: u8 = 7;
/// UTF-8 text: something the client noticed (a freeze, a path change and
/// why), for the server's event log.
pub const C_EVENT: u8 = 8;
pub const C_MOVE: u8 = 10;
pub const C_BUTTON: u8 = 11;
pub const C_WHEEL: u8 = 12;
pub const C_KEY: u8 = 13;

/// Rect payload encodings.
pub const ENC_RAW: u8 = 0;
/// LZ4 block (lz4_flex::block) of the raw BGRX rows.
pub const ENC_LZ4: u8 = 1;
/// zstd frame of the raw BGRX rows (used on slow links: smaller, more CPU).
pub const ENC_ZSTD: u8 = 2;

/// FRAME flags.
pub const FLAG_VIDEO: u8 = 1;
pub const FLAG_KEY: u8 = 2;

/// FRAME header: type u8, flags u8, n_rects u16, seq u32,
/// capture_oldest_us u64, capture_newest_us u64, send_us u64,
/// pull_us u64 (frame build started), encode_us u32 (H.264 encode only),
/// n_copies u16, upload u16 (encoder input upload, 10 µs units). Followed by:
/// - copies, each 6 x u16: src_x, src_y, dst_x, dst_y, w, h — screen moves to
///   replay on the lossless layer, in order, before the rects
/// - rects, each: x u16, y u16, w u16, h u16, enc u8, pad u8, pad u16, len u32, data
/// - n_switches u16, then u16 each: tile index | 0x8000 when the tile moves to video
/// - if FLAG_VIDEO: len u32 + H.264 Annex B access unit
pub const FRAME_HEADER_LEN: usize = 48;

pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

pub fn hello(width: u32, height: u32, tile: u32, first_seq: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(20);
    m.extend_from_slice(&[S_HELLO, 0, 0, 0]);
    for v in [width, height, tile, first_seq] {
        m.extend_from_slice(&v.to_le_bytes());
    }
    m
}

pub fn signal(json: &serde_json::Value) -> Vec<u8> {
    let mut m = vec![S_SIGNAL];
    m.extend_from_slice(json.to_string().as_bytes());
    m
}

/// Splits a FRAME into data channel chunks.
pub fn chunks(frame: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    let flags = frame[1] & FLAG_VIDEO;
    let seq = &frame[4..8];
    let parts = frame.len().div_ceil(CHUNK_PAYLOAD) as u16;
    frame.chunks(CHUNK_PAYLOAD).enumerate().map(move |(part, data)| {
        let mut m = Vec::with_capacity(CHUNK_HEADER_LEN + data.len());
        m.extend_from_slice(&[S_CHUNK, flags]);
        m.extend_from_slice(&(part as u16).to_le_bytes());
        m.extend_from_slice(&parts.to_le_bytes());
        m.extend_from_slice(&[0, 0]);
        m.extend_from_slice(seq);
        m.extend_from_slice(data);
        m
    })
}

pub fn pong(client_ts: f64) -> Vec<u8> {
    let mut m = Vec::with_capacity(24);
    m.extend_from_slice(&[S_PONG, 0, 0, 0, 0, 0, 0, 0]);
    m.extend_from_slice(&client_ts.to_le_bytes());
    m.extend_from_slice(&now_us().to_le_bytes());
    m
}

pub fn cursor(state: &CursorState) -> Vec<u8> {
    let (w, h, xhot, yhot, rgba): (u32, u32, u32, u32, &[u8]) = match state {
        CursorState::Hidden => (0, 0, 0, 0, &[]),
        CursorState::Visible(s) => (s.width, s.height, s.hot_x, s.hot_y, &s.rgba),
    };
    let mut m = Vec::with_capacity(12 + rgba.len());
    m.extend_from_slice(&[S_CURSOR, 0]);
    for v in [w, h, xhot, yhot] {
        m.extend_from_slice(&(v as u16).to_le_bytes());
    }
    m.extend_from_slice(&[0, 0]);
    m.extend_from_slice(rgba);
    m
}
