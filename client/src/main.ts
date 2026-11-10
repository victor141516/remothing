// Main thread: input capture on its own WebSocket (sent the moment the event
// fires), cursor, stats overlay. Frames are handled in worker.ts.

import { Input } from './input';
import { InputProber } from './probe';
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
const wsBase = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}`;
// Stream access token, given in the page URL (`?token=`).
const auth = `?token=${encodeURIComponent(params.get('token') ?? '')}`;

const canvas = document.getElementById('screen') as HTMLCanvasElement;
const statsEl = document.getElementById('stats') as HTMLDivElement;

let inputWs: WebSocket;
function connectInput(): void {
  inputWs = new WebSocket(`${wsBase}/input${auth}`);
  inputWs.onclose = () => setTimeout(connectInput, 1000);
}
connectInput();
const input = new Input(canvas, (m) => {
  if (inputWs.readyState === WebSocket.OPEN) inputWs.send(m);
});

const stats = new Stats();
let pingRtt = NaN;
let pipeline = '';
const prober = new InputProber((x, y) => input.moveTo(x, y), (rtt) => stats.inputRtt.push(rtt));

const worker = new Worker('worker.js', { type: 'module' });
const toWorker = (m: ToWorker, transfer: Transferable[] = []) => worker.postMessage(m, transfer);

function layout(width: number, height: number): void {
  const dpr = window.devicePixelRatio || 1;
  canvas.style.width = `${width / dpr}px`;
  canvas.style.height = `${height / dpr}px`;
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
  }
};

const offscreen = canvas.transferControlToOffscreen();
// `noread` keeps timing stats but skips the GPU readback the barcode probe needs.
const readBarcode = probeEnabled && !params.has('noread');
toWorker({ type: 'init', canvas: offscreen, url: `${wsBase}/ws${auth}`, probe: readBarcode }, [offscreen]);
statsEl.hidden = !(params.has('stats') || probeEnabled);
if (!statsEl.hidden) setInterval(() => (statsEl.textContent = `${stats.text()}\n${pipeline}`), 500);
// Recent stats go to the server for the admin UI.
setInterval(() => toWorker({ type: 'report', json: JSON.stringify(stats.brief()) }), 1000);

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
