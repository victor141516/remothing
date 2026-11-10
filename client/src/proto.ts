// Mirrors crates/core/src/proto.rs. Little-endian.

export const S_HELLO = 1;
export const S_FRAME = 2;
export const S_CURSOR = 3;
export const S_PONG = 4;

export const C_PING = 1;
/** Frame drawn: pad x3, seq u32, hold_us u32 (receive-to-draw time). */
export const C_ACK = 2;
export const C_KEYFRAME = 3;
export const C_STATS = 4;
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
