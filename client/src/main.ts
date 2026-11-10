// Main thread: input capture (sent the moment the event fires, over the
// WebRTC input channel when frames use the data channel, else its own
// WebSocket), the peer connection, cursor, stats overlay. Frames are handled
// in worker.ts.

import { Input } from './input';
import { Peer } from './peer';
import { InputProber } from './probe';
import { C_PING, S_PONG, now } from './proto';
import { Stats } from './stats';
import type { FromWorker, ToWorker } from './worker';

declare global {
  interface Window {
    __bench?: { done: boolean; result?: Record<string, unknown> };
  }
}

const params = new URLSearchParams(location.search);
const benchSeconds = Number(params.get('bench') ?? 0);
const probeEnabled = benchSeconds > 0 || params.has('probe');
// Relative to the page, so the client also works behind a reverse proxy under a path prefix.
const wsBase = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}${new URL('.', location.href).pathname}`;
// Stream access token, given in the page URL (`?token=`).
const auth = `?token=${encodeURIComponent(params.get('token') ?? '')}`;

const canvas = document.getElementById('screen') as HTMLCanvasElement;
const statsEl = document.getElementById('stats') as HTMLDivElement;

let inputWs: WebSocket;
/** Pings the input socket: carrying no frames, it gives the WebSocket path's
 * unloaded round trip, compared by the worker with the data channel's. */
function pingInput(): void {
  if (inputWs.readyState !== WebSocket.OPEN) return;
  const m = new Uint8Array(16);
  m[0] = C_PING;
  new DataView(m.buffer).setFloat64(8, now(), true);
  inputWs.send(m);
}
function connectInput(): void {
  inputWs = new WebSocket(`${wsBase}input${auth}`);
  inputWs.binaryType = 'arraybuffer';
  inputWs.onopen = () => {
    for (let i = 0; i < 5; i++) setTimeout(pingInput, i * 50);
  };
  inputWs.onmessage = (e) => {
    const v = new DataView(e.data as ArrayBuffer);
    if (v.getUint8(0) === S_PONG) toWorker({ type: 'wsrtt', rtt: now() - v.getFloat64(8, true) });
  };
  inputWs.onclose = () => setTimeout(connectInput, 1000);
}
connectInput();
setInterval(pingInput, 1000);
/** Frames go over the data channel (the worker's choice): input follows. */
let dcPath = false;
const input = new Input(canvas, (m) => {
  if (!(dcPath && peer.sendInput(m)) && inputWs.readyState === WebSocket.OPEN) inputWs.send(m);
});

const stats = new Stats();
let pingRtt = NaN;
let pipeline = '';
const prober = new InputProber((x, y) => input.moveTo(x, y), (rtt) => stats.inputRtt.push(rtt));

const worker = new Worker('worker.js', { type: 'module' });
const toWorker = (m: ToWorker, transfer: Transferable[] = []) => worker.postMessage(m, transfer);

// `?transport=ws` keeps everything on the WebSocket, `?transport=webrtc` uses
// the data channel whenever it is up (comparisons, troubleshooting); by
// default the worker picks the path with the shorter round trip.
const transport = params.get('transport');
const webrtc = transport !== 'ws' && 'RTCPeerConnection' in window;
const peer = new Peer({
  signal: (json) => toWorker({ type: 'signal', json: JSON.stringify(json) }),
  message: (buf, received) => toWorker({ type: 'dcmsg', buf, received }, [buf]),
  open: (open) => toWorker({ type: 'dc', open }),
});

function onSignal(json: string): void {
  const m = JSON.parse(json) as { type: string; sdp?: string; iceServers?: RTCIceServer[] };
  if (m.type === 'config' && webrtc) {
    peer.start(m.iceServers ?? []).catch((e) => console.warn('webrtc offer failed', e));
  } else if (m.type === 'answer' && m.sdp) {
    peer.answer(m.sdp).catch((e) => console.warn('webrtc answer rejected', e));
  } else if (m.type === 'closed') {
    // The server dropped the peer: back to the WebSocket until the next offer.
    peer.close();
  } else if (m.type === 'slow') {
    toWorker({ type: 'dcslow' });
  }
}

// `?scale=1` shows remote pixels 1:1 (cropped when larger than the window);
// by default a larger remote screen is scaled down to fit.
const nativeScale = params.get('scale') === '1';
function layout(width: number, height: number): void {
  const dpr = window.devicePixelRatio || 1;
  const fit = nativeScale ? 1 : Math.min(1, (window.innerWidth * dpr) / width, (window.innerHeight * dpr) / height);
  canvas.style.width = `${(width * fit) / dpr}px`;
  canvas.style.height = `${(height * fit) / dpr}px`;
}

function setCursor(buf: ArrayBuffer): void {
  const v = new DataView(buf);
  const w = v.getUint16(2, true);
  const h = v.getUint16(4, true);
  if (w === 0 || h === 0) {
    canvas.style.cursor = 'none';
    return;
  }
  const c = document.createElement('canvas');
  c.width = w;
  c.height = h;
  c.getContext('2d')!.putImageData(new ImageData(new Uint8ClampedArray(buf, 12, w * h * 4), w, h), 0, 0);
  canvas.style.cursor = `url(${c.toDataURL()}) ${v.getUint16(6, true)} ${v.getUint16(8, true)}, default`;
}

worker.onmessage = (ev: MessageEvent<FromWorker>) => {
  const m = ev.data;
  switch (m.type) {
    case 'hello':
      input.screenWidth = m.width;
      input.screenHeight = m.height;
      layout(m.width, m.height);
      window.onresize = () => layout(m.width, m.height);
      break;
    case 'cursor':
      setCursor(m.buf);
      break;
    case 'marker':
      prober.observed(m.marker, m.at);
      break;
    case 'stats':
      stats.add(m.batch);
      pingRtt = m.pingRtt;
      pipeline = m.pipeline;
      break;
    case 'signal':
      onSignal(m.json);
      break;
    case 'dcsend':
      peer.sendFrames(m.buf);
      break;
    case 'path':
      dcPath = m.webrtc;
      break;
  }
};

const offscreen = canvas.transferControlToOffscreen();
// `noread` keeps timing stats but skips the GPU readback the barcode probe needs.
const readBarcode = probeEnabled && !params.has('noread');
toWorker(
  { type: 'init', canvas: offscreen, url: `${wsBase}ws${auth}`, probe: readBarcode, forceDc: transport === 'webrtc' },
  [offscreen],
);
statsEl.hidden = !(params.has('stats') || probeEnabled);
if (!statsEl.hidden) setInterval(() => (statsEl.textContent = `${stats.text()}\n${pipeline}`), 500);
// Recent stats go to the server for the admin UI.
setInterval(() => toWorker({ type: 'report', json: JSON.stringify(stats.brief()) }), 1000);
// A hidden tab asks for no frames: several open tabs would otherwise each
// hold an encoder and a share of the link. Measurements must not depend on
// which window has the focus.
if (benchSeconds === 0) {
  document.addEventListener('visibilitychange', () => toWorker({ type: 'visible', visible: !document.hidden }));
  if (document.hidden) toWorker({ type: 'visible', visible: false });
}

if (benchSeconds > 0) {
  window.__bench = { done: false };
  setTimeout(() => {
    stats.reset();
    prober.start();
    setTimeout(() => {
      prober.stop();
      window.__bench = { done: true, result: { ...stats.report(), pingRtt } };
    }, benchSeconds * 1000);
  }, 2000);
} else if (probeEnabled) {
  setTimeout(() => prober.start(), 2000);
}

canvas.focus();
