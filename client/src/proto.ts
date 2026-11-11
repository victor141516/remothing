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
/** UTF-8 text now on the remote clipboard. */
export const S_CLIPBOARD = 7;
/** UTF-8 JSON: `{displays: [...], current}`. */
export const S_DISPLAYS = 8;
/** UTF-8 JSON: the sound stream (`codec`, `rate`, `channels`, `description`) or why there is none. */
export const S_AUDIO_INFO = 9;
/** One encoded sound packet. */
export const S_AUDIO = 10;
/** UTF-8 JSON: this client's view of the session's microphone (`state`, see mic.ts). */
export const S_MIC_INFO = 11;

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
/** UTF-8 text for the remote clipboard. */
export const C_CLIPBOARD = 9;
export const C_MOVE = 10;
export const C_BUTTON = 11;
export const C_WHEEL = 12;
/** down u8, evdev code u16: held on the remote side until released. */
export const C_KEY = 13;
/** Asks for the display list. */
export const C_DISPLAYS = 14;
/** UTF-8 display id to stream. */
export const C_DISPLAY = 15;
/** UTF-8 text to type as characters (`\n` is Enter). */
export const C_TEXT = 16;
/** on u8: send the session's sound, or stop. */
export const C_AUDIO = 17;
/** UTF-8 JSON: asks for the session's microphone, releases it, or answers its questions. */
export const C_MIC_INFO = 18;
/** One packet of this device's microphone. */
export const C_MIC = 19;

export const ENC_RAW = 0;
export const ENC_LZ4 = 1;
export const ENC_ZSTD = 2;

export const FLAG_VIDEO = 1;
export const FLAG_KEY = 2;
/** The screen is in motion mode (what changes is shown from video). */
export const FLAG_MOTION = 4;

/** One piece of a FRAME as a WebTransport datagram (see proto.rs S_PIECE). */
export const S_PIECE = 12;
export const PIECE_HEADER_LEN = 16;
export const PIECE_PAYLOAD = 1100;
/** Pieces of a frame that did not arrive: pad u8, count u16, seq u32, count x index u16 (0: all). */
export const C_NACK = 20;
/** pad u8, width u16, height u16 (picture in the window, device pixels), refresh Hz u16. */
export const C_VIEW = 21;

export const FRAME_HEADER_LEN = 48;
/** Screen move entry after the FRAME header: src_x, src_y, dst_x, dst_y, w, h (u16 each). */
export const COPY_LEN = 12;
export const RECT_HEADER_LEN = 16;

/** Wall-clock milliseconds with sub-ms resolution. */
export const now = (): number => performance.timeOrigin + performance.now();
