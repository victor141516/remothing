export interface Summary {
  n: number;
  mean: number;
  p50: number;
  p95: number;
  p99: number;
  max: number;
}

export class Series {
  private values: number[] = [];

  push(v: number): void {
    this.values.push(v);
    if (this.values.length > 20000) this.values.splice(0, 10000);
  }

  clear(): void {
    this.values = [];
  }

  summary(): Summary {
    const s = [...this.values].sort((a, b) => a - b);
    const q = (p: number) => (s.length ? s[Math.min(s.length - 1, Math.floor(p * s.length))] : NaN);
    const mean = s.length ? s.reduce((a, b) => a + b, 0) / s.length : NaN;
    return { n: s.length, mean, p50: q(0.5), p95: q(0.95), p99: q(0.99), max: s.length ? s[s.length - 1] : NaN };
  }
}

export const p50p99 = (s: Summary): string => (s.n ? `${s.p50.toFixed(2)} / ${s.p99.toFixed(2)}` : '-');

/** Per-frame timings in ms. Server timestamps are converted with the synced clock offset. */
export class Stats {
  readonly server = new Series(); // oldest capture -> sent
  readonly net = new Series(); // sent -> received
  readonly client = new Series(); // received -> drawn
  readonly total = new Series(); // oldest capture -> drawn
  readonly appToDraw = new Series(); // app draw (barcode) -> drawn
  frames = 0;
  bytes = 0;
  since = performance.now();

  reset(): void {
    for (const s of [this.server, this.net, this.client, this.total, this.appToDraw]) s.clear();
    this.frames = 0;
    this.bytes = 0;
    this.since = performance.now();
  }

  report(): Record<string, unknown> {
    const secs = (performance.now() - this.since) / 1000;
    return {
      seconds: +secs.toFixed(2),
      fps: +(this.frames / secs).toFixed(1),
      mbps: +((this.bytes * 8) / secs / 1e6).toFixed(2),
      server: this.server.summary(),
      net: this.net.summary(),
      client: this.client.summary(),
      total: this.total.summary(),
      appToDraw: this.appToDraw.summary(),
    };
  }

  text(): string {
    const r = this.report() as Record<string, Summary | number>;
    return [
      `fps ${r.fps}   ${r.mbps} Mbit/s`,
      `            p50 / p99 ms`,
      `server      ${p50p99(r.server as Summary)}`,
      `network     ${p50p99(r.net as Summary)}`,
      `client      ${p50p99(r.client as Summary)}`,
      `capture→draw ${p50p99(r.total as Summary)}`,
      `app→draw    ${p50p99(r.appToDraw as Summary)}`,
    ].join('\n');
  }
}
