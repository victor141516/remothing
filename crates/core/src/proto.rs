//! Binary wire protocol. All integers little-endian. Mirrored in `client/src/proto.ts`.
//! Identical on every platform.

use crate::platform::CursorState;

// Server -> client
/// width u32, height u32, tile u32 (at offsets 4/8/12). Sent on connect and
/// whenever the streamed display changes; the client resets its layers and decoder.
pub const S_HELLO: u8 = 1;
pub const S_FRAME: u8 = 2;
/// w u16, h u16, xhot u16, yhot u16, pad u16, RGBA. w = h = 0 hides the cursor.
pub const S_CURSOR: u8 = 3;
pub const S_PONG: u8 = 4;

// Client -> server
pub const C_PING: u8 = 1;
/// Frame drawn: pad u8 x3, seq u32, hold_us u32 (client time from receiving
/// the frame to drawing it, excluded from the network round trip).
pub const C_ACK: u8 = 2;
/// Client decoder lost sync and needs an IDR frame.
pub const C_KEYFRAME: u8 = 3;
/// UTF-8 JSON with the client's measured stats (shown in the admin UI).
pub const C_STATS: u8 = 4;
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

pub fn hello(width: u32, height: u32, tile: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(16);
    m.extend_from_slice(&[S_HELLO, 0, 0, 0]);
    m.extend_from_slice(&width.to_le_bytes());
    m.extend_from_slice(&height.to_le_bytes());
    m.extend_from_slice(&tile.to_le_bytes());
    m
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
