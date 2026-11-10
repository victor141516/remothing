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

### Multiplatform core, first cut (before the GPU-capture work)

| scenario | fps | Mbit/s | srv wait | encode | capture→draw | app→draw | input RTT |
|---|---|---|---|---|---|---|---|
| idle (Xvfb shm) | 287 | 6.9 | 0.14 / 0.46 | – | 1.45 / 10.9 | 1.59 / 10.2 | 2.48 / 8.13 |
| idle (X11 XShm path) | 284 | 6.8 | 0.20 / 2.54 | – | 3.40 / 16.9 | 3.54 / 12.7 | 2.53 / 12.8 |
| idle 1280×720 over TLS | 194 | 3.7 | 0.18 / 0.41 | – | 1.82 / 7.70 | 2.01 / 8.03 | 2.60 / 13.4 |

## Linux vs Windows — current build

Same `rt-testapp` scenarios and harness. Linux: Xvfb 1920×1080 in Docker
(WSL2), NVENC through CUDA. Windows: native `remothing.exe` (non-elevated),
test display `\\.\DISPLAY5` 3168×1440 @ 144 Hz captured with DXGI, NVENC with
D3D11 input; the browser runs on the other monitor.

| platform | scenario | fps | Mbit/s | srv wait | upload | encode | capture→draw | app→draw | input RTT |
|---|---|---|---|---|---|---|---|---|---|
| Linux | idle | 301 | 12.5 | 0.10 / 0.25 | – | – | 1.14 / 6.08 | 1.24 / 6.20 | 2.04 / 6.82 |
| Linux | box | 243 | 22.6 | 0.21 / 3.40 | 0.26 / 0.48 | 1.83 / 3.49 | 5.38 / 11.1 | 5.54 / 8.86 | 6.22 / 10.8 |
| Linux | scroll | 183 | 5.2 | 3.09 / 4.76 | 1.43 / 2.26 | 1.86 / 3.19 | 8.83 / 11.4 | 9.44 / 12.1 | 9.62 / 13.4 |
| Linux | full | 202 | 14.6 | 1.25 / 4.81 | 1.78 / 2.58 | 2.07 / 3.65 | 8.39 / 11.2 | 8.96 / 11.7 | 9.33 / 13.5 |
| Windows | idle | 156 | 6.4 | 0.88 / 2.20 | – | – | 2.27 / 6.04 | 7.77 / 12.9 | 10.0 / 32.3 |
| Windows | box | 152 | 18.5 | 1.18 / 3.08 | 0.00 / 0.06 | 2.42 / 4.02 | 7.38 / 13.9 | 15.0 / 49.3 | 16.9 / 56.5 |
| Windows | scroll | 128 | 10.0 | 0.12 / 11.6 | 0.00 / 0.11 | 2.93 / 5.45 | 7.14 / 21.8 | 17.1 / 33.5 | 23.3 / 55.4 |
| Windows | full | 126 | 29.4 | 0.11 / 1.35 | 0.00 / 0.10 | 3.45 / 6.12 | 7.49 / 14.4 | 17.0 / 23.9 | 26.0 / 38.7 |

Reading the numbers:
- **Latency we add** is `capture→draw` (capture of a change → drawn and
  GPU-complete in the browser) plus input injection. At p50 it is within the
  8 ms target on both platforms (lossless paths 1–2.3 ms, video paths
  5.4–8.8 ms); p99 exceeds it on video paths (11–22 ms), dominated by encode
  plus browser-side jitter.
- **Windows `app→draw` / input RTT** also contain what happens before
  capture: the app presents, DWM composes at the display's 144 Hz and only
  then Desktop Duplication delivers the frame (≈5–10 ms). That part is the OS
  compositor, not remothing; Xvfb has no compositor, which is why Linux
  app→draw ≈ capture→draw.
- Windows fps is capped by the 144 Hz test display (duplication delivers at
  most one frame per refresh); Linux Xvfb has no refresh limit.
- Video encode time is per whole frame: 3168×1440 costs ~2.4 ms even for a
  small moving box (NVENC encodes the full surface), ~3.4 ms full motion.
- `client` includes a GPU readback done only by the probe (≈1–3 ms); real use
  doesn't pay it (`&noread`: idle capture→draw 0.49 / 1.98 ms on Linux).

### Optimizations that produced these numbers (Windows)

| change | effect (3168×1440) |
|---|---|
| banded shared screen copy, no commit copy | commit 3.2 ms → 0 |
| skip CPU readback of tiles shown only from video | full: read+diff 8 ms → 0 |
| GPU compute tile diff (`gpu_diff.rs`) | box: tiles read per frame 300–540 → ~20, read+diff 6 ms → 0.45 ms |
| non-blocking `AcquireNextFrame` polling (the blocking call held the D3D11 device lock and stalled NVENC) | full encode 16 ms → 3.4 ms, 50 → 126 fps, capture→draw 40 → 7.5 ms |

### Optimizations that produced these numbers (both)

| change | effect |
|---|---|
| screen moves: DXGI move rects / detected vertical scroll applied before the diff, replayed by clients as GPU copies | scrolling text stays lossless when cheap |
| moved video content stays video in the destination | Linux scroll 55 → 5 Mbit/s, capture→draw 10.3 → 8.8 ms |
