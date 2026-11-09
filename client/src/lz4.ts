// LZ4 block format decoder (no frame header), as produced by lz4_flex::block.

/** Decodes `src` into `dst`; returns the number of bytes written. */
export function lz4Decode(src: Uint8Array, dst: Uint8Array): number {
  let s = 0;
  let d = 0;
  const end = src.length;
  while (s < end) {
    const token = src[s++];
    let lit = token >>> 4;
    if (lit === 15) {
      let b: number;
      do {
        b = src[s++];
        lit += b;
      } while (b === 255);
    }
    if (lit < 32) {
      for (let i = 0; i < lit; i++) dst[d + i] = src[s + i];
    } else {
      dst.set(src.subarray(s, s + lit), d);
    }
    s += lit;
    d += lit;
    if (s >= end) break;
    const offset = src[s] | (src[s + 1] << 8);
    s += 2;
    let len = token & 15;
    if (len === 15) {
      let b: number;
      do {
        b = src[s++];
        len += b;
      } while (b === 255);
    }
    len += 4;
    let m = d - offset;
    if (offset >= len && len >= 32) {
      dst.copyWithin(d, m, m + len);
      d += len;
    } else {
      // Overlapping match (e.g. a run of one repeated pixel): byte-wise copy.
      const stop = d + len;
      while (d < stop) dst[d++] = dst[m++];
    }
  }
  return d;
}
