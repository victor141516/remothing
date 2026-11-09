// Stream worker: owns the stream WebSocket, the WebGL renderer and frame
// timing, so frame handling never waits on main-thread work.

import { BarcodeReader, barcodeAge } from './probe';
import {
  C_ACK, C_PING, ENC_RAW, FRAME_HEADER_LEN, RECT_HEADER_LEN, S_CURSOR, S_FRAME, S_HELLO, S_PONG, now,
} from './proto';
import { Renderer } from './renderer';
import { Stats } from './stats';

export type ToWorker =
  | { type: 'init'; canvas: OffscreenCanvas; url: string; probe: boolean }
  | { type: 'reset' }
  | { type: 'report' };

export type FromWorker =
  | { type: 'hello'; width: number; height: number }
  | { type: 'cursor'; buf: ArrayBuffer }
  | { type: 'marker'; marker: number; at: number }
  | { type: 'stats'; text: string }
  | { type: 'report'; report: Record<string, unknown> };

const post = (m: FromWorker, transfer: Transferable[] = []) => self.postMessage(m, { transfer });

let renderer: Renderer;
let barcode: BarcodeReader | null = null;
let ws: WebSocket;
const stats = new Stats();

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

function onFrame(buf: ArrayBuffer, received: number): void {
  const v = new DataView(buf);
  const n = v.getUint16(2, true);
  const seq = v.getUint32(4, true);
  const oldest = Number(v.getBigUint64(8, true)) / 1000;
  const sent = Number(v.getBigUint64(24, true)) / 1000;
  let off = FRAME_HEADER_LEN;
  for (let i = 0; i < n; i++) {
    const x = v.getUint16(off, true);
    const y = v.getUint16(off + 2, true);
    const w = v.getUint16(off + 4, true);
    const h = v.getUint16(off + 6, true);
    const enc = v.getUint8(off + 8);
    const len = v.getUint32(off + 12, true);
    off += RECT_HEADER_LEN;
    if (enc === ENC_RAW) renderer.upload(x, y, w, h, new Uint8Array(buf, off, len));
    off += len;
  }
  renderer.draw();
  // Probe mode reads back pixels, which waits for the GPU to execute the draw:
  // `drawn` then marks GPU completion rather than command submission.
  const code = barcode?.read();
  const drawn = now();

  const ack = new Uint8Array(8);
  ack[0] = C_ACK;
  new DataView(ack.buffer).setUint32(4, seq, true);
  send(ack);

  if (code) {
    if (code.ts !== lastTs) {
      lastTs = code.ts;
      stats.appToDraw.push(barcodeAge(code.ts, drawn + offset));
    }
    if (code.marker !== lastMarker) {
      lastMarker = code.marker;
      post({ type: 'marker', marker: code.marker, at: drawn });
    }
  }
  stats.frames++;
  stats.bytes += buf.byteLength;
  stats.server.push(sent - oldest);
  stats.net.push(received + offset - sent);
  stats.client.push(drawn - received);
  stats.total.push(drawn + offset - oldest);
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
        renderer.resize(width, height);
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
      connect(m.url);
      setInterval(ping, 1000);
      setInterval(() => post({ type: 'stats', text: stats.text() }), 500);
      break;
    case 'reset':
      stats.reset();
      break;
    case 'report':
      post({ type: 'report', report: { ...stats.report(), pingRtt: Math.min(...syncSamples.map((s) => s.rtt)) } });
      break;
  }
};
