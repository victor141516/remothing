// Stream worker: owns the stream WebSocket, the H.264 decoder and the WebGL
// renderer, so frame handling never waits on main-thread work.
//
// Frames arrive over the WebSocket or, when the worker picks the WebRTC path
// (shorter round trip), in chunks over the data channel's frames channel
// (relayed by the main thread). That channel is reliable but unordered: a
// sequencer reassembles chunks and restores order. A frame missing for long
// there means SCTP is recovering a loss: the channel is abandoned, the
// frames left on it are given up, the ACK reports them and the server
// re-sends what they touched.
//
// Frames are applied strictly in order and atomically: a frame carrying video
// waits for its decoded picture, and later frames wait behind it, so lossless
// rects, tile layer switches and the video picture always match.

import { ZSTDDecoder } from 'zstddec';
import { lz4Decode } from './lz4';
import { BarcodeReader, barcodeAge } from './probe';
import {
  BT601, BT709, C_ACK, C_AUDIO, C_BT709, C_CAPTURE, C_CLIPBOARD, C_CLIPBOARD_IMAGE, C_DISPLAY, C_DISPLAYS, C_EVENT, C_HELLO_ACK, C_KEYFRAME,
  C_MIC_INFO, C_PING, C_SIGNAL, CLIPBOARD_PASTE, CLOSE_UNAUTHORIZED, CLIPBOARD_CHANGED, CURSOR_FOLLOW,
  C_PASTE, C_STATS, C_TRACE, C_VIEW, C_VIRTUAL, C_VISIBLE, CHUNK_HEADER_LEN, CODECS, COPY_LEN, ENC_LZ4, ENC_RAW, ENC_ZSTD, FLAG_BT601,
  FLAG_CODEC_MASK, FLAG_CODEC_SHIFT, FLAG_KEY, FLAG_MOTION, FLAG_RECOVERY, FLAG_VIDEO, FRAME_HEADER_LEN,
  RECT_HEADER_LEN, S_AUDIO, S_AUDIO_INFO, S_CLIPBOARD, S_CURSOR, S_CURSOR_POS, S_DISPLAYS, S_FRAME, S_HELLO, S_MIC_INFO, S_PONG,
  MAX_IMAGE, S_CLIPBOARD_FILES, S_CLIPBOARD_IMAGE, S_PASTED, S_SIGNAL, VIEW_HDR, now,
} from './proto';
import { Renderer } from './renderer';
import { WtPath, type WtOffer } from './wt';
import { Recorder, refreshRate, type Batch } from './stats';

export type ToWorker =
  /** `hdr`: the page started on an HDR screen (see `Renderer`). */
  | { type: 'init'; canvas: OffscreenCanvas; url: string; display: string; probe: boolean; forceDc: boolean; wt: boolean; codec: string | null; hdr: boolean; pace: boolean }
  | { type: 'report'; json: string }
  | { type: 'signal'; json: string }
  | { type: 'dcmsg'; buf: ArrayBuffer; received: number }
  | { type: 'dc'; open: boolean }
  /** The server's link model: the data channel carries less than the
   * WebSocket did (SCTP under packet loss), without stalling. */
  | { type: 'dcslow' }
  /** Round trip of a ping on the input WebSocket (unloaded: no frames on it). */
  | { type: 'wsrtt'; rtt: number }
  /** The tab is shown or hidden. */
  | { type: 'visible'; visible: boolean }
  /** Text for the remote clipboard. */
  | { type: 'clipboard'; text: string }
  /** Ask for the display list; stream display `id`. */
  | { type: 'displays' }
  | { type: 'display'; id: string }
  /** A virtual display request, JSON (C_VIRTUAL). */
  | { type: 'virtual'; json: string }
  /** This computer's clipboard for a Ctrl+V: text (C_PASTE) or a PNG picture (C_CLIPBOARD_IMAGE). */
  | { type: 'paste'; text: string }
  | { type: 'pasteimage'; png: ArrayBuffer }
  /** Ask the server for the session's sound, or to stop sending it. */
  | { type: 'audio'; on: boolean }
  /** Text for the server's event log. */
  | { type: 'event'; text: string }
  /** About this device's microphone: a C_MIC_INFO as JSON, or a complete C_MIC message. */
  | { type: 'mic'; json: string }
  | { type: 'micdata'; buf: ArrayBuffer }
  /** An input message, for the WebTransport path; `latest`: a
   * C_MOVE_LATEST, as a datagram. */
  | { type: 'input'; buf: ArrayBuffer; latest: boolean }
  /** How the picture is shown: its size in the window (device pixels), the
   * refresh rate, and whether the page can show HDR where it is now. */
  | { type: 'view'; width: number; height: number; hz: number; hdr: boolean }
  /** The mouse is trapped in the picture (or not any more). */
  | { type: 'capture'; on: boolean };

export type FromWorker =
  /** `hdr`: the picture is HDR (the page has to be switched to HDR). */
  | { type: 'hello'; width: number; height: number; hdr: boolean }
  | { type: 'cursor'; buf: ArrayBuffer }
  /** Where the remote cursor's hot spot is (S_CURSOR_POS); `follow`: the
   * mouse is trapped in another window, this one draws the cursor. */
  | { type: 'cursorpos'; x: number; y: number; follow: boolean }
  | { type: 'marker'; marker: number; at: number }
  | { type: 'stats'; batch: Batch; pipeline: string; pingRtt: number }
  | { type: 'signal'; json: string }
  | { type: 'dcsend'; buf: ArrayBuffer }
  /** Where frames (so input too) go. */
  | { type: 'path'; path: Path }
  /** The remote clipboard: text (S_CLIPBOARD) or a PNG picture (S_CLIPBOARD_IMAGE); `changed`: since this client connected, not what was there before. */
  | { type: 'clipboard'; text: string; changed: boolean }
  | { type: 'clipimage'; png: ArrayBuffer; changed: boolean }
  /** Display list as JSON (S_DISPLAYS). */
  | { type: 'displays'; json: string }
  /** The sound stream, or why there is none (S_AUDIO_INFO), and its packets (S_AUDIO). */
  | { type: 'audioinfo'; json: string }
  | { type: 'audio'; buf: ArrayBuffer }
  /** How the server sees this client's microphone (S_MIC_INFO). */
  | { type: 'micinfo'; json: string }
  /** The server closed the stream for good with this close code. */
  | { type: 'refused'; code: number }
  /** The server set the clipboard and pressed V (S_PASTED). */
  | { type: 'pasted' }
  /** The files on the remote clipboard, JSON (S_CLIPBOARD_FILES). */
  | { type: 'clipfiles'; json: string }
  /** The refresh rate this worker measured (it presents the picture). */
  | { type: 'refresh'; hz: number };

const post = (m: FromWorker, transfer: Transferable[] = []) => self.postMessage(m, { transfer });

/** Frames over the WebSocket, a WebRTC data channel, or WebTransport datagrams. */
export type Path = 'ws' | 'webrtc' | 'wt';

interface Signal {
  type: string;
}

interface Frame {
  buf: ArrayBuffer;
  seq: number;
  received: number;
  oldest: number;
  sent: number;
  /** Offset of the switch list (rects precede it). */
  switchesAt: number;
  hasVideo: boolean;
  picture: VideoFrame | null;
  decoded: number;
  /** Only complete after pieces were asked for again (WebTransport). */
  repaired: boolean;
}

let renderer: Renderer;
/** Decompression target, one row of tiles. */
let staging = new Uint8Array(0);
let barcode: BarcodeReader | null = null;
const zstd = new ZSTDDecoder();
/** Opened once the zstd decoder is ready. */
let ws: WebSocket | undefined;
let decoder: VideoDecoder;
/** Codec the decoder is configured for (a key of CODECS). */
let decoderCodec = 0;
/** The decoder is configured for BT.601 video (FLAG_BT601), from the frame of this sequence on. */
let decoderBt601 = false;
let bt601From = 0;
/** Why BT.601 video shows in the wrong colours in this browser (`bt601Wrong`); null: it does not. */
let bt601Broken: string | null = null;
/** C_BT709 was sent on this connection. */
let bt709Asked = false;
/** Codecs besides H.264 offered to the server: those decoded in hardware here. */
let offered: string[] = [];
let awaitingKey = true;
/** The first video frame lost while the decoder had all its references
 * (-1: waiting for a keyframe for another reason): a recovery frame for it
 * or an earlier frame ends the wait. */
let lostVideoFrom = -1;

/** Video frame `seq` never arrives: later ones reference a missing picture
 * until a keyframe, or a recovery frame (FLAG_RECOVERY), comes. */
function videoLost(seq: number): void {
  if (!awaitingKey) lostVideoFrom = seq;
  awaitingKey = true;
}
/** The hardware decoder got stuck: H.264 is decoded in software (`watchDecoder`). */
let decoderSoftware = false;
/** Waiting this long for a picture, with this many video frames behind it: stuck. */
const DECODER_STUCK_MS = 1500;
const DECODER_STUCK_FRAMES = 3;
/** Where the stream socket connects (`connect`), when it last heard anything,
 * and how long until the next reconnect. */
let streamUrl = '';
let wsHeard = 0;
/** Why the last stream socket was dropped (`watchSocket`), for the next one. */
let dropped = '';
let retryMs = 0;
const RETRY_MIN_MS = 250;
const RETRY_MAX_MS = 2000;
/** Pings go every second: this long without even a pong, the socket is dead. */
const WS_SILENT_MS = 4000;
const queue: Frame[] = [];
const rec = new Recorder();

// Clock sync: offset = server ms - client ms, from the lowest-RTT ping of a sliding window.
let offset = 0;
const syncSamples: { rtt: number; offset: number }[] = [];
let lastTs = -1;
let lastMarker = -1;
/** Pictures drawn since the last display refresh, and when the last was drawn. */
let drawsSinceRefresh = 0;
/** At most one draw per display refresh (off with `?pace=0`): a picture that
 * comes when one was already drawn in this refresh waits for the next, so
 * none is replaced unseen and the next refresh is not left empty. On a phone
 * at 120 Hz it raised the refreshes showing a new picture from 83 to 92 %
 * for 2.5 ms more capture→draw at the median. */
let paceDraws = false;
let lastDraw = -Infinity;
/** Gaps longer than this between draws are pauses, not the frame rate. */
const GAP_MAX_MS = 100;

/** The frame trace the server asked for (see C_TRACE): rows of drawn frames
 * and display refresh times, on the server's clock, since the last batch. */
interface TraceBatch {
  frames: number[][];
  refreshes: number[];
}
let trace: TraceBatch | null = null;
let traceTimer = 0;
const TRACE_EVERY_MS = 2000;

/** The display's refresh rate as this worker's animation frames see it:
 * they present the picture, and on a phone they ran at 120 Hz while the
 * page's own ran at 60. From the last REFRESH_STAMPS of them, checked every
 * REFRESH_CHECK_MS (phones change their rate); 0 until measured. */
let shownHz = 0;
const REFRESH_STAMPS = 61;
const REFRESH_CHECK_MS = 1000;
const refreshStamps: number[] = [];

function onRefresh(t: number): void {
  rec.refresh(drawsSinceRefresh);
  drawsSinceRefresh = 0;
  // `t` counts from the worker's start; `now()` from the epoch.
  trace?.refreshes.push(Math.round((performance.timeOrigin + t + offset) * 100) / 100);
  refreshStamps.push(t);
  if (refreshStamps.length > REFRESH_STAMPS) refreshStamps.shift();
  // A picture held back in the last refresh goes into this one.
  if (undrawn.length) present();
  self.requestAnimationFrame(onRefresh);
}

/** A new refresh rate goes to the server in the view, and to the page. */
function checkRefresh(): void {
  if (refreshStamps.length < REFRESH_STAMPS) return;
  const hz = refreshRate(refreshStamps);
  if (hz === shownHz) return;
  shownHz = hz;
  post({ type: 'refresh', hz });
  if (viewMessage) {
    new DataView(viewMessage.buffer).setUint16(6, hz, true);
    send(viewMessage);
  }
}

/** Frames left on a data channel abandoned by choice (the WebSocket got
 * shorter) are given up after this long: those still on their way arrive
 * within a round trip. */
const LOSS_WAIT_MS = 100;
/** On the data channel in use, a frame still missing this long (or 4 round
 * trips) after a later one arrived: a packet was lost and everything behind
 * it waits for SCTP, whose retransmission timeout is ~1 s and whose
 * congestion control collapses on random loss. Frames go back to the
 * WebSocket, whose TCP copes better. */
const DC_GAP_MS = 150;
/** A data-channel ping unanswered this long (or 4 round trips): the path died. */
const DC_STALL_MS = 300;
/** Pings on the data channel in use, so a dead path shows quickly. */
const DC_PING_MS = 250;
/** After moving to the data channel, frames still arriving over the
 * WebSocket leave gaps that are not losses. */
const DC_GRACE_MS = 500;
/** A stalled data channel is not used again for this long, doubling per stall. */
const DC_BAN_MS = 30_000;
const DC_BAN_MAX_MS = 300_000;
/** Recent pings per path whose minimum round trips are compared, and how
 * many each path needs before a choice. */
const RTT_WINDOW = 10;
const RTT_MIN_SAMPLES = 5;
/** An ACK not followed by another is re-sent after this long. */
const ACK_REPEAT_MS = 100;
/** A frame drawn this long after its change was captured is a stutter worth
 * a line in the server's event log; reported in batches. */
const LATE_MS = 200;
const LATE_REPORT_MS = 5000;
/** Not judged after a HELLO or a hidden tab: those frames carry old changes. */
const LATE_SKIP_MS = 1000;
/** Next frame to apply (-1 before the first HELLO). */
let expected = -1;
/** Newest frame seen (a chunk or all of it). */
let newest = -1;
/** Complete frames that arrived ahead of `expected`. */
const early = new Map<number, { buf: ArrayBuffer; received: number; repaired: boolean }>();
interface Assembly {
  pieces: Uint8Array[];
  got: number;
  size: number;
  video: boolean;
}
/** Frames arriving in data-channel chunks. */
const assembling = new Map<number, Assembly>();
/** Given-up frames for the next ACK. */
let lost: number[] = [];
let lostTotal = 0;
let ackTimer = 0;
/** Since when `expected` is missing while a later frame arrived (0: not). */
let gapSince = 0;
/** Frames channel open (main thread relays it). */
let dcOpen = false;
/** Frames come over the data channel (the server was asked to). */
let useDc = false;
/** Gaps before this are frames of the path used before (see DC_GRACE_MS). */
let dcGraceUntil = 0;
/** The UDP path was given up as dead (see `setPath`). */
let udpDead = false;
/** The last frame drawn said the screen is in motion mode. */
let motion = false;
/** Send time of the oldest unanswered data-channel ping (0: none). */
let dcPingSince = 0;
/** Round trips of the WebSocket path (input socket) and the data channel. */
const wsRtts: number[] = [];
const dcRtts: number[] = [];
let dcBannedUntil = 0;
let dcBan = DC_BAN_MS;
/** `?transport=webrtc`: the data channel whenever it is up, regardless of round trips. */
let forceDc = false;
/** WebTransport: tried at every connection, used for frames whenever up. */
const wt = new WtPath(
  {
    frame: (buf, received, repaired) => onSequenced(buf, received, repaired),
    giveUp,
    pong: (v, received) => {
      wtPingSince = 0;
      onPong(v, received);
    },
    down: wtDown,
  },
  () => expected,
);
let useWt = false;
/** `?transport=` leaves WebTransport out. */
let wtAllowed = true;
let wtOffer: WtOffer | null = null;
let wtConnecting = false;
let wtBannedUntil = 0;
let wtBan = DC_BAN_MS;
/** Send time of the oldest unanswered WebTransport ping (0: none). */
let wtPingSince = 0;
/** Frames the WebTransport path gave up, by seq: whether they may have carried video. */
const gaveUp = new Map<number, boolean>();
/** The tab is shown; the server sends nothing to a hidden one. */
let visible = true;
/** Late frames since the last report: count, and the worst one's times.
 * Server time splits into waiting until the frame was built (the link
 * window, pacing, a busy producer), encoder upload, encoding, and the rest
 * until it was sent (lossless compression, the send queue). */
const NO_LATE = { n: 0, total: 0, server: 0, wait: 0, upload: 0, encode: 0, out: 0, net: 0, client: 0 };
let late = { ...NO_LATE };
/** Frames drawn before this are not judged: the first ones of a display, or
 * after a hidden tab, carry old changes without being late. */
let lateFrom = 0;

/** `a` precedes `b` in wrapping 32-bit sequence order. */
const before = (a: number, b: number) => ((b - a) | 0) > 0;

function send(m: Uint8Array): void {
  if (ws?.readyState === WebSocket.OPEN) ws.send(m);
}

function sendDc(m: Uint8Array): void {
  if (!dcOpen) return;
  const buf = m.buffer.slice(m.byteOffset, m.byteOffset + m.byteLength) as ArrayBuffer;
  post({ type: 'dcsend', buf }, [buf]);
}

/** A tagged UTF-8 message (signaling, stats, events). */
function sendText(tag: number, text: string): void {
  const body = new TextEncoder().encode(text);
  const msg = new Uint8Array(1 + body.length);
  msg[0] = tag;
  msg.set(body, 1);
  send(msg);
}

function sendVisible(): void {
  send(new Uint8Array([C_VISIBLE, visible ? 1 : 0]));
}

/** The page wants the session's sound (asked again on every connection). */
let audioOn = false;
/** The last C_VIEW (sent again on every connection). */
let viewMessage: Uint8Array | null = null;
/** The mouse is trapped (told again on every connection). */
let captureOn = false;
/** The display to stream: the page's address at first, then the one streamed. */
let display = '';
/** The part of S_DISPLAYS the worker reads. */
interface ShownDisplay {
  current: string | null;
}
function sendAudio(): void {
  send(new Uint8Array([C_AUDIO, audioOn ? 1 : 0]));
}

function requestKey(): void {
  const m = new Uint8Array(8);
  m[0] = C_KEYFRAME;
  // The newest frame received: the server ignores requests that its last
  // keyframe already answers (one goes out per undecodable frame).
  new DataView(m.buffer).setUint32(4, (expected - 1) >>> 0, true);
  send(m);
}

/** ACKs go over the WebSocket, which always works, and over the UDP path
 * when one is open: that one usually arrives first (the path the frames
 * took) and the server takes the first, so the others stay cheap duplicates. */
function sendAckBoth(m: Uint8Array): void {
  send(m);
  sendDc(m);
  wt.send(m);
}

/** `repaired`: the frame was only complete after pieces were asked for
 * again, so its arrival time is no sample of the link's queue. `received`:
 * when the frame arrived, ms on this page's clock (0: none to time). */
function sendAck(seq: number, holdUs: number, repaired: boolean, received: number): void {
  const n = Math.min(lost.length, 255);
  const ack = new Uint8Array(20 + 4 * n);
  const v = new DataView(ack.buffer);
  ack[0] = C_ACK;
  ack[1] = n;
  ack[2] = repaired ? 1 : 0;
  v.setUint32(4, seq, true);
  v.setUint32(8, holdUs, true);
  v.setBigUint64(12, BigInt(Math.round(received * 1000)), true);
  for (let i = 0; i < n; i++) v.setUint32(20 + 4 * i, lost[i], true);
  lost = lost.slice(n);
  sendAckBoth(ack);
  // A lost last ACK would hold the server's window until its timeout.
  clearTimeout(ackTimer);
  ackTimer = setTimeout(() => sendAckBoth(ack), ACK_REPEAT_MS);
}

/** Frames go over the data channel only where it saves latency: a direct
 * path where the WebSocket is relayed (Funnel, a reverse proxy). Over the
 * same path TCP handles loss better than the data channel's SCTP. */
function choosePath(): void {
  // WebTransport is used whenever it is up; it has no round trip to beat.
  if (useWt) return;
  const t = now();
  let want = false;
  if (forceDc) {
    want = dcOpen && t >= dcBannedUntil;
  } else if (dcOpen && t >= dcBannedUntil && wsRtts.length >= RTT_MIN_SAMPLES && dcRtts.length >= RTT_MIN_SAMPLES) {
    const ws = Math.min(...wsRtts);
    const dc = Math.min(...dcRtts);
    // Clearly shorter only: jitter makes minimums of a few pings differ.
    // Once in use, frames queue ahead of its pongs: it stays while shorter at all.
    const margin = Math.max(8, 0.25 * ws);
    want = dc < ws - (useDc ? 0 : margin);
  }
  if (want !== useDc) setPath(want ? 'webrtc' : 'ws');
}

/** `dead`: the UDP path in use is given up as stalled or closed. The server
 * then repairs what is unacknowledged there at once, and frames missing
 * when the next one arrives over the WebSocket are not waited for. */
function setPath(p: Path, dead = false): void {
  useDc = p === 'webrtc';
  useWt = p === 'wt';
  udpDead = dead;
  dcPingSince = 0;
  wtPingSince = 0;
  dcGraceUntil = now() + DC_GRACE_MS;
  sendText(C_SIGNAL, JSON.stringify({ type: 'path', path: p, dead }));
  post({ type: 'path', path: p });
}

/** Gives the data channel up as dead and keeps away from it for a while,
 * longer each time. */
function abandonDc(why: string): void {
  if (!useDc) return;
  sendText(C_EVENT, `data channel abandoned for ${dcBan / 1000} s: ${why}`);
  dcBannedUntil = now() + dcBan;
  dcBan = Math.min(2 * dcBan, DC_BAN_MAX_MS);
  setPath('ws', true);
}

/** Opens WebTransport when the server offers it and it is not kept away. */
function tryWt(): void {
  if (!wtAllowed || !wtOffer || wt.up || wtConnecting || now() < wtBannedUntil) return;
  wtConnecting = true;
  wt.connect(wtOffer).then(
    (where) => {
      wtConnecting = false;
      wtBan = DC_BAN_MS;
      sendText(C_EVENT, `webtransport up via ${where}`);
      for (let i = 0; i < 5; i++) setTimeout(pingWt, i * 20);
      setPath('wt');
    },
    (e: unknown) => {
      wtConnecting = false;
      banWt(`webtransport failed: ${e instanceof Error ? e.message : String(e)}`);
    },
  );
}

/** Not tried again for a while, longer each time. */
function banWt(why: string): void {
  sendText(C_EVENT, `${why}; next try in ${wtBan / 1000} s`);
  wtBannedUntil = now() + wtBan;
  wtBan = Math.min(2 * wtBan, DC_BAN_MAX_MS);
}

/** The WebTransport session closed or stalled: frames go back to the other paths. */
function wtDown(why: string, key: string): void {
  // A session of a WebSocket connection that is gone ended with it; the
  // new connection's offer is tried at once.
  if (key === wtOffer?.key) banWt(`webtransport down: ${why}`);
  if (useWt) {
    setPath('ws', true);
    choosePath();
  }
  tryWt();
}

/** The WebTransport path will not deliver frame `seq`. */
function giveUp(seq: number, video: boolean): void {
  if (before(seq, expected)) return;
  gaveUp.set(seq, video);
  release();
  // Nothing drawn after it carries the loss to the server: say it now. The
  // frame is listed lost, so its own timing is no sample.
  if (lost.length) sendAck((expected - 1) >>> 0, 0, true, 0);
}

function pingWt(): void {
  if (!wt.up) return;
  if (!wtPingSince) wtPingSince = now();
  wt.send(pingMessage());
}

/** Runs every 20 ms: health of the UDP path in use, and frames lost with an abandoned one. */
function watchGap(): void {
  const t = now();
  if (useDc) {
    const limit = 4 * (dcRtts.length ? Math.min(...dcRtts) : 0);
    const gap = gapSince ? t - Math.max(gapSince, dcGraceUntil) : 0;
    const silent = dcPingSince ? t - dcPingSince : 0;
    if (gap > Math.max(DC_GAP_MS, limit)) abandonDc(`a frame missing for ${gap.toFixed(0)} ms`);
    else if (silent > Math.max(DC_STALL_MS, limit)) abandonDc(`no answer for ${silent.toFixed(0)} ms`);
  }
  if (useWt) {
    // Its gaps are the WebTransport path's own business (asked for again,
    // or given up); a path that stops answering is dead.
    // A queue on the path delays the answers too, without the path being
    // dead: the limit follows the last round trip.
    // Pings are datagrams: one lost (or its pong) is only made up by the
    // next one's answer, so the limit covers one ping interval more.
    const silent = wtPingSince ? t - wtPingSince : 0;
    if (silent > Math.max(DC_STALL_MS + DC_PING_MS, 4 * wt.rtt, 3 * wt.lastRtt)) wt.close(`no answer for ${silent.toFixed(0)} ms`);
    return;
  }
  if (!gapSince || (useDc && dcOpen) || t - gapSince < (udpDead ? 0 : LOSS_WAIT_MS)) return;
  // Everything missing before the next frame that arrived is gone.
  while (!early.has(expected) && before(expected, newest)) {
    const a = assembling.get(expected);
    assembling.delete(expected);
    lost.push(expected);
    lostTotal++;
    // Its video is gone (unknown without any chunk: maybe), so later P-frames
    // reference a missing picture: wait for the keyframe the server now sends.
    if (!a || a.video) videoLost(expected);
    expected = (expected + 1) >>> 0;
  }
  gapSince = 0;
  release();
}

/** Data-channel message: a frame chunk or a pong. */
function onDcMessage(buf: ArrayBuffer, received: number): void {
  const v = new DataView(buf);
  if (v.getUint8(0) === S_PONG) {
    dcPingSince = 0;
    dcRtts.push(onPong(v, received));
    if (dcRtts.length > RTT_WINDOW) dcRtts.shift();
    return;
  }
  const seq = v.getUint32(8, true);
  if (expected < 0 || before(seq, expected)) return;
  if (newest < 0 || before(newest, seq)) newest = seq;
  const part = v.getUint16(2, true);
  const parts = v.getUint16(4, true);
  let a = assembling.get(seq);
  if (!a) {
    a = { pieces: new Array(parts), got: 0, size: 0, video: (v.getUint8(1) & FLAG_VIDEO) !== 0 };
    assembling.set(seq, a);
  }
  if (a.pieces[part]) return;
  const piece = new Uint8Array(buf, CHUNK_HEADER_LEN);
  a.pieces[part] = piece;
  a.got++;
  a.size += piece.length;
  if (a.got < parts) {
    release();
    return;
  }
  assembling.delete(seq);
  let frame: ArrayBuffer;
  if (parts === 1) {
    frame = buf.slice(CHUNK_HEADER_LEN);
  } else {
    const out = new Uint8Array(a.size);
    let at = 0;
    for (const p of a.pieces) {
      out.set(p, at);
      at += p.length;
    }
    frame = out.buffer;
  }
  onSequenced(frame, received, false);
}

function onSequenced(buf: ArrayBuffer, received: number, repaired: boolean): void {
  const seq = new DataView(buf).getUint32(4, true);
  // Before the current display, or already given up on (late frame).
  if (expected < 0 || before(seq, expected)) return;
  if (newest < 0 || before(newest, seq)) newest = seq;
  early.set(seq, { buf, received, repaired });
  release();
}

/** Applies frames in sequence, skipping those the WebTransport path gave
 * up; notes when the next one is missing. */
function release(): void {
  for (;;) {
    const f = early.get(expected);
    if (f) {
      early.delete(expected);
      expected = (expected + 1) >>> 0;
      onFrame(f.buf, f.received, f.repaired);
      continue;
    }
    const video = gaveUp.get(expected);
    if (video === undefined) break;
    gaveUp.delete(expected);
    lost.push(expected);
    lostTotal++;
    // Later P-frames reference the missing picture: wait for the keyframe
    // the server sends once it hears of the loss.
    if (video) videoLost(expected);
    expected = (expected + 1) >>> 0;
  }
  if (!before(expected, newest)) gapSince = 0;
  else if (!gapSince) gapSince = now();
}

function pingMessage(): Uint8Array {
  const m = new Uint8Array(16);
  m[0] = C_PING;
  new DataView(m.buffer).setFloat64(8, now(), true);
  return m;
}

function pingDc(): void {
  if (!dcOpen) return;
  if (!dcPingSince) dcPingSince = now();
  sendDc(pingMessage());
}

/** Pings every path (round trips for the path choice and health, clock offset). */
function ping(): void {
  send(pingMessage());
  pingDc();
  pingWt();
}

/** A pong from either path: clock offset from the shortest round trips (the
 * most symmetric ones); returns the round trip. */
function onPong(v: DataView, received: number): number {
  const sentAt = v.getFloat64(8, true);
  const serverMs = Number(v.getBigUint64(16, true)) / 1000;
  syncSamples.push({ rtt: received - sentAt, offset: serverMs - (sentAt + received) / 2 });
  if (syncSamples.length > 40) syncSamples.shift();
  offset = syncSamples.reduce((a, b) => (b.rtt < a.rtt ? b : a)).offset;
  return received - sentAt;
}

/**
 * Codecs besides H.264 this browser decodes in hardware at 4K (a
 * software decoder would cost more than the bits saved). `only`
 * (`?codec=`): offer just that one.
 */
async function hardwareCodecs(only: string | null): Promise<string[]> {
  const found: string[] = [];
  for (const { name, config } of Object.values(CODECS)) {
    if (name === 'h264' || (only !== null && only !== name)) continue;
    const support = await VideoDecoder.isConfigSupported({
      codec: config, codedWidth: 3840, codedHeight: 2160, optimizeForLatency: true, hardwareAcceleration: 'prefer-hardware',
    }).catch(() => ({ supported: false }));
    if (support.supported === true) found.push(name);
  }
  return found;
}

/** The decoder of a codec said to be supported fails: it is not offered
 * again, and a new session lets the server choose without it. */
function dropCodec(why: string): void {
  const name = CODECS[decoderCodec].name;
  offered = offered.filter((c) => c !== name);
  sendText(C_EVENT, `decoder error with ${name}, no longer offered: ${why}`);
  decoderCodec = 0;
  resetDecoder();
  ws?.close();
}

function createDecoder(): void {
  decoder = new VideoDecoder({
    output: (picture) => {
      // A decoder that ignores the colour space it was given (Chromium's
      // HEVC) reports the stream's: those colours would be wrong.
      if (decoderBt601 && !before(picture.timestamp, bt601From) && picture.colorSpace.matrix !== 'smpte170m') {
        bt601Wrong(`the ${CODECS[decoderCodec].name} decoder ignores the colour space it is given`);
      }
      const f = queue.find((q) => q.hasVideo && !q.picture && q.seq === picture.timestamp);
      if (!f) {
        picture.close();
        return;
      }
      f.picture = picture;
      f.decoded = now();
      pump();
    },
    error: (e) => {
      if (CODECS[decoderCodec].name !== 'h264') {
        dropCodec(e.message);
        return;
      }
      console.warn('decoder error, requesting keyframe', e);
      // A browser without software H.264 fails at once: back to hardware.
      sendText(C_EVENT, `decoder error${decoderSoftware ? ' in software, back to hardware' : ''}: ${e.message}`);
      decoderSoftware = false;
      resetDecoder();
    },
  });
  configureDecoder();
  awaitingKey = true;
  lostVideoFrom = -1;
}

function configureDecoder(): void {
  decoder.configure({
    codec: CODECS[decoderCodec].config,
    optimizeForLatency: true,
    hardwareAcceleration: decoderSoftware && CODECS[decoderCodec].name === 'h264' ? 'prefer-software' : 'prefer-hardware',
    // Overrides what the stream says (Chromium takes BT.709 whatever it says).
    colorSpace: decoderBt601 ? BT601 : BT709,
  });
}

/** This browser shows BT.601 video in the wrong colours: the server is asked for BT.709. */
function bt601Wrong(why: string): void {
  if (bt601Broken !== null) return;
  bt601Broken = why;
  if (decoderBt601) askBt709();
}

function askBt709(): void {
  if (bt709Asked) return;
  bt709Asked = true;
  sendText(C_EVENT, `BT.601 video would show in the wrong colours here (${bt601Broken}): asking for BT.709`);
  send(new Uint8Array([C_BT709]));
}

function resetDecoder(): void {
  if (decoder.state !== 'closed') decoder.close();
  // Frames waiting for pictures that will never come are shown without video.
  for (const f of queue) f.hasVideo = !!f.picture;
  createDecoder();
  requestKey();
  pump();
}

function onFrame(buf: ArrayBuffer, received: number, repaired: boolean): void {
  const v = new DataView(buf);
  const flags = v.getUint8(1);
  const n = v.getUint16(2, true);
  let off = FRAME_HEADER_LEN + v.getUint16(44, true) * COPY_LEN;
  for (let i = 0; i < n; i++) off += RECT_HEADER_LEN + v.getUint32(off + 12, true);
  const f: Frame = {
    buf,
    seq: v.getUint32(4, true),
    received,
    oldest: Number(v.getBigUint64(8, true)) / 1000,
    sent: Number(v.getBigUint64(24, true)) / 1000,
    switchesAt: off,
    hasVideo: false,
    picture: null,
    decoded: 0,
    repaired,
  };
  if (flags & FLAG_VIDEO) {
    const sw = v.getUint16(off, true);
    let videoAt = off + 2 + sw * 2;
    const recovers = flags & FLAG_RECOVERY ? v.getUint32(videoAt, true) : -1;
    if (recovers >= 0) videoAt += 4;
    const len = v.getUint32(videoAt, true);
    const key = (flags & FLAG_KEY) !== 0;
    const codec = (flags >> FLAG_CODEC_SHIFT) & FLAG_CODEC_MASK;
    const bt601 = (flags & FLAG_BT601) !== 0;
    if (codec !== decoderCodec || bt601 !== decoderBt601) {
      // Another codec or matrix comes from a new encoder, which starts with a
      // keyframe; queued pictures of the old one still come out first.
      if (key) {
        decoderCodec = codec;
        decoderBt601 = bt601;
        configureDecoder();
        bt601From = f.seq;
      } else {
        awaitingKey = true;
        lostVideoFrom = -1;
      }
    }
    if (bt601 && bt601Broken !== null) askBt709();
    if (key && codec === decoderCodec) awaitingKey = false;
    // A recovery frame predicts only from frames before `recovers`: it ends
    // a wait for a loss no earlier than that.
    else if (awaitingKey && recovers >= 0 && lostVideoFrom >= 0 && codec === decoderCodec && !before(lostVideoFrom, recovers)) {
      awaitingKey = false;
    }
    if (!awaitingKey) {
      f.hasVideo = true;
      decoder.decode(new EncodedVideoChunk({
        type: key ? 'key' : 'delta',
        timestamp: f.seq,
        data: new Uint8Array(buf, videoAt + 4, len),
      }));
    } else if (lostVideoFrom < 0) {
      // A loss is answered by the server from the ACK that lists it (a
      // recovery frame, or a keyframe): asking here as well came first and
      // made it a keyframe.
      requestKey();
    }
  }
  queue.push(f);
  pump();
}

function pump(): void {
  while (queue.length && (!queue[0].hasVideo || queue[0].picture)) apply(queue.shift()!);
}

/** Frames applied to the textures but not drawn yet (held for the next refresh). */
const undrawn: Frame[] = [];

function apply(f: Frame): void {
  const v = new DataView(f.buf);
  const n = v.getUint16(2, true);
  const copies = v.getUint16(44, true);
  let off = FRAME_HEADER_LEN;
  for (let i = 0; i < copies; i++, off += COPY_LEN) {
    renderer.copy(
      v.getUint16(off, true),
      v.getUint16(off + 2, true),
      v.getUint16(off + 4, true),
      v.getUint16(off + 6, true),
      v.getUint16(off + 8, true),
      v.getUint16(off + 10, true),
    );
  }
  for (let i = 0; i < n; i++) {
    const x = v.getUint16(off, true);
    const y = v.getUint16(off + 2, true);
    const w = v.getUint16(off + 4, true);
    const h = v.getUint16(off + 6, true);
    const enc = v.getUint8(off + 8);
    const len = v.getUint32(off + 12, true);
    off += RECT_HEADER_LEN;
    const data = new Uint8Array(f.buf, off, len);
    if (enc === ENC_RAW) {
      renderer.upload(x, y, w, h, data);
    } else if (enc === ENC_LZ4) {
      const out = staging.subarray(0, w * h * 4);
      lz4Decode(data, out);
      renderer.upload(x, y, w, h, out);
    } else if (enc === ENC_ZSTD) {
      renderer.upload(x, y, w, h, zstd.decode(data, w * h * 4));
    }
    off += len;
  }
  const switches = v.getUint16(f.switchesAt, true);
  for (let i = 0; i < switches; i++) renderer.switchTile(v.getUint16(f.switchesAt + 2 + i * 2, true));
  if (switches) renderer.uploadMask();
  if (f.picture) {
    renderer.uploadVideo(f.picture);
    f.picture.close();
    rec.push('decode', f.decoded - f.received);
  }
  undrawn.push(f);
  // At most one picture per display refresh: a second one in the same
  // refresh would replace the first before it was shown.
  if (!paceDraws || drawsSinceRefresh === 0) present();
}

/** Draws what the frames applied since the last draw left on the textures. */
function present(): void {
  renderer.draw();
  // Probe mode reads back pixels, which waits for the GPU to execute the draw:
  // `drawn` then marks GPU completion rather than command submission.
  const code = barcode?.read();
  const drawn = now();
  drawsSinceRefresh++;
  // Only between pictures that keep coming (a still screen sends none).
  if (drawn - lastDraw < GAP_MAX_MS) rec.push('gap', drawn - lastDraw);
  lastDraw = drawn;
  if (code) {
    if (code.ts !== lastTs) {
      lastTs = code.ts;
      rec.push('appToDraw', barcodeAge(code.ts, drawn + offset));
    }
    if (code.marker !== lastMarker) {
      lastMarker = code.marker;
      post({ type: 'marker', marker: code.marker, at: drawn });
    }
  }
  for (const f of undrawn) drew(f, drawn);
  undrawn.length = 0;
}

/** Acknowledgement, stats and trace of a frame drawn at `drawn`. */
function drew(f: Frame, drawn: number): void {
  const v = new DataView(f.buf);
  // The server leaves this frame's time here out of its network model.
  sendAck(f.seq, Math.max(0, Math.round((now() - f.received) * 1000)), f.repaired, f.received);

  motion = (v.getUint8(1) & FLAG_MOTION) !== 0;
  rec.frame(f.buf.byteLength, !!f.picture);
  rec.push('server', f.sent - f.oldest);
  rec.push('serverWait', Number(v.getBigUint64(32, true)) / 1000 - f.oldest);
  if (f.picture) {
    rec.push('upload', v.getUint16(46, true) / 100);
    rec.push('encode', v.getUint32(40, true) / 1000);
  }
  rec.push('net', f.received + offset - f.sent);
  rec.push('client', drawn - f.received);
  const total = drawn + offset - f.oldest;
  rec.push('total', total);
  // Without clock sync yet, `total` is the difference between two clocks.
  if (total > LATE_MS && now() >= lateFrom && syncSamples.length >= 5) {
    late.n++;
    if (total > late.total) {
      const pulled = Number(v.getBigUint64(32, true)) / 1000;
      const upload = f.picture ? v.getUint16(46, true) / 100 : 0;
      const encode = f.picture ? v.getUint32(40, true) / 1000 : 0;
      late = {
        n: late.n,
        total,
        server: f.sent - f.oldest,
        wait: pulled - f.oldest,
        upload,
        encode,
        out: f.sent - pulled - upload - encode,
        net: f.received + offset - f.sent,
        client: drawn - f.received,
      };
    }
  }
  if (trace) {
    const ms = (x: number) => Math.round(x * 100) / 100;
    const video = !!f.picture;
    trace.frames.push([
      f.seq,
      f.buf.byteLength,
      v.getUint8(1),
      ms(f.oldest),
      ms(Number(v.getBigUint64(16, true)) / 1000),
      ms(Number(v.getBigUint64(32, true)) / 1000),
      video ? v.getUint16(46, true) / 100 : 0,
      video ? v.getUint32(40, true) / 1000 : 0,
      ms(f.sent),
      ms(f.received + offset),
      video ? ms(f.decoded + offset) : 0,
      ms(drawn + offset),
    ]);
  }
}

/** A batch of the frame trace to the server (C_TRACE); nothing while nothing is drawn. */
function sendTrace(): void {
  if (!trace) return;
  if (trace.frames.length) sendText(C_TRACE, JSON.stringify({ offset: Math.round(offset * 100) / 100, ...trace }));
  trace = { frames: [], refreshes: [] };
}

function reportLate(): void {
  if (!late.n) return;
  const ms = (x: number) => x.toFixed(0);
  const frames = late.n === 1 ? '1 frame' : `${late.n} frames`;
  const server = `server ${ms(late.server)}: wait ${ms(late.wait)}, upload ${ms(late.upload)}, encode ${ms(late.encode)}, out ${ms(late.out)}`;
  sendText(C_EVENT, `${frames} late, worst ${ms(late.total)} ms (${server}; network ${ms(late.net)}, client ${ms(late.client)})`);
  late = { ...NO_LATE };
}

function connect(url: string): void {
  streamUrl = url;
  const codecs = offered.length ? `&codecs=${offered.join(',')}` : '';
  ws = new WebSocket(`${url}${codecs}&display=${encodeURIComponent(display)}`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => {
    wsHeard = now();
    if (dropped) sendText(C_EVENT, dropped);
    dropped = '';
    // Each server session says whether it wants the frame trace.
    trace = null;
    // The server's choice is per connection.
    bt709Asked = false;
    syncSamples.length = 0;
    for (let i = 0; i < 20; i++) setTimeout(ping, i * 25);
    // Which display the server chose (the page's address shows it).
    send(new Uint8Array([C_DISPLAYS]));
    if (!visible) sendVisible();
    if (audioOn) sendAudio();
    if (viewMessage) send(viewMessage);
    if (captureOn) send(new Uint8Array([C_CAPTURE, 1]));
  };
  ws.onmessage = (ev) => {
    const received = now();
    wsHeard = received;
    const buf = ev.data as ArrayBuffer;
    const v = new DataView(buf);
    switch (v.getUint8(0)) {
      case S_FRAME:
        onSequenced(buf, received, false);
        break;
      case S_SIGNAL: {
        const json = new TextDecoder().decode(new Uint8Array(buf, 1));
        const signal = JSON.parse(json) as Signal;
        if (signal.type === 'trace') {
          trace ??= { frames: [], refreshes: [] };
          traceTimer ||= setInterval(sendTrace, TRACE_EVERY_MS);
        } else if (signal.type === 'wt') {
          // A new WebSocket connection is a new server session: its offer
          // is tried at once, whatever happened to the last one.
          const offer = JSON.parse(json) as WtOffer;
          if (offer.key !== wtOffer?.key) {
            wtBannedUntil = 0;
            wtBan = DC_BAN_MS;
          }
          wtOffer = offer;
          tryWt();
        } else if (signal.type === 'wtclosed') {
          wt.close('closed by the server');
        } else {
          post({ type: 'signal', json });
        }
        break;
      }
      case S_CURSOR:
        post({ type: 'cursor', buf }, [buf]);
        break;
      case S_CLIPBOARD_FILES:
        post({ type: 'clipfiles', json: new TextDecoder().decode(new Uint8Array(buf, 1)) });
        break;
      case S_PASTED:
        post({ type: 'pasted' });
        break;
      case S_CURSOR_POS:
        post({ type: 'cursorpos', x: v.getUint16(2, true), y: v.getUint16(4, true), follow: (v.getUint8(1) & CURSOR_FOLLOW) !== 0 });
        break;
      case S_CLIPBOARD:
        post({ type: 'clipboard', text: new TextDecoder().decode(new Uint8Array(buf, 2)), changed: (v.getUint8(1) & CLIPBOARD_CHANGED) !== 0 });
        break;
      case S_CLIPBOARD_IMAGE: {
        const png = buf.slice(2);
        post({ type: 'clipimage', png, changed: (v.getUint8(1) & CLIPBOARD_CHANGED) !== 0 }, [png]);
        break;
      }
      case S_DISPLAYS: {
        const json = new TextDecoder().decode(new Uint8Array(buf, 1));
        // A reconnect comes back to the display streamed now.
        display = (JSON.parse(json) as ShownDisplay).current ?? display;
        post({ type: 'displays', json });
        break;
      }
      case S_AUDIO_INFO:
        post({ type: 'audioinfo', json: new TextDecoder().decode(new Uint8Array(buf, 1)) });
        break;
      case S_AUDIO:
        post({ type: 'audio', buf }, [buf]);
        break;
      case S_MIC_INFO:
        post({ type: 'micinfo', json: new TextDecoder().decode(new Uint8Array(buf, 1)) });
        break;
      case S_HELLO: {
        // The session works: the next drop reconnects at once again.
        retryMs = 0;
        const width = v.getUint32(4, true);
        const height = v.getUint32(8, true);
        const tile = v.getUint32(12, true);
        renderer.resize(width, height, tile, v.getFloat32(20, true));
        staging = new Uint8Array(width * tile * 4);
        queue.length = 0;
        undrawn.length = 0;
        // Frames of the previous display (late, on any path) are dropped.
        expected = v.getUint32(16, true);
        for (const s of [...early.keys()]) if (before(s, expected)) early.delete(s);
        for (const s of [...assembling.keys()]) if (before(s, expected)) assembling.delete(s);
        for (const s of [...gaveUp.keys()]) if (before(s, expected)) gaveUp.delete(s);
        newest = -1;
        gapSince = 0;
        lost = [];
        lateFrom = received + LATE_SKIP_MS;
        resetDecoder();
        post({ type: 'hello', width, height, hdr: renderer.float });
        // The server sends this display's frames once it knows we are ready.
        send(new Uint8Array([C_HELLO_ACK]));
        break;
      }
      case S_PONG:
        onPong(v, received);
        break;
    }
  };
  // A refused token stays refused: retrying would only repeat the refusal.
  ws.onclose = (ev) => {
    if (ev.code === CLOSE_UNAUTHORIZED) post({ type: 'refused', code: ev.code });
    else reconnect();
  };
}

/** At once the first time (a server restart, a dropped connection: the
 * picture comes back within a round trip), then backing off while the
 * server stays away. */
function reconnect(): void {
  setTimeout(() => connect(streamUrl), retryMs);
  retryMs = Math.min(Math.max(2 * retryMs, RETRY_MIN_MS), RETRY_MAX_MS);
}

/** A socket that hears nothing, not even the pongs of its pings, is dead
 * without knowing it (a NAT or proxy forgot it, the computer slept): TCP
 * would take minutes to say so. Dropped for a new one. */
function watchSocket(): void {
  if (ws?.readyState !== WebSocket.OPEN || now() - wsHeard < WS_SILENT_MS) return;
  // Told over the new socket: the old one reaches nobody.
  dropped = `stream socket silent for ${Math.round(now() - wsHeard)} ms: reconnected`;
  const dead = ws;
  dead.onclose = dead.onmessage = null;
  dead.close();
  reconnect();
}

/** A decoder that takes frames and gives no pictures is stuck (hardware
 * decoders after a GPU reset or sleep have done so without an error). H.264
 * goes on in software; other codecs are only offered in hardware and are
 * dropped as after a decoder error. */
function watchDecoder(): void {
  const waiting = queue.findIndex((f) => f.hasVideo && !f.picture);
  if (waiting < 0 || now() - queue[waiting].received < DECODER_STUCK_MS) return;
  if (queue.slice(waiting + 1).filter((f) => f.hasVideo).length < DECODER_STUCK_FRAMES) return;
  const why = `no picture for ${Math.round(now() - queue[waiting].received)} ms`;
  if (CODECS[decoderCodec].name !== 'h264') {
    dropCodec(why);
    return;
  }
  sendText(C_EVENT, `decoder stuck (${why}): ${decoderSoftware ? 'reset' : 'decoding in software from now on'}`);
  decoderSoftware = true;
  resetDecoder();
}

self.onmessage = (ev: MessageEvent<ToWorker>) => {
  const m = ev.data;
  switch (m.type) {
    case 'init':
      renderer = new Renderer(m.canvas, m.hdr);
      if (!renderer.showsBt601()) bt601Wrong('WebGL ignores the colour space of video frames');
      // Display refreshes as this worker sees them (Chromium has animation
      // frames in workers; elsewhere the rates stay at 0).
      if (typeof self.requestAnimationFrame === 'function') {
        self.requestAnimationFrame(onRefresh);
        setInterval(checkRefresh, REFRESH_CHECK_MS);
      }
      if (m.probe) barcode = new BarcodeReader(renderer);
      forceDc = m.forceDc;
      paceDraws = m.pace && typeof self.requestAnimationFrame === 'function';
      wtAllowed = m.wt;
      createDecoder();
      // WASM zstd (~1.5 GB/s; pure-JS decoders run ~60x slower and stall
      // this worker, delaying video output and ACKs). Ready before any frame.
      void Promise.all([zstd.init(), hardwareCodecs(m.codec)]).then(([, codecs]) => {
        offered = codecs;
        display = m.display;
        connect(m.url);
      });
      setInterval(() => {
        ping();
        choosePath();
        tryWt();
        watchSocket();
        watchDecoder();
      }, 1000);
      setInterval(watchGap, 20);
      // Missing pieces are asked for within a few ms.
      setInterval(() => wt.tick(), 5);
      setInterval(() => {
        if (useDc) pingDc();
        if (useWt) pingWt();
      }, DC_PING_MS);
      setInterval(reportLate, LATE_REPORT_MS);
      setInterval(() => {
        const rtts = dcOpen ? `  rtt ws ${Math.min(...wsRtts).toFixed(0)} dc ${Math.min(...dcRtts).toFixed(0)} ms` : '';
        const path = useWt
          ? `webtransport rtt ${wt.rtt.toFixed(0)} ms`
          : useDc
            ? 'webrtc'
            : dcOpen
              ? 'websocket (webrtc standby)'
              : 'websocket';
        const picture = renderer.float ? '  HDR' : renderer.hdr ? '  HDR picture clipped to SDR' : '';
        const mode = `${motion ? 'motion mode' : 'tile mode'}, video ${renderer.videoTiles}/${renderer.tiles} tiles${picture}`;
        const software = decoderSoftware ? ' software' : '';
        const pipeline = `${path}${rtts}  ${mode}  lost ${lostTotal}  queue ${queue.length}  decoder ${CODECS[decoderCodec].name}${software} ${decoder.state} q${decoder.decodeQueueSize}${awaitingKey ? ' awaiting key' : ''}`;
        const pingRtt = Math.min(...syncSamples.map((s) => s.rtt));
        post({ type: 'stats', batch: rec.take(), pipeline, pingRtt });
      }, 250);
      break;
    case 'report':
      sendText(C_STATS, m.json);
      break;
    case 'signal':
      sendText(C_SIGNAL, m.json);
      break;
    case 'dcslow':
      abandonDc('slower than the websocket was');
      break;
    case 'visible':
      visible = m.visible;
      // What changed meanwhile arrives as one frame: old, not late.
      lateFrom = now() + LATE_SKIP_MS;
      sendVisible();
      break;
    case 'clipboard':
      sendText(C_CLIPBOARD, m.text);
      break;
    case 'displays':
      send(new Uint8Array([C_DISPLAYS]));
      break;
    case 'display':
      sendText(C_DISPLAY, m.id);
      break;
    case 'virtual':
      sendText(C_VIRTUAL, m.json);
      break;
    case 'paste':
      sendText(C_PASTE, m.text);
      break;
    case 'pasteimage': {
      // A picture over the limit is not sent: the remote clipboard is pasted as it is.
      if (m.png.byteLength > MAX_IMAGE) {
        sendText(C_EVENT, `clipboard picture of ${m.png.byteLength} bytes not pasted (more than ${MAX_IMAGE})`);
        sendText(C_PASTE, '');
        break;
      }
      const msg = new Uint8Array(2 + m.png.byteLength);
      msg[0] = C_CLIPBOARD_IMAGE;
      msg[1] = CLIPBOARD_PASTE;
      msg.set(new Uint8Array(m.png), 2);
      send(msg);
      break;
    }
    case 'audio':
      audioOn = m.on;
      sendAudio();
      break;
    case 'event':
      sendText(C_EVENT, m.text);
      break;
    case 'mic':
      sendText(C_MIC_INFO, m.json);
      break;
    case 'view': {
      const v = new Uint8Array(8);
      const d = new DataView(v.buffer);
      v[0] = C_VIEW;
      v[1] = m.hdr && renderer.hdrCapable ? VIEW_HDR : 0;
      d.setUint16(2, Math.min(m.width, 0xffff), true);
      d.setUint16(4, Math.min(m.height, 0xffff), true);
      d.setUint16(6, shownHz || m.hz, true);
      viewMessage = v;
      send(v);
      break;
    }
    case 'capture':
      if (m.on !== captureOn) {
        captureOn = m.on;
        send(new Uint8Array([C_CAPTURE, m.on ? 1 : 0]));
      }
      break;
    case 'micdata':
      send(new Uint8Array(m.buf));
      break;
    case 'input': {
      // The input stream may not be open yet: the stream socket takes input too.
      const b = new Uint8Array(m.buf);
      if (!(m.latest ? wt.send(b) : wt.sendInput(b))) send(b);
      break;
    }
    case 'wsrtt':
      wsRtts.push(m.rtt);
      if (wsRtts.length > RTT_WINDOW) wsRtts.shift();
      break;
    case 'dcmsg':
      onDcMessage(m.buf, m.received);
      break;
    case 'dc':
      dcOpen = m.open;
      dcRtts.length = 0;
      dcPingSince = 0;
      if (dcOpen) {
        // A quick first measurement, then the regular pings decide.
        for (let i = 1; i <= 5; i++) setTimeout(pingDc, i * 50);
        setTimeout(choosePath, 400);
      } else if (useDc) {
        setPath('ws', true);
      }
      break;
  }
};
