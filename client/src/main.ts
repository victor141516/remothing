// Main thread: input capture (sent the moment the event fires, over the
// WebRTC input channel when frames use the data channel, else its own
// WebSocket), the peer connection, cursor, control bar, stats overlay.
// Frames are handled in worker.ts.

import { Sound } from './audio';
import { Input } from './input';
import { Microphone } from './mic';
import { Peer } from './peer';
import { InputProber } from './probe';
import { C_PING, S_PONG, now } from './proto';
import { Stats } from './stats';
import { Ui, type DisplayEntry } from './ui';
import type { FromWorker, Path, ToWorker } from './worker';

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
/** Where frames go (the worker's choice): input follows. */
let path: Path = 'ws';
const input = new Input(
  canvas,
  (m) => {
    if (path === 'wt') {
      const buf = m.slice().buffer;
      toWorker({ type: 'input', buf }, [buf]);
    } else if (!(path === 'webrtc' && peer.sendInput(m)) && inputWs.readyState === WebSocket.OPEN) {
      inputWs.send(m);
    }
  },
  showPointer,
);

const stats = new Stats();
let pingRtt = NaN;
let pipeline = '';
const prober = new InputProber((x, y) => input.moveTo(x, y), (rtt) => stats.inputRtt.push(rtt));

const worker = new Worker('worker.js', { type: 'module' });
const toWorker = (m: ToWorker, transfer: Transferable[] = []) => worker.postMessage(m, transfer);

const ui = new Ui(document.getElementById('ui') as HTMLDivElement, {
  key: (evdev, down) => input.setKey(evdev, down),
  text: (text) => input.text(text),
  clipboard: (text) => toWorker({ type: 'clipboard', text }),
  listDisplays: () => toWorker({ type: 'displays' }),
  selectDisplay: (id) => toWorker({ type: 'display', id }),
  toggleStats: () => (statsEl.hidden = !statsEl.hidden),
  toggleSound: () => sound.toggle(),
  toggleMic: () => void mic.toggle(),
  layout,
});

const sound = new Sound({
  request: (on) => toWorker({ type: 'audio', on }),
  log: (text) => toWorker({ type: 'event', text }),
  changed: () => ui.sound(sound.playing, sound.status),
});
ui.sound(sound.playing, sound.status);

const mic = new Microphone({
  tell: (message) => toWorker({ type: 'mic', json: JSON.stringify(message) }),
  send: (message) => toWorker({ type: 'micdata', buf: message.buffer as ArrayBuffer }, [message.buffer]),
  log: (text) => toWorker({ type: 'event', text }),
  ask: (question) => ui.ask(question),
  changed: () => ui.microphone(mic.state, mic.status),
});
ui.microphone(mic.state, mic.status);

// `?transport=ws` keeps everything on the WebSocket, `?transport=webrtc` uses
// the data channel whenever it is up, `?transport=wt` WebTransport only
// (comparisons, troubleshooting); by default WebTransport when it comes up,
// else the worker picks the path with the shorter round trip.
const transport = params.get('transport');
const webrtc = transport !== 'ws' && transport !== 'wt' && 'RTCPeerConnection' in window;
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
// by default a larger remote screen is scaled down to fit what the
// keyboards leave free (ours, and a phone's own: it shrinks the visual
// viewport).
const nativeScale = params.get('scale') === '1';
function layout(): void {
  const { screenWidth: width, screenHeight: height } = input;
  if (width === 0) return;
  const dpr = window.devicePixelRatio || 1;
  const view = window.visualViewport;
  const free = (view?.height ?? window.innerHeight) - ui.reservedBottom;
  const fit = nativeScale ? 1 : Math.min(1, ((view?.width ?? window.innerWidth) * dpr) / width, (free * dpr) / height);
  canvas.style.width = `${(width * fit) / dpr}px`;
  canvas.style.height = `${(height * fit) / dpr}px`;
  showPointer();
  reportView(Math.round(width * fit), Math.round(height * fit));
}
window.addEventListener('resize', layout);
window.visualViewport?.addEventListener('resize', layout);

/** A window being resized passes through many sizes: the server hears of
 * the size once it has settled this long (each change costs it an encoder
 * restart and a keyframe in motion mode). */
const VIEW_SETTLE_MS = 3000;
let viewTimer = 0;
let shownView = '';
/** The display's refresh rate, from animation frames (0 until measured). */
let refreshHz = 0;
function reportView(width: number, height: number): void {
  // `?bench` measures the pipeline at the display's size and the capture rate.
  if (benchSeconds > 0) return;
  clearTimeout(viewTimer);
  const send = () => {
    const key = `${width}x${height}@${refreshHz}`;
    if (key === shownView) return;
    shownView = key;
    toWorker({ type: 'view', width, height, hz: refreshHz });
  };
  // The first report goes at once; later ones once the window settles.
  viewTimer = setTimeout(send, shownView ? VIEW_SETTLE_MS : 0);
}

/** Median interval of 60 animation frames, rounded to the usual rates. */
function measureRefresh(): void {
  const stamps: number[] = [];
  const step = (t: number) => {
    stamps.push(t);
    if (stamps.length <= 60) {
      requestAnimationFrame(step);
      return;
    }
    const gaps = stamps.slice(1).map((s, i) => s - stamps[i]).sort((a, b) => a - b);
    refreshHz = Math.round(1000 / gaps[gaps.length >> 1]);
    shownView = '';
    layout();
  };
  requestAnimationFrame(step);
}
// A hidden tab gets no animation frames: measured once it is shown.
if (document.hidden) document.addEventListener('visibilitychange', measureRefresh, { once: true });
else measureRefresh();

const pointer = document.getElementById('pointer') as HTMLCanvasElement;
/** Hot spot of the remote cursor's shape; null while the remote side hides its cursor. */
let hotSpot: [number, number] | null = null;

/** A finger has no cursor in the browser: with touch input the remote
 * cursor's shape is drawn over the picture, where the pointer is. */
function showPointer(): void {
  pointer.hidden = !input.touch || hotSpot === null || input.screenWidth === 0;
  if (pointer.hidden || hotSpot === null) return;
  const r = canvas.getBoundingClientRect();
  const x = r.left + (input.pointerX * r.width) / input.screenWidth - hotSpot[0];
  const y = r.top + (input.pointerY * r.height) / input.screenHeight - hotSpot[1];
  pointer.style.transform = `translate(${x}px, ${y}px)`;
}

function setCursor(buf: ArrayBuffer): void {
  const v = new DataView(buf);
  const w = v.getUint16(2, true);
  const h = v.getUint16(4, true);
  if (w === 0 || h === 0) {
    canvas.style.cursor = 'none';
    hotSpot = null;
  } else {
    pointer.width = w;
    pointer.height = h;
    pointer.getContext('2d')!.putImageData(new ImageData(new Uint8ClampedArray(buf, 12, w * h * 4), w, h), 0, 0);
    hotSpot = [v.getUint16(6, true), v.getUint16(8, true)];
    canvas.style.cursor = `url(${pointer.toDataURL()}) ${hotSpot[0]} ${hotSpot[1]}, default`;
  }
  showPointer();
}

worker.onmessage = (ev: MessageEvent<FromWorker>) => {
  const m = ev.data;
  switch (m.type) {
    case 'hello':
      input.setScreen(m.width, m.height);
      layout();
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
      path = m.path;
      break;
    case 'clipboard':
      ui.remoteClipboard(m.text);
      break;
    case 'displays': {
      const list = JSON.parse(m.json) as { displays: DisplayEntry[]; current: string | null };
      ui.displays(list.displays, list.current);
      break;
    }
    case 'audioinfo':
      sound.info(m.json);
      break;
    case 'audio':
      sound.packet(m.buf);
      break;
    case 'micinfo':
      mic.info(m.json);
      break;
  }
};

const offscreen = canvas.transferControlToOffscreen();
// `noread` keeps timing stats but skips the GPU readback the barcode probe needs.
const readBarcode = probeEnabled && !params.has('noread');
toWorker(
  {
    type: 'init',
    canvas: offscreen,
    url: `${wsBase}ws${auth}`,
    probe: readBarcode,
    forceDc: transport === 'webrtc',
    wt: transport !== 'ws' && transport !== 'webrtc',
    // `?codec=h264|hevc|av1`: offer only that codec (H.264 is always possible).
    codec: params.get('codec'),
  },
  [offscreen],
);
statsEl.hidden = !(params.has('stats') || probeEnabled);
setInterval(() => {
  if (!statsEl.hidden) statsEl.textContent = `${stats.text()}\n${pipeline}\nsound ${sound.status}`;
}, 500);
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
