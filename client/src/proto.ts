// Mirrors crates/core/src/proto.rs. Little-endian.

export const S_HELLO = 1;
export const S_FRAME = 2;
export const S_CURSOR = 3;
export const S_PONG = 4;
/** UTF-8 JSON: WebRTC signaling (`config`, `answer`). */
export const S_SIGNAL = 5;
/** Data-channel piece of a FRAME: flags u8, part u16, parts u16, pad u16, seq u32. */
export const S_CHUNK = 6;
export const CHUNK_HEADER_LEN = 12;

export const C_PING = 1;
/** Frames handled: n_lost u8, pad u16, seq u32, hold_us u32, n_lost x lost seq u32. */
export const C_ACK = 2;
export const C_KEYFRAME = 3;
export const C_STATS = 4;
/** UTF-8 JSON: WebRTC signaling (`offer`, `candidate`). */
export const C_SIGNAL = 5;
/** The last HELLO was applied. */
export const C_HELLO_ACK = 6;
/** visible u8: the tab is shown; a hidden tab gets no frames. */
export const C_VISIBLE = 7;
/** UTF-8 text for the server's event log. */
export const C_EVENT = 8;
export const C_MOVE = 10;
export const C_BUTTON = 11;
export const C_WHEEL = 12;
export const C_KEY = 13;

export const ENC_RAW = 0;
export const ENC_LZ4 = 1;
export const ENC_ZSTD = 2;

export const FLAG_VIDEO = 1;
export const FLAG_KEY = 2;

export const FRAME_HEADER_LEN = 48;
/** Screen move entry after the FRAME header: src_x, src_y, dst_x, dst_y, w, h (u16 each). */
export const COPY_LEN = 12;
export const RECT_HEADER_LEN = 16;

/** Wall-clock milliseconds with sub-ms resolution. */
export const now = (): number => performance.timeOrigin + performance.now();
