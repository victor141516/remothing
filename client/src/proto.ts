// Mirrors crates/core/src/proto.rs. Little-endian.

export const S_HELLO = 1;
export const S_FRAME = 2;
export const S_CURSOR = 3;
/** flags u8, x u16, y u16: the remote cursor's hot spot, while this client
 * captures the mouse, or with CURSOR_FOLLOW while another client does. */
export const S_CURSOR_POS = 13;
export const CURSOR_FOLLOW = 1;
/** The server set the clipboard of a C_PASTE and pressed V. */
export const S_PASTED = 14;
export const S_PONG = 4;
/** UTF-8 JSON: WebRTC signaling (`config`, `answer`). */
export const S_SIGNAL = 5;
/** Data-channel piece of a FRAME: flags u8, part u16, parts u16, pad u16, seq u32. */
export const S_CHUNK = 6;
export const CHUNK_HEADER_LEN = 12;
/** flags u8, UTF-8 text now on the remote clipboard (see proto.rs). */
export const S_CLIPBOARD = 7;
/** The remote clipboard changed since the client connected: copy it to this computer's too. */
export const CLIPBOARD_CHANGED = 1;
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
/** pad u8, dx i16, dy i16 in wheel units (positive = right / down). */
export const C_WHEEL = 12;
/** Wheel units per notch of a mouse wheel (see proto.rs C_WHEEL). */
export const WHEEL_NOTCH = 120;
/** down u8, evdev code u16: held on the remote side until released. */
export const C_KEY = 13;
/** Asks for the display list. */
export const C_DISPLAYS = 14;
/** UTF-8 display id this client streams from now on. */
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
/** Bits 3-4 of FRAME flags: the video codec, a key of CODECS (see proto.rs FLAG_CODEC_SHIFT). */
export const FLAG_CODEC_SHIFT = 3;
export const FLAG_CODEC_MASK = 3;
/** Video codecs by their number in FRAME flags: the server's name and the WebCodecs string (levels for 4K at 120 Hz; H.264's 5.1 is what it supports). */
export const CODECS: Record<number, { name: string; config: string }> = {
  0: { name: 'h264', config: 'avc1.640033' },
  1: { name: 'hevc', config: 'hev1.1.6.L156.B0' },
  2: { name: 'av1', config: 'av01.0.14M.08' },
};

/** One piece of a FRAME as a WebTransport datagram (see proto.rs S_PIECE). */
export const S_PIECE = 12;
export const PIECE_HEADER_LEN = 16;
export const PIECE_PAYLOAD = 1100;
/** Pieces of a frame that did not arrive: pad u8, count u16, seq u32, count x index u16 (0: all). */
export const C_NACK = 20;
/** flags u8, width u16, height u16 (picture in the window, device pixels), refresh Hz u16. */
export const C_VIEW = 21;
/** C_VIEW flag: this page can show an HDR picture where it is now. */
export const VIEW_HDR = 1;
/** flags u8, dx i16, dy i16: mouse counts, while the mouse is captured. */
export const C_MOVE_REL = 22;
/** C_MOVE_REL flag: the server injects the counts as a position (see proto.rs). */
export const MOVE_AS_POSITION = 1;
/** on u8: this client captured the mouse (it then gets S_CURSOR_POS). */
export const C_CAPTURE = 23;
/** UTF-8 JSON, a virtual display request (see proto.rs); answered with S_DISPLAYS. */
export const C_VIRTUAL = 24;
/** UTF-8 text: this computer's clipboard for a Ctrl+V; the server sets it and presses V (see proto.rs). */
export const C_PASTE = 25;
/** WebSocket close code for a token the server does not accept (see proto.rs). */
export const CLOSE_UNAUTHORIZED = 4401;

export const FRAME_HEADER_LEN = 48;
/** Screen move entry after the FRAME header: src_x, src_y, dst_x, dst_y, w, h (u16 each). */
export const COPY_LEN = 12;
export const RECT_HEADER_LEN = 16;

/** Wall-clock milliseconds with sub-ms resolution. */
export const now = (): number => performance.timeOrigin + performance.now();
