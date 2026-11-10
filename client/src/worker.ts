// Stream worker: owns the stream WebSocket, the H.264 decoder and the WebGL
// renderer, so frame handling never waits on main-thread work.
//
// Frames are applied strictly in order and atomically: a frame carrying video
// waits for its decoded picture, and later frames wait behind it, so lossless
// rects, tile layer switches and the video picture always match.

import { lz4Decode } from './lz4';
import { BarcodeReader, barcodeAge } from './probe';
import {
  C_ACK, C_KEYFRAME, C_PING, ENC_LZ4, ENC_RAW, FLAG_KEY, FLAG_VIDEO, FRAME_HEADER_LEN, RECT_HEADER_LEN,
  S_CURSOR, S_FRAME, S_HELLO, S_PONG, now,
} from './proto';
import { Renderer } from './renderer';
import { Recorder, type Batch } from './stats';

export type ToWorker = { type: 'init'; canvas: OffscreenCanvas; url: string; probe: boolean };

export type FromWorker =
  | { type: 'hello'; width: number; height: number }
  | { type: 'cursor'; buf: ArrayBuffer }
  | { type: 'marker'; marker: number; at: number }
  | { type: 'stats'; batch: Batch; pipeline: string; pingRtt: number };

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
let ws: WebSocket;
let decoder: VideoDecoder;
let awaitingKey = true;
const queue: Frame[] = [];
const rec = new Recorder();

// Clock sync: offset = server ms - client ms, from the lowest-RTT ping of a sliding window.
let offset = 0;
const syncSamples: { rtt: number; offset: number }[] = [];
let lastTs = -1;
let lastMarker = -1;

function send(m: Uint8Array): void {
  if (ws.readyState === WebSocket.OPEN) ws.send(m);
}

function ping(): void {
  const m = new Uint8Array(16);
  m[0] = C_PING;
  new DataView(m.buffer).setFloat64(8, now(), true);
  send(m);
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
  send(new Uint8Array([C_KEYFRAME]));
  pump();
}

function onFrame(buf: ArrayBuffer, received: number): void {
  const v = new DataView(buf);
  const flags = v.getUint8(1);
  const n = v.getUint16(2, true);
  let off = FRAME_HEADER_LEN;
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
      send(new Uint8Array([C_KEYFRAME]));
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
  let off = FRAME_HEADER_LEN;
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

  const ack = new Uint8Array(8);
  ack[0] = C_ACK;
  new DataView(ack.buffer).setUint32(4, f.seq, true);
  send(ack);

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
  if (f.picture) rec.push('encode', v.getUint32(40, true) / 1000);
  rec.push('net', f.received + offset - f.sent);
  rec.push('client', drawn - f.received);
  rec.push('total', drawn + offset - f.oldest);
}

function connect(url: string): void {
  ws = new WebSocket(url);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => {
    syncSamples.length = 0;
    for (let i = 0; i < 20; i++) setTimeout(ping, i * 25);
  };
  ws.onmessage = (ev) => {
    const received = now();
    const buf = ev.data as ArrayBuffer;
    const v = new DataView(buf);
    switch (v.getUint8(0)) {
      case S_FRAME:
        onFrame(buf, received);
        break;
      case S_CURSOR:
        post({ type: 'cursor', buf }, [buf]);
        break;
      case S_HELLO: {
        const width = v.getUint32(4, true);
        const height = v.getUint32(8, true);
        const tile = v.getUint32(12, true);
        renderer.resize(width, height, tile);
        staging = new Uint8Array(width * tile * 4);
        queue.length = 0;
        resetDecoder();
        post({ type: 'hello', width, height });
        break;
      }
      case S_PONG: {
        const sentAt = v.getFloat64(8, true);
        const serverMs = Number(v.getBigUint64(16, true)) / 1000;
        syncSamples.push({ rtt: received - sentAt, offset: serverMs - (sentAt + received) / 2 });
        if (syncSamples.length > 40) syncSamples.shift();
        offset = syncSamples.reduce((a, b) => (b.rtt < a.rtt ? b : a)).offset;
        break;
      }
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
      createDecoder();
      connect(m.url);
      setInterval(ping, 1000);
      setInterval(() => {
        const pipeline = `queue ${queue.length}  decoder ${decoder.state} q${decoder.decodeQueueSize}${awaitingKey ? ' awaiting key' : ''}`;
        const pingRtt = Math.min(...syncSamples.map((s) => s.rtt));
        post({ type: 'stats', batch: rec.take(), pipeline, pingRtt });
      }, 250);
      break;
  }
};
