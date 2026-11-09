// Latency probes for rt-testapp (see testapp/src/main.rs).
// BarcodeReader runs in the render worker and decodes the barcode from the
// drawn frame; InputProber runs on the main thread, sends pointer-motion
// markers through the real input path and matches them with decoded markers.

import { now } from './proto';
import type { Renderer } from './renderer';

const BLOCK = 16;
const BITS = 64;
const MAGIC = 0xa5;
const MARKER_BASE = 1100;
const MARKER_SPAN = 700;
const MARKER_TIMEOUT_MS = 500;

export interface Barcode {
  /** app draw time, low 40 bits of server-clock microseconds */
  ts: number;
  marker: number;
}

export class BarcodeReader {
  private row = new Uint8Array(BLOCK * BITS * 4);

  constructor(private readonly renderer: Renderer) {}

  read(): Barcode | null {
    if (this.renderer.width < BLOCK * BITS) return null;
    this.renderer.readRow(BLOCK / 2, BLOCK * BITS, this.row);
    let lo = 0;
    let hi = 0;
    for (let i = 0; i < BITS; i++) {
      const p = (i * BLOCK + BLOCK / 2) * 4;
      if (this.row[p] + this.row[p + 1] + this.row[p + 2] < 384) continue;
      if (i < 32) lo = (lo | (1 << i)) >>> 0;
      else hi = (hi | (1 << (i - 32))) >>> 0;
    }
    if ((lo & 0xff) !== MAGIC) return null;
    return { ts: (lo >>> 8) + (hi & 0xffff) * 2 ** 24, marker: hi >>> 16 };
  }
}

/** Age in ms of a 40-bit server-clock µs timestamp at server time `serverNowMs`. */
export function barcodeAge(ts: number, serverNowMs: number): number {
  const nowUs = Math.round(serverNowMs * 1000);
  return ((((nowUs - ts) % 2 ** 40) + 2 ** 40) % 2 ** 40) / 1000;
}

export class InputProber {
  private pending = -1;
  private sentAt = 0;
  private index = 0;
  private timer = 0;
  private running = false;

  constructor(
    private readonly moveTo: (x: number, y: number) => void,
    private readonly onSample: (rttMs: number) => void,
  ) {}

  start(): void {
    this.running = true;
    this.next();
  }

  stop(): void {
    this.running = false;
    clearTimeout(this.timer);
  }

  /** A frame showing `marker` was drawn at wall-clock `drawnAt`. */
  observed(marker: number, drawnAt: number): void {
    if (!this.running || marker !== this.pending) return;
    this.onSample(drawnAt - this.sentAt);
    this.pending = -1;
    clearTimeout(this.timer);
    // Random gap so markers do not phase-lock with the app's frame clock.
    this.timer = window.setTimeout(() => this.next(), Math.random() * 8);
  }

  private next(): void {
    if (!this.running) return;
    this.index = (this.index + 1) % MARKER_SPAN;
    this.pending = MARKER_BASE + this.index;
    this.sentAt = now();
    this.moveTo(this.pending, 600);
    clearTimeout(this.timer);
    this.timer = window.setTimeout(() => this.next(), MARKER_TIMEOUT_MS);
  }
}
