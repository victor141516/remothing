# Measurements

All numbers are p50 / p99 in milliseconds unless noted, measured with the
same harness on every platform: `rt-testapp` draws a barcode (wall-clock
timestamp + last pointer x) on the streamed display; the client opened with
`?bench=10` decodes it after drawing and reports:

| column | meaning |
|---|---|
| srv wait | oldest captured change → frame build starts (ACK window, encoder busy) |
| encode | video input upload + H.264 encode |
| server | oldest captured change → written to the socket |
| net | socket write → message received in the client worker (synced clocks) |
| decode | received → WebCodecs picture available (video frames only) |
| client | received → drawn and GPU-complete (probe reads pixels back) |
| capture→draw | oldest captured change → drawn |
| app→draw | app drew the barcode → drawn (end to end, excludes display scan-out) |
| input RTT | input sent by the browser → its effect drawn in the browser |

Scenarios (1920×1080, testapp at 165 fps): **idle** (only the barcode changes),
**box** (256×256 box moving over text), **scroll** (text scrolling 3 px/frame),
**full** (whole screen animated, video-like).

## Linux (Docker on WSL2, RTX 4080 SUPER, Edge 156, 165 Hz)

### Iteration history

| iteration | scenario | fps | Mbit/s | capture→draw | input RTT |
|---|---|---|---|---|---|
| 1 raw tiles, main-thread client | idle | 209 | 111 | 1.24 / 42.2 | 6.30 / 48.6 |
| 1 raw tiles, worker + input socket | idle | 279 | 150 | 2.73 / 12.9 | 3.00 / 12.0 |
| 1 raw tiles | full | 10 | 672 | 226 / 322 | 204 / 343 |
| 2 hybrid video (NVENC host input) | full | 194 | 14.4 | 8.45 / 15.1 | 9.93 / 15.9 |
| 2 + CUDA partial uploads | box | 257 | 37.9 | 4.72 / 13.5 | 6.19 / 14.3 |
| 3 + LZ4 + cost policy | idle | 284 | 6.7 | 2.32 / 12.3 | 2.47 / 11.8 |
| 3 + stats off the worker hot path | idle | 320 | 7.9 | 2.37 / 5.44 | 1.45 / 4.61 |
| 3 + stats off the worker hot path | box | 262 | 8.9 | 4.87 / 8.04 | 5.60 / 9.76 |
| 3 + stats off the worker hot path | full | 210 | 14.7 | 7.57 / 11.0 | 8.49 / 12.9 |

Without the GPU readback of the probe (`&noread`): idle capture→draw
0.49 / 1.98 ms, full 5.03 / 7.03 ms.

### Multiplatform core (current)

| scenario | fps | Mbit/s | srv wait | encode | capture→draw | app→draw | input RTT |
|---|---|---|---|---|---|---|---|
| idle (Xvfb shm) | 287 | 6.9 | 0.14 / 0.46 | – | 1.45 / 10.9 | 1.59 / 10.2 | 2.48 / 8.13 |
| idle (X11 XShm path) | 284 | 6.8 | 0.20 / 2.54 | – | 3.40 / 16.9 | 3.54 / 12.7 | 2.53 / 12.8 |
| idle 1280×720 over TLS | 194 | 3.7 | 0.18 / 0.41 | – | 1.82 / 7.70 | 2.01 / 8.03 | 2.60 / 13.4 |
| box | 244 | 8.8 | 0.50 / 3.35 | 2.40 / 3.99 | 6.11 / 9.63 | 6.42 / 9.72 | 7.15 / 11.6 |
| full | 203 | 14.6 | 2.42 / 5.36 | 3.26 / 5.05 | 8.82 / 13.9 | 9.78 / 14.5 | 10.0 / 16.2 |

Notes:
- Lossless paths (idle, typing, small widgets) are well under the 8 ms
  target at p50; p99 is dominated by browser-side delivery jitter.
- Video paths are bounded by NVENC under WSL2 (≈2–3 ms per 1080p frame incl.
  upload); `srv wait` grows when a new change arrives while the previous
  frame is still encoding.
