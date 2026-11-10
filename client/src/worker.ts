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
  C_ACK, C_AUDIO, C_CLIPBOARD, C_DISPLAY, C_DISPLAYS, C_EVENT, C_HELLO_ACK, C_KEYFRAME, C_MIC_INFO, C_PING, C_SIGNAL,
  C_STATS, C_VISIBLE, CHUNK_HEADER_LEN, COPY_LEN, ENC_LZ4, ENC_RAW, ENC_ZSTD, FLAG_KEY, FLAG_VIDEO, FRAME_HEADER_LEN,
  RECT_HEADER_LEN, S_AUDIO, S_AUDIO_INFO, S_CLIPBOARD, S_CURSOR, S_DISPLAYS, S_FRAME, S_HELLO, S_MIC_INFO, S_PONG,
  S_SIGNAL, now,
} from './proto';
import { Renderer } from './renderer';
import { Recorder, type Batch } from './stats';

export type ToWorker =
  | { type: 'init'; canvas: OffscreenCanvas; url: string; probe: boolean; forceDc: boolean }
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
  /** Ask the server for the session's sound, or to stop sending it. */
  | { type: 'audio'; on: boolean }
  /** Text for the server's event log. */
  | { type: 'event'; text: string }
  /** About this device's microphone: a C_MIC_INFO as JSON, or a complete C_MIC message. */
  | { type: 'mic'; json: string }
  | { type: 'micdata'; buf: ArrayBuffer };

export type FromWorker =
  | { type: 'hello'; width: number; height: number }
  | { type: 'cursor'; buf: ArrayBuffer }
  | { type: 'marker'; marker: number; at: number }
  | { type: 'stats'; batch: Batch; pipeline: string; pingRtt: number }
  | { type: 'signal'; json: string }
  | { type: 'dcsend'; buf: ArrayBuffer }
  /** Frames (so input too) go over the data channel. */
  | { type: 'path'; webrtc: boolean }
  /** The remote clipboard's text. */
  | { type: 'clipboard'; text: string }
  /** Display list as JSON (S_DISPLAYS). */
  | { type: 'displays'; json: string }
  /** The sound stream, or why there is none (S_AUDIO_INFO), and its packets (S_AUDIO). */
  | { type: 'audioinfo'; json: string }
  | { type: 'audio'; buf: ArrayBuffer }
  /** How the server sees this client's microphone (S_MIC_INFO). */
  | { type: 'micinfo'; json: string };

const post = (m: FromWorker, transfer: Transferable[] = []) => self.postMessage(m, { transfer });

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
}

let renderer: Renderer;
/** Decompression target, one row of tiles. */
let staging = new Uint8Array(0);
let barcode: BarcodeReader | null = null;
const zstd = new ZSTDDecoder();
/** Opened once the zstd decoder is ready. */
let ws: WebSocket | undefined;
let decoder: VideoDecoder;
let awaitingKey = true;
const queue: Frame[] = [];
const rec = new Recorder();

// Clock sync: offset = server ms - client ms, from the lowest-RTT ping of a sliding window.
let offset = 0;
const syncSamples: { rtt: number; offset: number }[] = [];
let lastTs = -1;
let lastMarker = -1;

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
const early = new Map<number, { buf: ArrayBuffer; received: number }>();
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
/** The data channel was given up as dead (see `setPath`). */
let dcDead = false;
/** Send time of the oldest unanswered data-channel ping (0: none). */
let dcPingSince = 0;
/** Round trips of the WebSocket path (input socket) and the data channel. */
const wsRtts: number[] = [];
const dcRtts: number[] = [];
let dcBannedUntil = 0;
let dcBan = DC_BAN_MS;
/** `?transport=webrtc`: the data channel whenever it is up, regardless of round trips. */
let forceDc = false;
/** The tab is shown; the server sends nothing to a hidden one. */
let visible = true;
/** Late frames since the last report: count, and the worst one's times. */
let late = { n: 0, total: 0, server: 0, net: 0, client: 0 };
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

/** ACKs go over the WebSocket, which always works, and over the data channel
 * when open: that one usually arrives first (shorter path, the same one the
 * frames took) and the server takes the first, so both stay cheap duplicates. */
function sendAckBoth(m: Uint8Array): void {
  send(m);
  sendDc(m);
}

function sendAck(seq: number, holdUs: number): void {
  const n = Math.min(lost.length, 255);
  const ack = new Uint8Array(12 + 4 * n);
  const v = new DataView(ack.buffer);
  ack[0] = C_ACK;
  ack[1] = n;
  v.setUint32(4, seq, true);
  v.setUint32(8, holdUs, true);
  for (let i = 0; i < n; i++) v.setUint32(12 + 4 * i, lost[i], true);
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
  if (want !== useDc) setPath(want);
}

/** `dead`: the data channel is given up as stalled or closed. The server
 * then repairs what is unacknowledged there at once, and frames missing
 * when the next one arrives over the WebSocket are not waited for. */
function setPath(dc: boolean, dead = false): void {
  useDc = dc;
  dcDead = dead;
  dcPingSince = 0;
  dcGraceUntil = now() + DC_GRACE_MS;
  sendText(C_SIGNAL, JSON.stringify({ type: 'path', webrtc: dc, dead }));
  post({ type: 'path', webrtc: dc });
}

/** Gives the data channel up as dead and keeps away from it for a while,
 * longer each time. */
function abandonDc(why: string): void {
  if (!useDc) return;
  sendText(C_EVENT, `data channel abandoned for ${dcBan / 1000} s: ${why}`);
  dcBannedUntil = now() + dcBan;
  dcBan = Math.min(2 * dcBan, DC_BAN_MAX_MS);
  setPath(false, true);
}

/** Runs every 20 ms: data-channel health, and frames lost with an abandoned one. */
function watchGap(): void {
  const t = now();
  if (useDc) {
    const limit = 4 * (dcRtts.length ? Math.min(...dcRtts) : 0);
    const gap = gapSince ? t - Math.max(gapSince, dcGraceUntil) : 0;
    const silent = dcPingSince ? t - dcPingSince : 0;
    if (gap > Math.max(DC_GAP_MS, limit)) abandonDc(`a frame missing for ${gap.toFixed(0)} ms`);
    else if (silent > Math.max(DC_STALL_MS, limit)) abandonDc(`no answer for ${silent.toFixed(0)} ms`);
  }
  if (!gapSince || (useDc && dcOpen) || t - gapSince < (dcDead ? 0 : LOSS_WAIT_MS)) return;
  // Everything missing before the next frame that arrived is gone.
  while (!early.has(expected) && before(expected, newest)) {
    const a = assembling.get(expected);
    assembling.delete(expected);
    lost.push(expected);
    lostTotal++;
    // Its video is gone (unknown without any chunk: maybe), so later P-frames
    // reference a missing picture: wait for the keyframe the server now sends.
    if (!a || a.video) awaitingKey = true;
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
  onSequenced(frame, received);
}

function onSequenced(buf: ArrayBuffer, received: number): void {
  const seq = new DataView(buf).getUint32(4, true);
  // Before the current display, or already given up on (late frame).
  if (expected < 0 || before(seq, expected)) return;
  if (newest < 0 || before(newest, seq)) newest = seq;
  early.set(seq, { buf, received });
  release();
}

/** Applies frames in sequence; notes when the next one is missing. */
function release(): void {
  for (let f = early.get(expected); f; f = early.get(expected)) {
    early.delete(expected);
    expected = (expected + 1) >>> 0;
    onFrame(f.buf, f.received);
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

/** Pings both paths (round trips for the path choice, clock offset). */
function ping(): void {
  send(pingMessage());
  pingDc();
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

function createDecoder(): void {
  decoder = new VideoDecoder({
    output: (picture) => {
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
      console.warn('decoder error, requesting keyframe', e);
      sendText(C_EVENT, `decoder error: ${e.message}`);
      resetDecoder();
    },
  });
  decoder.configure({ codec: 'avc1.640033', optimizeForLatency: true, hardwareAcceleration: 'prefer-hardware' });
  awaitingKey = true;
}

function resetDecoder(): void {
  if (decoder.state !== 'closed') decoder.close();
  // Frames waiting for pictures that will never come are shown without video.
  for (const f of queue) f.hasVideo = !!f.picture;
  createDecoder();
  requestKey();
  pump();
}

function onFrame(buf: ArrayBuffer, received: number): void {
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
  };
  if (flags & FLAG_VIDEO) {
    const sw = v.getUint16(off, true);
    const videoAt = off + 2 + sw * 2;
    const len = v.getUint32(videoAt, true);
    const key = (flags & FLAG_KEY) !== 0;
    if (key) awaitingKey = false;
    if (!awaitingKey) {
      f.hasVideo = true;
      decoder.decode(new EncodedVideoChunk({
        type: key ? 'key' : 'delta',
        timestamp: f.seq,
        data: new Uint8Array(buf, videoAt + 4, len),
      }));
    } else {
      requestKey();
    }
  }
  queue.push(f);
  pump();
}

function pump(): void {
  while (queue.length && (!queue[0].hasVideo || queue[0].picture)) apply(queue.shift()!);
}

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
  renderer.draw();
  // Probe mode reads back pixels, which waits for the GPU to execute the draw:
  // `drawn` then marks GPU completion rather than command submission.
  const code = barcode?.read();
  const drawn = now();

  // The server leaves this frame's time here out of its network model.
  sendAck(f.seq, Math.max(0, Math.round((now() - f.received) * 1000)));

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
      late = { n: late.n, total, server: f.sent - f.oldest, net: f.received + offset - f.sent, client: drawn - f.received };
    }
  }
}

function reportLate(): void {
  if (!late.n) return;
  const ms = (x: number) => x.toFixed(0);
  const frames = late.n === 1 ? '1 frame' : `${late.n} frames`;
  sendText(C_EVENT, `${frames} late, worst ${ms(late.total)} ms (server ${ms(late.server)}, network ${ms(late.net)}, client ${ms(late.client)})`);
  late = { n: 0, total: 0, server: 0, net: 0, client: 0 };
}

function connect(url: string): void {
  ws = new WebSocket(url);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => {
    syncSamples.length = 0;
    for (let i = 0; i < 20; i++) setTimeout(ping, i * 25);
    if (!visible) sendVisible();
    if (audioOn) sendAudio();
  };
  ws.onmessage = (ev) => {
    const received = now();
    const buf = ev.data as ArrayBuffer;
    const v = new DataView(buf);
    switch (v.getUint8(0)) {
      case S_FRAME:
        onSequenced(buf, received);
        break;
      case S_SIGNAL:
        post({ type: 'signal', json: new TextDecoder().decode(new Uint8Array(buf, 1)) });
        break;
      case S_CURSOR:
        post({ type: 'cursor', buf }, [buf]);
        break;
      case S_CLIPBOARD:
        post({ type: 'clipboard', text: new TextDecoder().decode(new Uint8Array(buf, 1)) });
        break;
      case S_DISPLAYS:
        post({ type: 'displays', json: new TextDecoder().decode(new Uint8Array(buf, 1)) });
        break;
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
        const width = v.getUint32(4, true);
        const height = v.getUint32(8, true);
        const tile = v.getUint32(12, true);
        renderer.resize(width, height, tile);
        staging = new Uint8Array(width * tile * 4);
        queue.length = 0;
        // Frames of the previous display (late, on any path) are dropped.
        expected = v.getUint32(16, true);
        for (const s of [...early.keys()]) if (before(s, expected)) early.delete(s);
        for (const s of [...assembling.keys()]) if (before(s, expected)) assembling.delete(s);
        newest = -1;
        gapSince = 0;
        lost = [];
        lateFrom = received + LATE_SKIP_MS;
        resetDecoder();
        post({ type: 'hello', width, height });
        // The server sends this display's frames once it knows we are ready.
        send(new Uint8Array([C_HELLO_ACK]));
        break;
      }
      case S_PONG:
        onPong(v, received);
        break;
    }
  };
  ws.onclose = () => setTimeout(() => connect(url), 1000);
}

self.onmessage = (ev: MessageEvent<ToWorker>) => {
  const m = ev.data;
  switch (m.type) {
    case 'init':
      renderer = new Renderer(m.canvas);
      if (m.probe) barcode = new BarcodeReader(renderer);
      forceDc = m.forceDc;
      createDecoder();
      // WASM zstd (~1.5 GB/s; pure-JS decoders run ~60x slower and stall
      // this worker, delaying video output and ACKs). Ready before any frame.
      void zstd.init().then(() => connect(m.url));
      setInterval(() => {
        ping();
        choosePath();
      }, 1000);
      setInterval(watchGap, 20);
      setInterval(() => {
        if (useDc) pingDc();
      }, DC_PING_MS);
      setInterval(reportLate, LATE_REPORT_MS);
      setInterval(() => {
        const rtts = dcOpen ? `  rtt ws ${Math.min(...wsRtts).toFixed(0)} dc ${Math.min(...dcRtts).toFixed(0)} ms` : '';
        const path = useDc ? 'webrtc' : dcOpen ? 'websocket (webrtc standby)' : 'websocket';
        const pipeline = `${path}${rtts}  lost ${lostTotal}  queue ${queue.length}  decoder ${decoder.state} q${decoder.decodeQueueSize}${awaitingKey ? ' awaiting key' : ''}`;
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
    case 'micdata':
      send(new Uint8Array(m.buf));
      break;
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
        setPath(false, true);
      }
      break;
  }
};
