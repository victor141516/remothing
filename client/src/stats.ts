// Timing statistics. The stream worker only records raw samples (O(1) per
// frame) and ships them in batches; percentiles are computed on the main
// thread, over a short window for the live overlay so sorting stays cheap.

export interface Summary {
  n: number;
  mean: number;
  p50: number;
  p95: number;
  p99: number;
  max: number;
}

/** Per-frame timings in ms. Server timestamps are converted with the synced clock offset. */
export const SERIES = [
  'server', // oldest capture -> sent
  'serverWait', // oldest capture -> frame build started (ACK window / scheduling)
  'upload', // video encoder input upload (changed regions)
  'encode', // H.264 encode
  'net', // sent -> received
  'decode', // received -> video picture decoded
  'client', // received -> drawn
  'total', // oldest capture -> drawn
  'appToDraw', // app draw (barcode) -> drawn
] as const;
export type SeriesName = (typeof SERIES)[number];

export interface Batch {
  samples: Record<SeriesName, number[]>;
  frames: number;
  videoFrames: number;
  bytes: number;
}

const emptySamples = () => Object.fromEntries(SERIES.map((s) => [s, []])) as unknown as Record<SeriesName, number[]>;

export class Recorder {
  private batch: Batch = { samples: emptySamples(), frames: 0, videoFrames: 0, bytes: 0 };

  push(name: SeriesName, v: number): void {
    this.batch.samples[name].push(v);
  }

  frame(bytes: number, video: boolean): void {
    this.batch.frames++;
    this.batch.bytes += bytes;
    if (video) this.batch.videoFrames++;
  }

  take(): Batch {
    const b = this.batch;
    this.batch = { samples: emptySamples(), frames: 0, videoFrames: 0, bytes: 0 };
    return b;
  }
}

export class Series {
  private values: number[] = [];
  push(v: number): void {
    this.values.push(v);
    if (this.values.length > 200_000) this.values.splice(0, 100_000);
  }

  clear(): void {
    this.values = [];
  }

  /** Percentiles over all samples, or the most recent `last`. */
  summary(last = Infinity): Summary {
    const s = (last < this.values.length ? this.values.slice(-last) : [...this.values]).sort((a, b) => a - b);
    const q = (p: number) => (s.length ? s[Math.min(s.length - 1, Math.floor(p * s.length))] : NaN);
    const mean = s.length ? s.reduce((a, b) => a + b, 0) / s.length : NaN;
    return { n: s.length, mean, p50: q(0.5), p95: q(0.95), p99: q(0.99), max: s.length ? s[s.length - 1] : NaN };
  }
}

export const p50p99 = (s: Summary): string => (s.n ? `${s.p50.toFixed(2)} / ${s.p99.toFixed(2)}` : '-');

const OVERLAY_WINDOW = 500;

export class Stats {
  readonly series = Object.fromEntries(SERIES.map((s) => [s, new Series()])) as unknown as Record<SeriesName, Series>;
  readonly inputRtt = new Series(); // input sent -> its effect drawn
  private frames = 0;
  private videoFrames = 0;
  private bytes = 0;
  private since = performance.now();
  // Rate over the last overlay refresh.
  private recent = { frames: 0, videoFrames: 0, bytes: 0, since: performance.now() };

  add(b: Batch): void {
    for (const name of SERIES) for (const v of b.samples[name]) this.series[name].push(v);
    this.frames += b.frames;
    this.videoFrames += b.videoFrames;
    this.bytes += b.bytes;
    this.recent.frames += b.frames;
    this.recent.videoFrames += b.videoFrames;
    this.recent.bytes += b.bytes;
  }

  reset(): void {
    for (const s of Object.values(this.series)) s.clear();
    this.inputRtt.clear();
    this.frames = this.videoFrames = this.bytes = 0;
    this.since = performance.now();
  }

  report(): Record<string, unknown> {
    const secs = (performance.now() - this.since) / 1000;
    return {
      seconds: +secs.toFixed(2),
      fps: +(this.frames / secs).toFixed(1),
      videoFrames: this.videoFrames,
      mbps: +((this.bytes * 8) / secs / 1e6).toFixed(2),
      ...Object.fromEntries(SERIES.map((s) => [s, this.series[s].summary()])),
      inputRtt: this.inputRtt.summary(),
    };
  }

  /** Recent p50/p99 (ms) of the main latencies, reported to the server for the admin UI. */
  brief(): Record<string, [number, number] | null> {
    const pair = (s: Series): [number, number] | null => {
      const x = s.summary(OVERLAY_WINDOW);
      return x.n ? [+x.p50.toFixed(2), +x.p99.toFixed(2)] : null;
    };
    return {
      captureToDraw: pair(this.series.total),
      appToDraw: pair(this.series.appToDraw),
      inputRtt: pair(this.inputRtt),
      network: pair(this.series.net),
      decode: pair(this.series.decode),
    };
  }

  text(): string {
    const r = this.recent;
    const secs = (performance.now() - r.since) / 1000;
    const line = (s: Series) => p50p99(s.summary(OVERLAY_WINDOW));
    const out = [
      `fps ${(r.frames / secs).toFixed(0)} (video ${(r.videoFrames / secs).toFixed(0)})   ${((r.bytes * 8) / secs / 1e6).toFixed(2)} Mbit/s`,
      `            p50 / p99 ms`,
      `server      ${line(this.series.server)}`,
      `  wait      ${line(this.series.serverWait)}`,
      `  upload    ${line(this.series.upload)}`,
      `  encode    ${line(this.series.encode)}`,
      `network     ${line(this.series.net)}`,
      `client      ${line(this.series.client)}`,
      `  decode    ${line(this.series.decode)}`,
      `capture→draw ${line(this.series.total)}`,
      `app→draw    ${line(this.series.appToDraw)}`,
      `input rtt   ${line(this.inputRtt)}`,
    ].join('\n');
    this.recent = { frames: 0, videoFrames: 0, bytes: 0, since: performance.now() };
    return out;
  }
}
