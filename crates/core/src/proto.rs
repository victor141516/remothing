//! Binary wire protocol. All integers little-endian. Mirrored in `client/src/proto.ts`.
//! Identical on every platform.

use crate::platform::CursorState;

// Server -> client
/// width u32, height u32, tile u32, first_seq u32, hdr_max f32 (at offsets
/// 4/8/12/16/20). Sent on connect and whenever the streamed display (or its
/// picture) changes; the client resets its layers and decoder, drops frames
/// before `first_seq` and answers C_HELLO_ACK. No frame of the new display is
/// sent before that answer, so frames on another path (data channel) can
/// never overtake it. `hdr_max` 0: an SDR picture (BGRX tiles, sRGB video).
/// Above 0: an HDR picture (`platform::Picture`): tiles are `R10G10B10A2`,
/// and tiles and video carry each channel's linear value `v`, relative to
/// the host's SDR white, through the curve
///   e = a * srgb(v)                           for v <= 1
///   e = a + (1 - a) * ln(v) / ln(hdr_max)     for 1 < v <= hdr_max
/// with `a = hdr_split(hdr_max)`: most codes for the SDR range, the rest for
/// highlights, in 10 bits for tiles and 8 for video.
pub const S_HELLO: u8 = 1;
pub const S_FRAME: u8 = 2;
/// w u16, h u16, xhot u16, yhot u16, pad u16, RGBA. w = h = 0 hides the cursor.
pub const S_CURSOR: u8 = 3;
pub const S_PONG: u8 = 4;
/// UTF-8 JSON, signaling over the WebSocket. WebRTC: `{"type":"config",
/// "iceServers":[...]}` on connect, `{"type":"answer","sdp":...}`,
/// `{"type":"closed"}` (the server dropped the peer) and `{"type":"slow"}`
/// (the data channel carries less than the WebSocket did: leave it).
/// `{"type":"trace"}` on connect while `[debug] frame_trace` is on: record
/// the frames drawn and send them (C_TRACE).
/// WebTransport: `{"type":"wt","key","hash","port","candidates":[...]}`
/// on connect and when the certificate is renewed: `candidates` are URLs to
/// try (`key` goes in their query as `k`), or bare hosts for
/// `https://host:port/wt`, the empty one standing for the page's own host;
/// `hash` is the SHA-256 (hex) of the certificate to accept at the hosts
/// (full URLs are checked as any HTTPS site).
pub const S_SIGNAL: u8 = 5;
/// One piece of a FRAME on the data channel: flags u8 (FLAG_VIDEO of the
/// frame), part u16, parts u16, pad u16, seq u32 (offsets 1/2/4/6/8), data.
pub const S_CHUNK: u8 = 6;
pub const CHUNK_HEADER_LEN: usize = 12;
/// Chunk payload: data channel messages must stay within 64 KiB, the size
/// every browser accepts without negotiation.
pub const CHUNK_PAYLOAD: usize = 60 * 1024;
/// flags u8, UTF-8 text now on the session's clipboard: on connect and after
/// a display switch when not empty (flags 0), then on every change another
/// client or the session itself makes (CLIPBOARD_CHANGED), which the client
/// copies to its own computer's clipboard; what was there before it came is
/// left alone.
pub const S_CLIPBOARD: u8 = 7;
pub const CLIPBOARD_CHANGED: u8 = 1;
/// UTF-8 JSON answering C_DISPLAYS, C_DISPLAY or C_VIRTUAL: `{"displays":[{"id",
/// "name","width","height","primary","kind","mode"?,...}],"current":id,
/// "default":id,"streamed":{id:clients},"virtual":null|{"setup":{"state":
/// "ready"|"install"|"unavailable","notice"|"reason"},"capabilities":{...}},
/// "error"?:text}`. `current` is the display this client streams; `error`
/// the failure of its last C_VIRTUAL. Also sent to every client whenever
/// displays come, change or go, or the default changes.
pub const S_DISPLAYS: u8 = 8;
/// UTF-8 JSON, after C_AUDIO and whenever it changes: the sound stream
/// `{"active":true,"codec","rate","channels","description":hex,"source"}`
/// (`codec` as WebCodecs names it, `description` its out-of-band
/// configuration) or `{"active":false,"reason","tried":[...]}`.
pub const S_AUDIO_INFO: u8 = 9;
/// One encoded sound packet.
pub const S_AUDIO: u8 = 10;
/// UTF-8 JSON, the microphone as this client sees it: `{"state":"free"}`,
/// `"busy"` (another client talks), `"active"` (`sink`: where it plays),
/// `"install"` (`component`: `{name, notice, restart}` is missing on the
/// server and can be installed), `"installing"`, `"restart"` (installed,
/// the computer has to restart) or `"unavailable"` (`reason`, `tried`).
pub const S_MIC_INFO: u8 = 11;
/// One piece of a FRAME as a WebTransport datagram: flags u8 (FLAG_VIDEO of
/// the frame), index u16, data pieces u16, parity pieces u16, seq u32, frame
/// length u32 (offsets 1/2/4/6/8/12), payload. Pieces `0..data` carry the
/// frame in order, PIECE_PAYLOAD bytes each but the last. Piece `data + j`
/// is parity: the XOR of the data pieces whose index is `j` modulo `parity`
/// (short ones padded with zeros), so one lost piece of each such group can
/// be rebuilt, and a run of up to `parity` lost in a row too. How many
/// parity pieces a frame has follows the loss the client could not repair
/// (`datagrams::Fec`).
pub const S_PIECE: u8 = 12;
pub const PIECE_HEADER_LEN: usize = 16;
/// flags u8, x u16, y u16: where the remote cursor's hot spot is, display
/// pixels, when it moves. To a client that captured the mouse (C_CAPTURE),
/// and while another client has it captured to every other client, with
/// CURSOR_FOLLOW: its user's mouse is in that other window, so it draws the
/// cursor itself. The same without CURSOR_FOLLOW ends that.
pub const S_CURSOR_POS: u8 = 13;
pub const CURSOR_FOLLOW: u8 = 1;
/// Empty: the C_PASTE of this client is done (its V pressed, or failed).
pub const S_PASTED: u8 = 14;
/// UTF-8 JSON, the files on the session's clipboard, on connect, after a
/// display switch and on every change: `{"files":[{"name","size","folder"}]}`
/// (empty: none). The page downloads one with `GET /files/clipboard`
/// (`files.rs`).
pub const S_CLIPBOARD_FILES: u8 = 15;
/// flags u8 as S_CLIPBOARD (CLIPBOARD_CHANGED), then the PNG picture now on
/// the session's clipboard (at most `platform::MAX_IMAGE` bytes). Sent like
/// S_CLIPBOARD: text and picture are alternatives, the latest copy wins.
pub const S_CLIPBOARD_IMAGE: u8 = 16;
/// Fits a QUIC datagram on the smallest path every QUIC connection has
/// (1200 bytes), with the headers around it.
pub const PIECE_PAYLOAD: usize = 1100;

// Client -> server
/// client_ts f64 at offset 8; answered with S_PONG on the same path (the
/// WebSocket or the data channel: the client compares their round trips).
pub const C_PING: u8 = 1;
/// Frames handled: n_lost u8, pad u16, seq u32, hold_us u32, arrived_us u64,
/// then n_lost x seq u32. `seq` is the last frame drawn; every earlier frame
/// was drawn or is listed as lost (in this ACK or an earlier one). `hold_us`
/// is the client time from receiving that frame to drawing it, excluded from
/// the network round trip. `arrived_us` is when it was received, on the
/// client's own clock (0: no frame to time): against the send time it is
/// the one-way delay plus a constant, so the link model does not take delay
/// on the way back (the ACK's) for a queue on the way out. The first pad
/// byte is 1 when the frame was only complete after asking for pieces again
/// (WebTransport): when it arrived then says nothing about the link's queue.
pub const C_ACK: u8 = 2;
/// The client's decoder needs an IDR frame: pad u8 x 3, seq u32 of the
/// newest frame it received (requests older than the last keyframe sent are
/// already answered).
pub const C_KEYFRAME: u8 = 3;
/// UTF-8 JSON with the client's measured stats (shown in the admin UI).
pub const C_STATS: u8 = 4;
/// UTF-8 JSON, signaling: `{"type":"offer","sdp":...}` and
/// `{"type":"candidate","candidate":"candidate:..."}` (WebRTC), and the
/// path the client wants frames on: `{"type":"path","path":"ws"|"webrtc"|
/// "wt","dead":bool}` (`dead`: it gave the UDP path it was on up; frames
/// left there are lost).
pub const C_SIGNAL: u8 = 5;
/// The client applied the last HELLO.
pub const C_HELLO_ACK: u8 = 6;
/// visible u8: the tab is shown (1) or hidden (0); a hidden tab gets no frames.
pub const C_VISIBLE: u8 = 7;
/// UTF-8 text: something the client noticed (a freeze, a path change and
/// why), for the server's event log.
pub const C_EVENT: u8 = 8;
/// UTF-8 text to put on the session's clipboard.
pub const C_CLIPBOARD: u8 = 9;
pub const C_MOVE: u8 = 10;
pub const C_BUTTON: u8 = 11;
/// pad u8, dx i16, dy i16: scroll in wheel units, WHEEL_NOTCH per notch of a
/// mouse wheel (positive = right / down). Trackpads and touch send fractions
/// of a notch, as precision touchpads do on Windows.
pub const C_WHEEL: u8 = 12;
/// Wheel units per notch (Windows' WHEEL_DELTA).
pub const WHEEL_NOTCH: i32 = 120;
/// down u8, evdev code u16. A key stays down on the remote side until its
/// release arrives or the connection that pressed it goes away, so holding
/// it repeats there as on a local keyboard.
pub const C_KEY: u8 = 13;
/// Asks for the display list (answered with S_DISPLAYS).
pub const C_DISPLAYS: u8 = 14;
/// UTF-8 display id: this client streams that display from now on.
pub const C_DISPLAY: u8 = 15;
/// UTF-8 text to type as characters, whatever the remote keyboard layout
/// (a phone's keyboard gives text, not keys). `\n` is the Enter key.
pub const C_TEXT: u8 = 16;
/// on u8: send this client the session's sound (S_AUDIO_INFO, S_AUDIO) or stop.
pub const C_AUDIO: u8 = 17;
/// UTF-8 JSON about this client's microphone: `{"on":true,"codec":"opus"|
/// "pcm","rate","channels"}` asks for the session's microphone (the first
/// client to ask gets it), `{"on":false}` releases it, `{"install":true}`
/// and `{"restart":true}` answer the S_MIC_INFO states of those names.
pub const C_MIC_INFO: u8 = 18;
/// One packet of the microphone, in the format given with C_MIC_INFO.
pub const C_MIC: u8 = 19;

/// Pieces of a frame that did not arrive (WebTransport datagram): pad u8,
/// count u16, seq u32, then count x index u16. The server sends them again.
pub const C_NACK: u8 = 20;
/// How the client shows the picture: flags u8, width u16, height u16 (its
/// size in the window, device pixels; sent at once, then a moment after the
/// window settles), refresh rate u16 (Hz, 0: unknown). In motion mode the
/// video is encoded no larger than the link affords and never smaller than
/// this, and never at more frames per second than the refresh rate. Flags:
/// VIEW_HDR, the client can show an HDR picture where it is now.
pub const C_VIEW: u8 = 21;
pub const VIEW_HDR: u8 = 1;
/// flags u8, dx i16, dy i16: move the pointer by that many mouse counts
/// (the client captured the mouse; a game reads them as they are). With
/// MOVE_AS_POSITION the server puts the pointer where it is now plus the
/// counts, as the absolute move of an uncaptured mouse (to compare how
/// programs take either).
pub const C_MOVE_REL: u8 = 22;
pub const MOVE_AS_POSITION: u8 = 1;
/// on u8: the client has the mouse captured (1) or not (0); while it has,
/// it gets S_CURSOR_POS.
pub const C_CAPTURE: u8 = 23;
/// UTF-8 JSON, a virtual display request: `{"op":"install"}` (what
/// S_DISPLAYS' `virtual.setup` asked for), `{"op":"create","mode":{"width",
/// "height","refresh_hz","hdr"}}` (and stream it), `{"op":"set","id","mode"}`,
/// `{"op":"destroy","id"}`. Answered with S_DISPLAYS.
pub const C_VIRTUAL: u8 = 24;
/// UTF-8 text: the client's own clipboard, for a Ctrl+V on its side. The
/// server puts it on the clipboard of the display (empty: leaves that as it
/// is), presses V there (the client already holds Ctrl, and holds back its
/// next keys) and answers S_PASTED. It travels on this socket because a
/// clipboard may be larger than an input message can be.
pub const C_PASTE: u8 = 25;
/// Empty: this browser shows BT.601 video (FLAG_BT601) in the wrong colours
/// (its decoder or renderer ignores the colour space it is given). From
/// then on this client's video is BT.709, converted by the server where
/// that costs (NVENC).
pub const C_BT709: u8 = 26;
/// UTF-8 JSON, one batch of the frame trace the server asked for with
/// `{"type":"trace"}` (S_SIGNAL), every 2 s while frames are drawn:
/// `{"offset", "frames": [[seq, bytes, flags, oldest, newest, pulled,
/// upload, encode, sent, received, decoded, drawn], ...], "refreshes":
/// [...]}`. Times are ms on the server's clock (the client's own converted
/// with its clock offset estimate, `offset` = server - client); `oldest`,
/// `newest`, `pulled`, `upload`, `encode` and `sent` are the FRAME header's,
/// `decoded` is 0 without video; `refreshes` are the display refreshes
/// (`requestAnimationFrame`) since the last batch.
pub const C_TRACE: u8 = 27;
/// flags u8, then a PNG picture (at most `platform::MAX_IMAGE` bytes) to put
/// on the session's clipboard. With CLIPBOARD_PASTE it is the client's own
/// clipboard for a Ctrl+V, handled as C_PASTE: set, V pressed, S_PASTED.
pub const C_CLIPBOARD_IMAGE: u8 = 28;
pub const CLIPBOARD_PASTE: u8 = 1;
/// pad u8, x u16, y u16, seq u32: C_MOVE where only the latest counts. Over
/// UDP paths each goes twice: unordered (a WebTransport datagram, the WebRTC
/// "moves" channel) so a lost move holds no later one back, and the ordered
/// way, so a lost one still comes after a round trip, ahead of later input.
/// Moves not newer than the last applied (by `seq`, wrapping) are dropped.
pub const C_MOVE_LATEST: u8 = 29;

/// WebSocket close code for a token the server does not accept. Browsers do
/// not show a page the HTTP status of a refused upgrade, so the server
/// upgrades and closes with this; the client then stops reconnecting.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

/// Rect payload encodings.
pub const ENC_RAW: u8 = 0;
/// LZ4 block (lz4_flex::block) of the raw BGRX rows.
pub const ENC_LZ4: u8 = 1;
/// zstd frame of the raw BGRX rows (used on slow links: smaller, more CPU).
pub const ENC_ZSTD: u8 = 2;

/// FRAME flags.
pub const FLAG_VIDEO: u8 = 1;
pub const FLAG_KEY: u8 = 2;
/// The screen is in motion mode: what changes is shown from video until
/// the whole screen has been still for a while.
pub const FLAG_MOTION: u8 = 4;
/// Bits 3-4: the codec of the video (`Codec::wire`: 0 H.264, 1 HEVC, 2 AV1).
/// A change of codec comes with a keyframe; the client reconfigures its
/// decoder for it.
pub const FLAG_CODEC_SHIFT: u8 = 3;
/// The video's YUV is BT.601 (NVENC converting RGB input itself), not
/// BT.709: the client decodes it with that matrix, or asks for BT.709
/// (C_BT709) when its browser cannot.
pub const FLAG_BT601: u8 = 32;

/// FRAME header: type u8, flags u8, n_rects u16, seq u32,
/// capture_oldest_us u64, capture_newest_us u64, send_us u64,
/// pull_us u64 (frame build started), encode_us u32 (video encode only),
/// n_copies u16, upload u16 (encoder input upload, 10 µs units). Followed by:
/// - copies, each 6 x u16: src_x, src_y, dst_x, dst_y, w, h — screen moves to
///   replay on the lossless layer, in order, before the rects
/// - rects, each: x u16, y u16, w u16, h u16, enc u8, pad u8, pad u16, len u32, data
/// - n_switches u16, then u16 each: tile index | 0x8000 when the tile moves to video
/// - if FLAG_VIDEO: len u32 + one access unit of the codec in the flags
pub const FRAME_HEADER_LEN: usize = 48;

pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

pub fn hello(width: u32, height: u32, tile: u32, first_seq: u32, hdr_max: f32) -> Vec<u8> {
    let mut m = Vec::with_capacity(24);
    m.extend_from_slice(&[S_HELLO, 0, 0, 0]);
    for v in [width, height, tile, first_seq] {
        m.extend_from_slice(&v.to_le_bytes());
    }
    m.extend_from_slice(&hdr_max.to_le_bytes());
    m
}

/// Share of the codes the SDR range gets in the HDR curve (see S_HELLO): the
/// two pieces meet with the same slope, so code steps stay even across white.
/// 0.4396 is the slope of the sRGB curve at 1 (1.055 / 2.4).
pub fn hdr_split(hdr_max: f32) -> f32 {
    1.0 / (1.0 + 1.055 / 2.4 * hdr_max.max(1.0).ln())
}

pub fn signal(json: &serde_json::Value) -> Vec<u8> {
    text(S_SIGNAL, &json.to_string())
}

/// A tagged UTF-8 message.
pub fn text(tag: u8, text: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(1 + text.len());
    m.push(tag);
    m.extend_from_slice(text.as_bytes());
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

/// Data and parity pieces a FRAME of `len` bytes is sent as over
/// WebTransport, one parity piece for every `per` data pieces.
pub fn piece_counts(len: usize, per: usize) -> (usize, usize) {
    let data = len.div_ceil(PIECE_PAYLOAD).max(1);
    (data, data.div_ceil(per))
}

/// Appends piece `index` of `frame` (see S_PIECE) to `out`; `per` as in
/// `piece_counts`, the same for every piece of the frame.
pub fn piece(frame: &[u8], index: usize, per: usize, out: &mut Vec<u8>) {
    let (data, parity) = piece_counts(frame.len(), per);
    out.extend_from_slice(&[S_PIECE, frame[1] & FLAG_VIDEO]);
    for v in [index, data, parity] {
        out.extend_from_slice(&(v as u16).to_le_bytes());
    }
    out.extend_from_slice(&frame[4..8]);
    out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    let chunk = |i: usize| &frame[i * PIECE_PAYLOAD..((i + 1) * PIECE_PAYLOAD).min(frame.len())];
    if index < data {
        out.extend_from_slice(chunk(index));
        return;
    }
    let at = out.len();
    for i in (index - data..data).step_by(parity) {
        let c = chunk(i);
        if out.len() < at + c.len() {
            out.resize(at + c.len(), 0);
        }
        for (o, b) in out[at..].iter_mut().zip(c) {
            *o ^= b;
        }
    }
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

pub fn cursor_pos(x: u32, y: u32, flags: u8) -> Vec<u8> {
    let mut m = vec![S_CURSOR_POS, flags];
    m.extend_from_slice(&(x.min(0xffff) as u16).to_le_bytes());
    m.extend_from_slice(&(y.min(0xffff) as u16).to_le_bytes());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame is whole again after losing any `parity` pieces in a row.
    #[test]
    fn parity_rebuilds_a_lost_run() {
        for (len, per) in [(FRAME_HEADER_LEN, 8), (PIECE_PAYLOAD, 2), (9 * PIECE_PAYLOAD - 3, 4), (40 * PIECE_PAYLOAD + 17, 16), (40 * PIECE_PAYLOAD, 2)] {
            let frame: Vec<u8> = (0..len).map(|i| (i * 31 + i / 7) as u8).collect();
            let (data, parity) = piece_counts(len, per);
            let payload = |index: usize| {
                let mut p = Vec::new();
                piece(&frame, index, per, &mut p);
                p.split_off(PIECE_HEADER_LEN)
            };
            for first in 0..data {
                let lost: Vec<usize> = (first..data.min(first + parity)).collect();
                let mut rebuilt = Vec::new();
                for i in 0..data {
                    let mut p = payload(i);
                    if lost.contains(&i) {
                        // Its group's parity, less the pieces that did arrive.
                        p = payload(data + i % parity);
                        for other in (i % parity..data).step_by(parity).filter(|&o| o != i) {
                            for (a, b) in p.iter_mut().zip(payload(other)) {
                                *a ^= b;
                            }
                        }
                        p.truncate(if i == data - 1 { len - i * PIECE_PAYLOAD } else { PIECE_PAYLOAD });
                    }
                    rebuilt.extend(p);
                }
                assert_eq!(rebuilt, frame, "len {len}, lost {lost:?}");
            }
        }
    }
}
