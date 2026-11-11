// WebTransport path (runs in the stream worker): frames arrive as datagram
// pieces (S_PIECE), ACKs and pings leave as datagrams, input on one
// unidirectional stream.
//
// A lost piece costs only its own frame a little time: one piece of each
// parity group is rebuilt on the spot; otherwise the missing pieces are
// asked for again (C_NACK) as soon as the frame stops growing, and the frame
// is given up only when they keep not coming. Nothing waits for a transport
// timer, unlike TCP and the reliable data channel.
//
// The server's certificate is its own short-lived one, accepted because its
// hash came over the WebSocket of a page the browser already trusts.

import { C_NACK, FLAG_VIDEO, PIECE_HEADER_LEN, PIECE_PAYLOAD, S_PIECE, S_PONG, now } from './proto';

/** What the server says about reaching it (S_SIGNAL `wt`). */
export interface WtOffer {
  key: string;
  hash: string | null;
  port: number;
  candidates: string[];
}

export interface WtEvents {
  /** A complete frame; `repaired`: only after asking for pieces again. */
  frame(buf: ArrayBuffer, received: number, repaired: boolean): void;
  /** Frame `seq` will not come; `video`: it carried (or may have carried) video. */
  giveUp(seq: number, video: boolean): void;
  pong(v: DataView, received: number): void;
  /** The session opened with `key` closed: why. */
  down(why: string, key: string): void;
}

/** Candidates are tried at once; the first up wins, the rest are closed. */
const CONNECT_MS = 3000;
/** Pieces come in order: a hole older than this behind a later piece is lost. */
const HOLE_MS = 4;
/** The rest of a frame is lost when its pieces stopped for this long (and
 * three times the usual spacing between them: on a slow link they come far
 * apart), or when pieces of a later frame arrive. */
const QUIET_MS = 4;
/** Asking again: not before a round trip (at least this), at most this often. */
const NACK_MIN_MS = 15;
const NACKS = 4;
/** A frame that got nothing new for this long (or 4 round trips) is given up. */
const GIVE_UP_MS = 250;

interface Assembly {
  /** Data and parity pieces; 0 while not one piece arrived (the frame size is unknown). */
  data: number;
  parity: number;
  len: number;
  video: boolean;
  pieces: (Uint8Array | undefined)[];
  /** Data pieces present (received or rebuilt). */
  got: number;
  /** Pieces received, the highest data index among them, since when a hole is open (0: none). */
  count: number;
  top: number;
  holeAt: number;
  first: number;
  last: number;
  nacks: number;
  nackAt: number;
}

/** `a` precedes `b` in wrapping 32-bit sequence order. */
const before = (a: number, b: number) => ((b - a) | 0) > 0;

export class WtPath {
  private transport: WebTransport | null = null;
  private datagrams: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private input: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private assembling = new Map<number, Assembly>();
  /** Newest frame a piece arrived for (-1: none). */
  private newest = -1;
  /** Shortest round trip seen on this path, ms. */
  rtt = 0;
  /** Round trip of the last ping answered, ms: grows with the queue on the path. */
  lastRtt = 0;
  /** Key of the server session this one belongs to. */
  private key = '';

  /** `expected`: the next frame the worker needs; older ones are done. */
  constructor(
    private readonly events: WtEvents,
    private readonly expected: () => number,
  ) {}

  get up(): boolean {
    return this.datagrams !== null;
  }

  /** Tries every candidate of `offer`; resolves to where it connected, or rejects with why not. */
  async connect(offer: WtOffer): Promise<string> {
    if (typeof WebTransport === 'undefined') throw new Error('not in this browser');
    const hash = offer.hash ? { algorithm: 'sha-256', value: hexBytes(offer.hash) } : null;
    const urls = new Set<string>();
    for (const c of offer.candidates) {
      if (c.startsWith('https://')) {
        urls.add(`${c}${c.includes('?') ? '&' : '?'}k=${encodeURIComponent(offer.key)}`);
      } else {
        const host = c || location.hostname;
        // "host:port" (not an IPv6 address) carries its own port.
        const hostPort = /^[^:]+:\d+$/.test(host) ? host : `${host}:${offer.port}`;
        urls.add(`https://${hostPort}/wt?k=${encodeURIComponent(offer.key)}`);
      }
    }
    // Grouped by error, each host with how long it took: a failure within
    // milliseconds was refused on the way (or by this browser), one at the
    // timeout never got an answer.
    const failures = new Map<string, string[]>();
    const started = performance.now();
    const attempts = [...urls].map(async (url) => {
      const pinned = hash && !offer.candidates.some((c) => c.startsWith('https://') && url.startsWith(c));
      const t = new WebTransport(url, {
        requireUnreliable: true,
        congestionControl: 'low-latency',
        ...(pinned ? { serverCertificateHashes: [hash] } : {}),
      });
      const timer = setTimeout(() => t.close(), CONNECT_MS);
      try {
        await t.ready;
        clearTimeout(timer);
        return { t, url };
      } catch (e) {
        clearTimeout(timer);
        const why = e instanceof Error ? e.message : String(e);
        const hosts = failures.get(why) ?? [];
        hosts.push(`${new URL(url).host} ${(performance.now() - started).toFixed(0)} ms`);
        failures.set(why, hosts);
        throw e;
      }
    });
    let won: { t: WebTransport; url: string };
    try {
      won = await Promise.any(attempts);
    } catch {
      throw new Error([...failures].map(([why, hosts]) => `${why} (${hosts.join(', ')})`).join('; '));
    }
    // Later winners are not needed.
    for (const a of attempts) {
      void a.then(({ t }) => t !== won.t && t.close()).catch(() => {});
    }
    this.key = offer.key;
    this.start(won.t);
    return new URL(won.url).host;
  }

  private start(t: WebTransport): void {
    this.transport = t;
    this.datagrams = t.datagrams.writable.getWriter();
    this.assembling.clear();
    this.newest = -1;
    this.rtt = 0;
    void t.createUnidirectionalStream().then((s) => (this.input = s.getWriter())).catch(() => {});
    void (async () => {
      const reader = t.datagrams.readable.getReader();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) return;
        this.onDatagram(value, now());
      }
    })().catch(() => {});
    void t.closed.then(
      () => this.closed(t, 'closed by the server'),
      (e: unknown) => this.closed(t, e instanceof Error ? e.message : String(e)),
    );
  }

  private closed(t: WebTransport, why: string): void {
    if (this.transport !== t) return;
    this.transport = null;
    this.datagrams = null;
    this.input = null;
    this.events.down(why, this.key);
  }

  close(why: string): void {
    const t = this.transport;
    if (!t) return;
    this.closed(t, why);
    t.close();
  }

  send(m: Uint8Array): void {
    void this.datagrams?.write(m).catch(() => {});
  }

  /** Input, in order (each message after its u16 length). */
  sendInput(m: Uint8Array): boolean {
    if (!this.input) return false;
    const framed = new Uint8Array(2 + m.length);
    new DataView(framed.buffer).setUint16(0, m.length, true);
    framed.set(m, 2);
    void this.input.write(framed).catch(() => {});
    return true;
  }

  private onDatagram(d: Uint8Array, received: number): void {
    const v = new DataView(d.buffer, d.byteOffset, d.byteLength);
    if (d[0] === S_PONG) {
      this.events.pong(v, received);
      const rtt = received - v.getFloat64(8, true);
      this.rtt = this.rtt ? Math.min(this.rtt, rtt) : rtt;
      this.lastRtt = rtt;
      return;
    }
    if (d[0] !== S_PIECE || d.length < PIECE_HEADER_LEN) return;
    const seq = v.getUint32(8, true);
    if (before(seq, this.expected())) return;
    if (this.newest < 0 || before(this.newest, seq)) this.newest = seq;
    const index = v.getUint16(2, true);
    const data = v.getUint16(4, true);
    const parity = v.getUint16(6, true);
    let a = this.assembling.get(seq);
    if (!a || a.data === 0) {
      a = {
        data, parity, len: v.getUint32(12, true), video: (d[1] & FLAG_VIDEO) !== 0,
        pieces: new Array(data + parity), got: 0, count: 0, top: -1, holeAt: 0,
        first: a?.first ?? received, last: received, nacks: a?.nacks ?? 0, nackAt: a?.nackAt ?? 0,
      };
      this.assembling.set(seq, a);
    }
    if (a.pieces[index]) return;
    a.pieces[index] = d.subarray(PIECE_HEADER_LEN);
    a.last = received;
    a.count++;
    if (index < a.data) {
      a.got++;
      if (index > a.top + 1 && !a.holeAt) a.holeAt = received;
      a.top = Math.max(a.top, index);
    }
    this.rebuild(a, index < a.data ? index % a.parity : index - a.data);
    if (a.got === a.data) this.complete(seq, a, received);
  }

  /** Length of data piece `i`. */
  private static size(a: Assembly, i: number): number {
    return i === a.data - 1 ? a.len - i * PIECE_PAYLOAD : PIECE_PAYLOAD;
  }

  /** A parity group missing one data piece: that piece is the parity XOR the others. */
  private rebuild(a: Assembly, group: number): void {
    const parity = a.pieces[a.data + group];
    if (!parity) return;
    let missing = -1;
    for (let i = group; i < a.data; i += a.parity) {
      if (a.pieces[i]) continue;
      if (missing >= 0) return;
      missing = i;
    }
    if (missing < 0) return;
    const out = parity.slice(0, WtPath.size(a, missing));
    for (let i = group; i < a.data; i += a.parity) {
      const p = a.pieces[i];
      if (!p) continue;
      for (let k = 0; k < Math.min(p.length, out.length); k++) out[k] ^= p[k];
    }
    a.pieces[missing] = out;
    a.got++;
  }

  private complete(seq: number, a: Assembly, received: number): void {
    this.assembling.delete(seq);
    const out = new Uint8Array(a.len);
    for (let i = 0; i < a.data; i++) out.set(a.pieces[i]!, i * PIECE_PAYLOAD);
    this.events.frame(out.buffer, received, a.nacks > 0);
  }

  /** Every few ms: asks for what is missing, gives up what will not come. */
  tick(): void {
    if (!this.up) return;
    const t = now();
    const expected = this.expected();
    // Frames not one piece of which arrived, while later ones did.
    if (this.newest >= 0) {
      for (let s = expected; before(s, this.newest); s = (s + 1) >>> 0) {
        if (!this.assembling.has(s)) {
          this.assembling.set(s, {
            data: 0, parity: 0, len: 0, video: true, pieces: [], got: 0, count: 0, top: -1, holeAt: 0,
            first: t, last: t, nacks: 0, nackAt: 0,
          });
        }
      }
    }
    const rtt = this.rtt || NACK_MIN_MS;
    for (const [seq, a] of this.assembling) {
      if (before(seq, expected)) {
        this.assembling.delete(seq);
        continue;
      }
      if (t - Math.max(a.last, a.nackAt) > Math.max(GIVE_UP_MS, 4 * rtt) || a.nacks > NACKS) {
        this.assembling.delete(seq);
        this.events.giveUp(seq, a.video);
        continue;
      }
      if (t - a.nackAt < Math.max(NACK_MIN_MS, 1.5 * rtt)) continue;
      const spacing = a.count > 1 ? (a.last - a.first) / (a.count - 1) : 0;
      const tail = a.data === 0 || before(seq, this.newest) || t - a.last > Math.max(QUIET_MS, 3 * spacing);
      const holes = a.holeAt > 0 && t - a.holeAt > HOLE_MS;
      if (!tail && !holes) continue;
      const missing: number[] = [];
      for (let i = 0; i < (tail ? a.data : a.top); i++) if (!a.pieces[i]) missing.push(i);
      // Without a size (or too many to list) the server sends them all.
      const list = a.data === 0 || missing.length > 400 ? [] : missing;
      const m = new Uint8Array(8 + 2 * list.length);
      const v = new DataView(m.buffer);
      m[0] = C_NACK;
      v.setUint16(2, list.length, true);
      v.setUint32(4, seq, true);
      list.forEach((i, k) => v.setUint16(8 + 2 * k, i, true));
      this.send(m);
      a.nacks++;
      a.nackAt = t;
      a.holeAt = 0;
    }
  }
}

function hexBytes(hex: string): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
  return out;
}
