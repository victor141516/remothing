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

## Bad links (link model, `pacer.rs`)

Linux bench container (Xvfb 1920×1080, NVENC) with the same harness, before
(fixed 2-frame ACK window, LZ4 only, constant-QP video) and after (link
model, paced video with a bitrate cap, zstd, video-first bursts). "Funnel
(real)": through Tailscale Funnel's public ingress from the same machine
(curl: ≈18 Mbit/s, ping RTT ≈50 ms). The other links are `tc netem` on the
container's egress (server → client); all delay sits on that direction, so
the clock sync halves it and capture→draw reads ≈half the one-way delay low.
One client per run.

| link | scenario | fps before | capture→draw before | fps after | capture→draw after | video fps after | Mbit/s after |
|---|---|---|---|---|---|---|---|
| Funnel (real) | idle | 31 | 56 / 114 | 170 | 26 / 99 | 0 | 6.6 |
| Funnel (real) | box | 34 | 57 / 119 | 168 | 26 / 106 | 152 | 3.7 |
| Funnel (real) | scroll | 10 | 259 / 1508 | 165 | 32 / 76 | 161 | 5.8 |
| Funnel (real) | full | 30 | 68 / 124 | 169 | 31 / 62 | 169 | 13.7 |
| WAN 50M/20ms | box | 61 | 35 / 63 | 205 | 16 / 23 | 0 | 27.7 |
| WAN 50M/20ms | scroll | 62 | 38 / 124 | 165 | 20 / 76 | 161 | 7.4 |
| WAN 50M/20ms | full | 65 | 35 / 51 | 173 | 20 / 77 | 173 | 13.6 |
| Funnel-like 18M/40ms | box | 32 | 74 / 139 | 195 | 26 / 80 | 175 | 4.4 |
| Funnel-like 18M/40ms | scroll | 11 | 194 / 875 | 166 | 32 / 88 | 150 | 6.8 |
| Funnel-like 18M/40ms | full | 36 | 60 / 92 | 173 | 33 / 59 | 173 | 14.1 |
| Mobile 6M/50ms±10 0.5%loss | box | 11 | 144 / 2910 | 99 | 79 / 679 | 61 | 3.4 |
| Mobile 6M/50ms±10 0.5%loss | scroll | 1 | 2231 / 5889 | 154 | 72 / 998 | 57 | 4.8 |
| Mobile 6M/50ms±10 0.5%loss | full | 21 | 108 / 246 | 52 | 73 / 241 | 52 | 4.1 |
| LAN (Docker) | idle | 288 | 2 / 10 | 289 | 2 / 20 | 0 | 11.9 |
| LAN (Docker) | box | 237 | 6 / 13 | 257 | 3 / 18 | 0 | 30.0 |
| LAN (Docker) | scroll | 179 | 10 / 18 | 181 | 9 / 17 | 181 | 5.4 |
| LAN (Docker) | full | 201 | 8 / 14 | 209 | 7 / 14 | 209 | 14.7 |

Reading the numbers:
- With a fixed window of 2 unacknowledged frames, frame rate was bound by the
  round trip (≈2 / RTT: ~30 fps over Funnel) and large lossless updates
  queued for seconds (scroll). Now small frames go out at screen rate and
  content is sized to the measured link.
- The mobile profile's p99 is TCP loss recovery (0.5 % random loss with
  reordering): retransmissions stall everything behind them. The WebRTC
  data channel does not avoid it (see below): its SCTP recovers worse.
- LAN is unchanged at p50; box became lossless because 30 Mbit/s of moving
  pixels is affordable on an unproven (fast) link, which is also faster.

## WebRTC data channel vs WebSocket

Same Linux bench container and harness (`total` = capture→draw p50 / p99,
input = input round trip). The data channel (str0m, reliable unordered,
60 KiB chunks) forced with `?transport=webrtc`, on the same path as the
WebSocket:

| link | scenario | WS fps | WS total | DC fps | DC total |
|---|---|---|---|---|---|
| LAN (Docker) | box | 253 | 4 / 25 | 283 | 3 / 8 |
| LAN (Docker) | full | 199 | 9 / 23 | 199 | 9 / 31 |
| 18M/40ms | box | 186 | 27 / 85 | 188 | 27 / 85 |
| 18M/40ms | full | 173 | 32 / 41 | 170 | 33 / 74 |
| 50M/20ms 1 % loss | box | 101 | 28 / 185 | 139¹ | 17 / 986 |
| 50M/20ms 1 % loss | full | 88 | 43 / 100 | 53¹ | 42 / 134 |
| Mobile 6M/50ms±10 0.5 % loss | box | 89 | 97 / 344 | 49 | 103 / 832 |
| Mobile 6M/50ms±10 0.5 % loss | full | 52 | 81 / 180 | 47 | 81 / 781 |

¹ stalled and fell back to the WebSocket during the run.

Where the WebSocket is relayed (TCP egress delayed 30 ms more than UDP,
like Funnel or a reverse proxy in front of a direct UDP path), the default
automatic choice moves to the data channel:

| scenario | path | fps | total | input RTT |
|---|---|---|---|---|
| box | WebSocket (`?transport=ws`) | 188 | 32 / 35 | 58 / 64 |
| box | auto → data channel | 200 | 16 / 28 | 27 / 39 |
| full | auto → data channel | 177 | 22 / 42 | 32 / 54 |

UDP cut mid-session (100 % loss on UDP while in use): back on the
WebSocket after ~1.3 s, 6–7 frames repaired, no artifacts. UDP blocked from
the start: ICE fails, WebSocket as before (191 fps box).

Reading the numbers:
- Over the same path the data channel is no better without loss and worse
  with it: SCTP's Reno-like congestion control (1.1 KB packets, 1 s minimum
  RTO fixed by str0m) caps throughput at ≈5 Mbit/s with 1 % loss at 20 ms,
  and tail losses stall for a second. Hence the automatic choice uses it
  only when it is clearly shorter, and abandons it on stalls.
- Unreliable delivery (`maxRetransmits: 0`) with per-packet chunks and
  client NACKs was worse still (0 fps at 1 % loss): abandoned packets still
  shrink SCTP's congestion window, and every given-up frame triggered a
  state repair plus keyframe that loaded the link further.

## Stutters: link model with light content, data-channel fallback

Found on a real direct session (6 ms, Wi-Fi jitter, 2-3 Mbit/s of desktop
content): the link estimate fell from 77 to 10 Mbit/s in 35 s and the video
cap with it (42 -> 6.7 Mbit/s at 3168x1440, four cap changes in 40 s, each a
keyframe). With one or two ACKs per judging window the queue signal was
mostly jitter, and nothing raised the estimate again while content stayed
below it. Pacer simulator, 3 Mbit/s of content on a 100 Mbit/s, 6 ms link,
estimate starting at 80 Mbit/s:

| ACK jitter | before, after 30 s | before, after 60 s | now, after 60 s |
|---|---|---|---|
| 20 ms | 56.5 | 37.2 | 80 |
| 30 ms | 9.9 | 9.4 | 80 |
| 60 ms | | | 55 (7 without waiting for more ACKs in sparse windows) |

The same content on a link that really carries 2 Mbit/s still brings the
estimate down (below 4 Mbit/s within 10 s, queue around 100 ms).

Data channel in trouble (Docker, Xvfb 1920x1080, WebSocket relayed: +30 ms
on TCP; `late` = longest capture->draw the client saw):

| scenario | before | now |
|---|---|---|
| UDP cut during full-screen video, 20 Mbit/s | frozen ~2.5 s | late 0.6 s, one keyframe |
| UDP cut during scroll, 20 Mbit/s | frozen ~2 s | late 0.6 s |
| UDP with 1 % loss, full: fps / Mbit/s / total | 50 / 4.0 / 54 / 193 (stays on the data channel; measured without the server-side rule) | 159 / 13.3 / 50 / 98 (leaves it after 2.3 s) |
| data channel forced on a 1 % loss link, box: total | 17 / 986 | 16 / 104 |
| data channel forced on a 1 % loss link, full: fps / total | 53 / 42 / 134 | 79 / 50 / 151 |
| clean UDP (no loss), box / full: total | 16 / 28, 22 / 42 | 17 / 33, 21 / 47 |

Single-path profiles (WebSocket) are unchanged or better: LAN 264 / 175 /
201 fps (box / scroll / full), 18M/40ms 206 / 163 / 173, WAN 50M/20ms scroll
20 / 31 and full 19 / 24 (were 20 / 76 and 20 / 77), Wi-Fi 50M/10ms +-3 with
2 % loss scroll 20 / 77 (was 22-30 / 245-309), mobile 6M/50ms +-10 with
0.5 % loss 92 / 150 / 53 fps (were 99 / 154 / 52).

## Start with little traffic

A client used to start at the configured bandwidth: the first full screen
went out as fast as a LAN takes it (a keyframe of about 500 KB, 0.7 s of a
6 Mbit/s link by itself). Now it starts at 5 Mbit/s and the estimate
follows what the link delivers (see ARCHITECTURE, link model). Same bench
container and profiles as above, 10 s runs, capture→draw p50 / p99:

| profile | first keyframe | estimate in the first 1.3 s | box | scroll | full |
|---|---|---|---|---|---|
| LAN | 69 KB at 0.25 s | video cap 7 → 58-123 Mbit/s at 0.5 s | 286 fps, 3 / 7 | 174 fps, 9 / 32 | 195 fps, 8 / 35 |
| WAN 50M/20ms | 69 KB at 0.2 s | cap 7 → 34-39 at 0.5 s, → 105-109 at 1.0 s | 194 fps, 14 / 47 | 171 fps, 20 / 25 | 180 fps, 19 / 36 |
| 18M/40ms | 69 KB at 0.2 s | cap 7 → 19-21 at 0.5 s; first cut at 22.5 Mbit/s (queue 37 ms) | 205 fps, 26 / 80 | 168 fps, 32 / 108 | 172 fps, 34 / 57 |
| mobile 6M/50ms +-10, 0.5 % loss | 69 KB at 0.2 s | no rise; first cut at 6.6-7.2 Mbit/s (queue 37-43 ms) | 169 fps, 45 / 145 | 121 fps, 66 / 218 | 53 fps, 82 / 151 |
| Wi-Fi 50M/10ms +-3, 2 % loss | 69 KB at 0.2 s | cap 7 → 31-33 at 0.5 s; cuts to 16-27 after | 189 fps, 13 / 74 | 140 fps, 26 / 105 | 96 fps, 38 / 146 |

Doubling the estimate every window without the bound to what was delivered
reached 270 Mbit/s on the 18 Mbit/s link before the first late ACK.

## Estimate without bound

The Windows demo once reported 3.2 million Mbit/s on a 4 ms Wi-Fi path:
waiting for the configured bandwidth counted as waiting for the estimate,
and light content never showed a queue. The pacer simulator
(`light_content_flagged_limited_does_not_inflate_the_estimate`: 3 KB at
60 fps on a 100 Mbit/s, 4 ms link, every frame flagged) reached 9.9e15
Mbit/s in 60 s before the fix and stays under the link after it. Bench
container, LAN, `box` at 60 fps for 30 s: 478 Mbit/s before, 1288 with
the first fix alone (samples over microseconds), 232 with the sample
interval floor. Mobile `box` (6M/50ms +-10, 0.5 % loss, three runs each,
WebSocket): 114 fps, 53 / 190 before today's changes; 103 fps, 62 / 200
after (runs spread 85-157 fps either way).

## Motion mode

Linux bench container, `rt-testapp --mode full` at 165 fps, one client
over the WebSocket (140 fps, 12 Mbit/s before and after the switch):

| event | observed |
|---|---|
| screen moving as a whole | mode on 10 s after the movement began |
| movement stops (`SIGSTOP`) | all 510 tiles still video at 4.5 s, all exact at 6.0 s |
| movement resumes for 1.5 s | every tile video again, mode still on |
| movement stops for good | mode still on at 39 s, off at 42 s |

Size and frame rate per client, same container (`full`), a client reporting
its view over the WebSocket:

| client | observed |
|---|---|
| 1920x1080 at 60 / 100 / 120 / 144 Hz, display capturing at 165 Hz | 60 / 100 / 120 / 144 video frames per second |
| no report | 165 |
| 800x450 at 144 Hz connecting in motion mode | 800x450 at once (slow start: cap 7 Mbit/s), full size within a second, 144 fps |
| 640x360 at 165 Hz, link 12 Mbit/s | 640x360, then 960x540 once the cap reached 12 Mbit/s; 127 fps |
| 800x450 browser window, link squeezed from LAN to 4 Mbit/s | 1920x1080 → 1280x720 → 960x540 as the cap fell |

GPU scaling on Windows (`d3d11_bench --scale 0.5`, plasma, 3168x1440 →
1584x720, RTX 4080 SUPER): encode with the video processor's scaling
1.33 / 3.76 / 11.26 ms (p50 / p90 / p99), 7.8 KiB per frame; unscaled
3.33 / 6.04 / 12.98 ms, 21.2 KiB. The system-memory path scaling on the
CPU: 11.94 / 17.01 / 18.45 ms. The scaled picture was not checked by
decoding (the bench compares only same-size pictures).

## Host under load (CPU and GPU busy)

2026-11-10, this PC (24 logical CPUs, RTX 4080 SUPER, HAGS off, a game
client open: GPU 20-35 % busy before any test), server not elevated. CPU
load: 48 processes spinning at normal priority (24 where noted). GPU load:
a heavy fragment shader drawn into a 4096x4096 target in a headless
browser, a few frames queued at a time as an uncapped game would (93-95 %
utilization next to the encoder bench, 75-88 % next to the server).
Priorities were set from outside the process (process class,
`D3DKMTSetProcessSchedulingPriorityClass`); what the server now sets by
itself (ARCHITECTURE, "Scheduling") was built from these numbers.

Encoder alone (`d3d11_bench --size 3168x1440 --enc nvenc-gpu --fps 144`,
1440 frames), encode ms p50 / p90 / p99:

| condition | `box` | `plasma` (whole screen) |
|---|---|---|
| as is (two runs) | 2.8-3.1 / 3.8 / 5.0-5.2 | 4.0-4.1 / 5.0-5.2 / 6.2-6.5 |
| CPU: 48 busy | 3.1 / 4.1 / 123 | 4.1 / 5.1 / 57.7 |
| CPU: 48 busy, high process class | 3.1 / 4.0 / 5.6 | 4.1 / 5.2 / 6.5 |
| GPU: 93-95 % busy (two runs) | 6.0-6.2 / 8.9-9.8 / 16.0-16.3 | 6.4-6.5 / 9.9-10.3 / 16.9-17.6 |
| GPU busy, `SetGPUThreadPriority(7)` | 6.1-6.2 / 9.5-9.7 / 17.2-17.8 | 6.3-6.4 / 9.6 / 16.1-16.6 |
| GPU busy, GPU class high | 6.2 / 9.8 / 17.7 | 6.6 / 10.3 / 17.2 |
| GPU busy, GPU class realtime | 2.9 / 3.9 / 4.4 | 3.5 / 4.5 / 4.8 |
| GPU busy, high process class (CPU) | 7.5 / 12.3 / 16.8 | 7.3 / 12.7 / 17.4 |

Server alone on Windows (3168x1440, `rt-testapp` at 144 fps, a client that
acknowledges every frame at once; 12 s per row), `capture→sent` ms p50 /
p99 / max, stalls = gaps over 50 ms:

| scenario | condition | fps | capture→sent | stalls |
|---|---|---|---|---|
| `full` | as is | 120 | 4.8 / 9.2 / 11 | 4 |
| `full` | CPU: 48 busy | 87 | 4.8 / 11.6 / 132 | 18 |
| `full` | CPU: 48 busy, high process class | 103 | 4.8 / 9.0 / 41 | 2 |
| `full` | GPU busy | 94 | 5.0 / 17.9 / 32 | 2 |
| `full` | GPU busy, GPU class realtime | 94 | 4.3 / 6.4 / 47 | 5 |
| `box` | as is | 139 | 1.8 / 4.1 / 8 | 2 |
| `box` | CPU: 48 busy | 100 | 1.8 / 71.9 / 142 | 49 |
| `box` | CPU: 48 busy, high process class | 133 | 1.8 / 4.2 / 8 | 1 |
| `box` | GPU busy | 117 | 2.0 / 13.5 / 49 | 2 |
| `box` | GPU busy, GPU class realtime | 122 | 1.5 / 3.6 / 9 | 5 |

Linux (Docker on WSL2, Xvfb 1920x1080 at 165 fps, load = 64 busy loops in
another container): `full` fell from 165 to 88 fps (p99 55.7 ms), and the
server's threads at nice −10 only brought 99 fps (p99 46.0 ms); the kernel
shares CPU between containers before it looks at nice. A native Linux host
was not tested.

A busy CPU stalls the server for 70-140 ms, which the high process class
removes; a busy GPU triples the encode p99, which only the realtime GPU
scheduling class undoes (thread priority 7 and the high class do nothing).
No sign that the GPU clocks down with an encoder open (encode 3.2-3.5 ms
p50 from 5 to 144 fps).

## A game at 4K for a 120 Hz client

Windows, RTX 4080 SUPER, HAGS off, 2026-11-11: ARC Raiders in the
foreground on the 3840x2160 display (165 Hz), GPU 94-99 % busy, the server
in the realtime GPU class (read back from outside; the game's is normal).
A private instance beside the demo, so two servers encoded the display. The
client is a script on the same machine over the WebSocket: it reports a
3840x2160 view at 120 Hz and holds each video frame for 1 ms + 0.026 ms
per KB before the ACK, which is what the user's laptop reported as decode
time in this game (about 3 ms with 90 KB frames, 8.4 ms with 290 KB). AV1,
video cap 140 Mbit/s, motion mode on, 10 s per run, builds alternating:

| build | fps | Mbit/s | frame KB | srv wait | encode | server | client | capture→ACK |
|---|---|---|---|---|---|---|---|---|
| before | 59 | 133 | 279 / 363 | 9.6 / 18.7 | 6.3 / 12.3 | 14.9 / 26.5 | 9.1 / 14.5 | 25.0 / 42.8 |
| cap for the client's 120 frames | 71 | 81 | 141 / 161 | 0.2 / 10.7 | 9.3 / 13.6 | 10.6 / 16.4 | 5.9 / 7.0 | 16.3 / 21.8 |
| before | 62 | 79 | 251 / 369 | 7.4 / 18.6 | 6.6 / 17.8 | 13.4 / 30.8 | 8.0 / 15.3 | 20.3 / 41.5 |
| cap for the client's 120 frames | 66 | 70 | 141 / 203 | 0.2 / 13.3 | 8.4 / 14.5 | 11.4 / 19.2 | 5.9 / 7.2 | 17.2 / 25.3 |

Before, NVENC made every frame as large as a sixtieth of the cap allows
(291 KB) and pacing let 60 of them through a second: the wait is frames
waiting for their turn. The scenes differ between runs (the user was
playing), so only what repeats in both pairs counts. Not measured: the
laptop itself.

What is left on the server is the GPU. Encode takes 6-9 ms here and
2.7-3.0 ms with the same game in the background (GPU 35-54 % busy, desktop
content, so not the same pictures). How long the GPU takes to get to a
small job next to the game, by the job's GPU scheduling class: a 256x256
texture copy, flushed and waited for with a fence, 300 times per run
(p50 / p90 / p99 ms, range over the runs):

| class | runs | p50 | p90 | p99 |
|---|---|---|---|---|
| normal | 3 | 5.5-11.0 | 10.5-33.7 | 30-134 |
| high | 1 | 4.4 | 8.6 | 23.5 |
| realtime | 4 | 3.0-4.7 | 4.4-12.0 | 7.8-20.2 |
| realtime, `SetGPUThreadPriority(7)` | 1 | 2.9 | 4.3 | 7.8 |

A 3840x2160 copy takes the same (realtime 3.2 / 5.1 / 8.2, normal 5.1 /
10.4 / 26.2): it is the wait for the GPU, not the work. Every step of the
frame path that waits for the GPU (tile comparison, readback, encode) pays
it. The realtime class halves the median and cuts the tail to a sixth; it
does not bring back the idle values as it did next to the synthetic load
("Host under load"). The same probe with an idle GPU was not run (the game stayed
open).

## WebTransport datagrams vs WebSocket

Linux bench container, `rt-testapp` at 165 fps, the same link shaping on
all traffic, 10 s runs; `total` = capture→draw p50 / p99 / max:

| profile | scenario | WebTransport | WebSocket |
|---|---|---|---|
| clean | full | 184 fps, 10 / 20 | (195 fps, 8 / 35 above) |
| Wi-Fi 50M/10ms +-3, 2 % loss | full | 161 fps, 20 / 45 / 77 | 96 fps, 41 / 132 / 183 |
| Wi-Fi 50M/10ms +-3, 2 % loss | scroll | 159 fps, 19 / 44 / 64 | 113 fps, 27 / 163 / 558 |
| 50M/4ms, 0.5 % loss | full | 176 fps, 14 / 30 / 35 | 177 fps, 13 / 25 / 31 |
| 50M/4ms, 0.5 % loss | scroll | 165 fps, 15 / 27 / 34 | 170 fps, 13 / 24 / 36 |
| mobile 6M/50ms +-10, 0.5 % loss | full | 53-57 fps, 72-75 / 148-153 | 63 fps, 68 / 137 |
| mobile 6M/50ms +-10, 0.5 % loss | scroll | 141 fps, 65 / 173 / 315 | 118 fps, 70 / 259 / 548 |

Where loss meets a round trip TCP takes time to recover (Wi-Fi), the
datagrams keep the frame rate and cut the worst frames to a third or less.
On a 4 ms path TCP recovers fast and both are the same. On the slow mobile
link the link model, not the transport, is the limit.

Failure cases (`transport=auto`):

| case | observed |
|---|---|
| all UDP dropped from the start | WebTransport failed on every candidate within 3 s, frames on the WebSocket throughout: 185 fps, 9 / 18 |
| all UDP dropped 5 s into the session | given up after 300 ms without a ping answer, 3 frames repaired with a keyframe, one frame 431 ms late, then WebSocket at full rate |

## NVENC rate control trial (one-frame VBV not adopted; two-pass, see below)

Linux bench container, `rt-testapp` at 165 fps, `transport=auto`, 10 s
runs; `total` = capture→draw p50 / p99 / max, encode p50 / p99. Current:
capped VBR, VBV of two frames at 60 fps. Trial: what Sunshine uses, a VBV
of one frame plus two-pass at quarter resolution (`multipass=qres`), then
the one-frame VBV alone.

| profile | scenario | current | one-frame VBV + qres | one-frame VBV only |
|---|---|---|---|---|
| 18M/40ms (3 runs) | full | 157-160 fps, 42-44 / 54-64 / 77-110 | 155-158 fps, 40-42 / 50-55 / 55-61 | 157-159 fps, 42-44 / 54-71 / 88-100 |
| mobile 6M/50ms +-10, 0.5 % loss (3 runs) | full | 51-52 fps, 78-82 / 153-158 / 175-195 | 59-63 fps, 75-77 / 141-158 / 199-208 | 43-50 fps, 80-86 / 140-208 / 167-240 |
| clean | full | 183 fps, 9 / 18 / 21, encode 2.3 / 4.5 | 157 fps, 14 / 21 / 24, encode 3.0 / 8.5 | 185 fps, 9 / 18 / 23, encode 2.3 / 4.5 |
| 50M/20ms | scroll | 170 fps, 22 / 31 / 40 | 157 fps, 26 / 36 / 50 | 167 fps, 22 / 31 / 37 |
| Wi-Fi 50M/10ms +-3, 2 % loss | full | 160 fps, 18 / 49 / 65 | 147 fps, 22 / 46 / 53 | not run |

The one-frame VBV alone changes nothing (worse on the mobile profile). The
two-pass gives tighter frames on slow links (worst frame at 18 Mbit/s from
77-110 to 55-61 ms, +20 % frame rate on the mobile profile) but costs
0.7 ms of encode at the median and 4 ms at p99, which the fast links pay in
frame rate and latency. Kept the one-frame VBV out; two-pass became
automatic, next section.

## Two-pass on slow links

Same container and harness, 10 s runs, `total` = capture→draw p50 / p99 /
max. First two-pass (`multipass=qres`) always on, per codec, against the
current single pass (2 runs each; H.264 on slow links 3 more):

| profile | scenario | codec | single pass | two-pass always |
|---|---|---|---|---|
| clean | full | H.264 | 179 fps, 11 / 22-23 / 28-29 | 144-154 fps, 14-16 / 23-34 / 28-50 |
| clean | full | HEVC | 163 fps, 9-11 / 20-24 / 27-42 | 149-156 fps, 13-14 / 23-26 / 28-37 |
| clean | full | AV1 | 176-179 fps, 11-12 / 22-24 / 28-29 | 148 fps, 15-16 / 28 / 31-35 |
| 18M/40ms | full | H.264 (5 runs) | 151-158 fps, 42-45 / 57-67 / 78-112 | 144-155 fps, 41-43 / 52-62 / 56-78 |
| 18M/40ms | full | HEVC | 148-150 fps, 46 / 60-71 / 74-123 | 146-150 fps, 44-45 / 57-69 / 68-137 |
| 18M/40ms | full | AV1 | 170 fps, 32-33 / 46-47 / 56-63 | 161-163 fps, 35 / 43-44 / 48-49 |
| mobile 6M/50ms +-10, 0.5 % loss | full | H.264 (5 runs) | 52-56 fps, 74-79 / 129-295 / 170-357 | 56-64 fps, 72-84 / 139-174 / 168-207 |
| mobile 6M/50ms +-10, 0.5 % loss | full | HEVC | 51 fps, 80-81 / 136-200 | 54-57 fps, 78-79 / 174-187 |
| mobile 6M/50ms +-10, 0.5 % loss | full | AV1 | 79-82 fps, 68-69 / 153-161 | 82-97 fps, 67-68 / 155-165 |
| 10M/30ms | full | H.264 (3 runs) | 75 fps, 68-71 / 133-147 | 73-75 fps, 67-72 / 131-139 |
| 10M/30ms | scroll | H.264 (3 runs) | 122-153 fps, 40-52 / 109-151 / 180-222 | 132-140 fps, 41-42 / 94-102 / 128-159 |
| 50M/20ms | scroll | H.264 | 153-165 fps, 23 / 38-81 / 89-140 | 148-158 fps, 26 / 37-42 / 43-60 |

Encode time with two-pass: 2.6-3.4 ms p50, 8-9 ms p99 (single pass 1.9-2.5
/ 4.1-4.7). For H.264 the even frame sizes pay on slow or distant links;
for HEVC and AV1 the differences stay within the spread between runs, so
they keep a single pass. On a fast near link every codec loses 10-20 % of
its frame rate.

The 18 Mbit/s link is not filled by this video (13 Mbit/s): the link
model never finds its ceiling (estimate 44 Mbit/s after 60 s), so the
video cap stays above 30 Mbit/s. Hence two-pass follows the round trip
as well as the cap. Opening the other NVENC encoder takes about 250 ms
(a gap of 254 ms in frames when it was opened on the client's thread),
so it opens beside the current one.

Automatic (H.264, on below 20 Mbit/s of cap or from 50 ms of round trip,
3 runs; 50M/20ms 2 runs), against the single pass above:

| profile | scenario | automatic | used |
|---|---|---|---|
| clean | full | 182-185 fps, 10-11 / 19-22 / 23-28 | two-pass for the first second (slow start), then single |
| 18M/40ms | full | 153-158 fps, 41 / 52-55 / 56-62 | two-pass (80 ms round trip) |
| mobile 6M/50ms +-10, 0.5 % loss | full | 60-80 fps, 67-72 / 156-179 / 183-268 | two-pass |
| 10M/30ms | scroll | 155-159 fps, 36 / 89-97 / 121-169 | two-pass |
| 50M/20ms | scroll | 164-165 fps, 23 / 33-35 / 40-44 | single after slow start |

## Video codecs

**Quality per byte.** Five seconds of `rt-testapp` recorded losslessly in
the container (1920x1080, 60 fps), encoded with our NVENC settings at
constant QP; PSNR (Y) and SSIM against the recording:

| codec, QP | `full`: KiB/frame, PSNR, SSIM | `scroll`: KiB/frame, PSNR, SSIM |
|---|---|---|
| H.264 20 | 10, 52.6 dB, 0.9945 | 8, 46.9 dB, 0.99955 |
| H.264 24 | 6, 51.8 dB, 0.9929 | 6, 43.3 dB, 0.99915 |
| HEVC 20 | 10, 52.2 dB, 0.9945 | 7, 48.8 dB, 0.99953 |
| HEVC 24 | 6, 51.3 dB, 0.9929 | 6, 46.2 dB, 0.99929 |
| AV1 60 | 4, 52.0 dB, 0.9945 | 5, 47.4 dB, 0.99934 |
| AV1 80 | 3, 51.5 dB, 0.9941 | 4, 44.5 dB, 0.99847 |
| AV1 100 | 2, 50.8 dB, 0.9937 | 4, 42.2 dB, 0.99738 |

AV1's QP floor is 60 (H.264 and HEVC: 20).

**Streaming.** Linux bench container (NVENC through CUDA), `rt-testapp` at
165 fps, `transport=auto`, the client restricted with `?codec=`, 10 s runs;
`total` = capture→draw p50 / p99 / max, decode = receive → picture p50 /
p99, encode p50 / p99:

| profile | scenario | H.264 | HEVC | AV1 |
|---|---|---|---|---|
| clean (2 runs) | full | 179-181 fps, 11 / 20-22 / 24-39, 9.9 KB/frame, decode 0.4 / 2.3-2.8, encode 2.4 / 4.5 | 164-165 fps, 9-10 / 17-18 / 20-23, 11.0 KB, decode 0.5-0.6 / 1.1-1.3, encode 2.0 / 4.1-4.3 | 178-191 fps, 9-10 / 19-20 / 22-23, 4.4-4.7 KB, decode 0.4 / 1.4-2.2, encode 1.9-2.0 / 4.1 |
| 18M/40ms (2 runs) | full | 158-160 fps, 42-43 / 56-58 / 78-79 | 149-151 fps, 46-47 / 62-69 / 102-106 | 173-175 fps, 31-32 / 40 / 43-45 |
| mobile 6M/50ms +-10, 0.5 % loss (2 runs) | full | 50-51 fps, 79-83 / 138-165 / 156-199 | 51-53 fps, 78-85 / 152-172 / 184-296 | 80-81 fps, 67 / 151-156 / 163-222 |
| 50M/20ms | scroll | 164 fps, 23 / 34 / 37, 4.0 KB | 163 fps, 23 / 32 / 41, 3.4 KB | 168 fps, 22 / 32 / 37, 1.8 KB |
| mobile 6M/50ms +-10, 0.5 % loss | scroll | 151 fps, 63 / 191 / 328 | 147 fps, 63 / 222 / 293 | 164 fps, 57 / 297 / 399 |

AV1 sends half the bytes or less at the same quality: on the 18 Mbit/s
link the median drops by 11 ms and the worst frame by half, on the mobile
profile the frame rate rises by 60 %. The decode medians stay far below
a frame interval for all three (no picture held back for reordering). The
single mobile `scroll` run per codec is within that profile's spread seen
before (p99 141-315 ms).

**Split-frame encoding** (Windows, `examples/d3d11_bench`, D3D11 input,
RTX 4080 SUPER, 800 frames; encode p50 / p99 ms, KiB/frame):

| codec | size | scene | driver's choice (`auto`) | forced |
|---|---|---|---|---|
| AV1 | 3840x2160 | plasma | 2.45 / 10.38, 17.6 (`disabled`: 4.16 / 12.08) | 2.46 / 10.13, 17.6 |
| AV1 | 3168x1440 | plasma | 2.59 / 10.34, 9.5 | 1.58 / 7.88, 9.7 |
| AV1 | 3168x1440 | window | 2.55 / 10.80, 14.5 | 1.62 / 9.30, 16.3 |
| AV1 | 1920x1080 | plasma | 1.34 / 8.19, 4.5 | 0.91 / 6.69, 4.6 |
| AV1 | 1920x1080 | window | 1.46 / 9.75, 14.9 | 1.01 / 8.62, 16.9 |
| HEVC | 3840x2160 | plasma | 3.01 / 11.65, 40.5 (`disabled`: 4.82 / 11.72) | 3.00 / 9.67, 40.5 |
| HEVC | 3168x1440 | plasma | 2.66 / 11.07, 21.5 | 1.67 / 8.81, 21.7 |
| HEVC | 3168x1440 | window | 2.51 / 10.11, 27.4 | 1.61 / 9.54, 28.7 |
| HEVC | 1920x1080 | plasma | 1.42 / 8.54, 10.1 | 0.97 / 7.69, 10.2 |
| HEVC | 1920x1080 | window | 1.46 / 10.12, 25.8 | 1.03 / 8.36, 26.9 |
| H.264 (no split) | 3168x1440 | plasma | 3.39 / 10.88, 21.2 | |

Streams of both codecs dumped from the Windows encoder (`--dump`, split
forced) decode without errors (200/200 frames, dav1d and FFmpeg's HEVC
decoder). Decoded and compared with the source, HEVC (split disabled or
the driver's choice) has the MAE of H.264 (1.4-1.8).

## NVENC through the SDK

Windows, RTX 4080 SUPER, driver 610.88 (NVENC API 13.1),
`examples/d3d11_bench`, D3D11 input. `sdk` = NVENC driven directly
(`crates/encoder/src/nvenc/`), `ffmpeg` = FFmpeg's NVENC with the same
input (`--enc nvenc-ffmpeg`). The GPU also drew this PC's live desktop
during the runs, so encode times scatter by a few tenths of a ms at the
median and much more at p99 from run to run.

**Same encoder.** 600 frames per case, every scene at 1920x1080 and
3168x1440, all three codecs (60 cases): the decoded MAE is the same to the
hundredth in every case, KiB/frame within 5 % except H.264 on `noise`
(378 against 334 KiB at 1080p, 395 against 376 at 1440p), encode p50
within the run-to-run spread (1080p `plasma`: H.264 2.23 / 2.06 ms, HEVC
1.22 / 1.35, AV1 1.11 / 1.18). Both declare the same level (the driver's
choice at the 200 Mbit/s cap, parsed from the streams): H.264 5.0 at 1080p,
5.2 at 4K; HEVC 5.2 High tier; AV1 31 at 1080p, 6.1 High tier at 4K.

**Bitrate changes** (`--rate-steps 25`, 500 frames of `noise` at 1080p, the
cap cycling 5, 12, 30, 80, 20 Mbit/s; KiB/frame under each cap, the budget
at 60 fps being 10.4, 25.0, 41.7, 62.5, 166.7):

| codec | path | keyframes after the first (19 changes) | KiB/frame at 5 / 12 / 20 / 30 / 80 Mbit/s |
|---|---|---|---|
| H.264 | sdk | 0 | 9.3 / 24.0 / 36.3 / 61.4 / 155.9 |
| H.264 | ffmpeg | 19 | 10.1 / 17.4 / 26.1 / 39.5 / 103.2 |
| HEVC | sdk | 0 | 9.3 / 24.5 / 38.3 / 56.9 / 150.2 |
| HEVC | ffmpeg | 19 | 10.2 / 23.0 / 35.9 / 52.1 / 124.6 |
| AV1 | sdk | 0 | 9.8 / 24.3 / 39.5 / 62.2 / 162.0 |
| AV1 | ffmpeg | 19 | 12.7 / 30.5 / 50.6 / 72.9 / 175.3 |

The new cap holds from the next frame; FFmpeg's restarts cost a keyframe
each (and the rate control's state). Same at 4K `plasma` (11 changes, no
keyframe after the first for any codec).

**Losses** (`--lose N`: N frames dropped from the decoder's input once a
period, then `invalidate_from`; a second decoder gets every frame and every
picture the client's decoder shows is compared with that loss-free
decode; acknowledgements 8 frames late). In every run below each picture
the client showed after a recovery was identical to the loss-free one
(0 differing) and the decoders refused nothing. H.264 and HEVC decoded by
FFmpeg 9 in the bench, AV1 by NVDEC (`av1_cuvid`) in the bench and, for
the dumped streams marked †, by dav1d 1.4 in an Ubuntu 24.04 container
(every picture identical to the loss-free decode's).

| scene, size | codec | lost frames per loss | losses | recovered without keyframe | recovery / keyframe / P frame KiB (p50) | ffmpeg: keyframes |
|---|---|---|---|---|---|---|
| plasma 1080p | H.264 | 1 / 3 / 6 / 30 / 100 | 14 / 14 / 14 / 11 / 5 | 14 / 13 / 13 / 8 / 2 | 1.6-12.7 / 10.0 / 9.6 | |
| plasma 1080p | HEVC | 1 / 3 / 6 / 30 / 100 | 14 / 14 / 14 / 11 / 5 | all | 2.8-22.2 / 23.5 / 10.3 | |
| plasma 1080p | AV1 | 1 / 3 / 6 / 30 / 100 † | 14 / 14 / 14 / 11 / 5 | all | 1.0-10.0 / 17.9 / 4.7 | |
| plasma 4K | H.264 | 6 / 30 | 9 / 7 | 8 / 5 | 50 / 39 / 38 | |
| plasma 4K | HEVC | 6 / 30 | 9 / 7 | all | 70-86 / 91 / 41 | |
| plasma 4K | AV1 | 6 † / 30 | 9 / 7 | all | 29-36 / 69 / 18 | |
| box 1080p | H.264 | 3 / 30 | 9 / 7 | 8 / 5 | 3.1-27.5 / 714 / 0.9 | 9 / 7, 690 KiB each |
| box 1080p | HEVC | 3 / 30 | 9 / 7 | all | 3.5-32.2 / 766 / 1.6 | 9 / 7, 697 KiB each |
| box 1080p | AV1 | 3 / 30 | 9 / 7 | all | 3.1-27.1 / 930 / 0.8 | 9 / 7, 823-829 KiB each |

On mostly still content (`box`: text with a moving square) a keyframe is
690-930 KiB against a few KiB for a P frame, and a recovery costs 3-32 KiB:
the stream averaged 2.4-5.7 KiB/frame instead of FFmpeg's 12.6-16.7. On
`plasma`, where every frame changes everything, P frames are as large as
keyframes and the difference is the keyframe the decoder no longer waits
for. Losses of 1-3 frames are predicted past from short-term references,
longer ones from an anchor (four short-term references at 1080p, three at
4K, before the anchors are needed).

The H.264 keyframes are the `frame_num` rule: NVENC's H.264 counts
`frame_num` in 256 values, and a loss covering a frame where it wraps is
answered with a keyframe. Without that rule (`--lose 30`, 900 frames, 11
losses) all 11 were predicted past and FFmpeg 9's decoder showed 435
pictures instead of 570: it dropped 135 after the three gaps across a
wrap, though every picture it did show was right.

Every frame still in the decoder's buffer has to be forgotten, not only
the newest four: first forgetting only those, HEVC losses of 6 frames left
older lost frames in the reference picture sets, and FFmpeg 9's HEVC
decoder refused every frame after the first such recovery ("Error
constructing the frame RPS"; FFmpeg 6.1 decoded the same stream).

**Linux** (Ubuntu 24.04 container with `--gpus all`, same GPU and driver,
`examples/nvenc_check`: 400 frames of `plasma` at 1080p loaded from system
memory into the CUDA surface, decoded by the container's FFmpeg 6.1, AV1
with its libdav1d): all three codecs open as `nvenc … (cuda, sdk)`, MAE
1.68 / 1.44 / 1.42 (H.264 / HEVC / AV1). 15 cap changes: no keyframe after
the first (FFmpeg's NVENC: 15 each). `--lose 3`: 6 of 6 losses recovered
without a keyframe for each codec; `--lose 30`: 4 of 4 for HEVC and AV1,
3 of 4 for H.264 (the `frame_num` rule); every picture the client showed
identical to the loss-free decode.

**Areas the client does not show** (`--unseen`: each case twice, the
second with the left half of the picture unseen; 300 frames, one keyframe
each; KiB/frame and the share saved):

| codec | 1080p plasma | 1080p scroll | 1080p window | 4K plasma | 4K scroll | 4K window |
|---|---|---|---|---|---|---|
| H.264 | 9.4 → 5.1, 46 % | 9.6 → 6.2, 35 % | 20.2 → 8.9, 56 % | 37.3 → 20.2, 46 % | 23.4 → 13.3, 43 % | 19.8 → 5.2, 74 % |
| HEVC | 10.2 → 5.8, 43 % | 9.0 → 5.7, 37 % | 26.3 → 10.6, 60 % | 40.3 → 22.6, 44 % | 21.8 → 13.1, 40 % | 28.9 → 5.9, 79 % |
| AV1 | 4.6 → 2.8, 40 % | 6.1 → 4.2, 31 % | 16.3 → 6.5, 60 % | 17.5 → 10.6, 39 % | 17.1 → 11.7, 31 % | 15.6 → 3.3, 79 % |

Encode time is unchanged (the QP delta map is on from the start; with no
area unseen it costs nothing measurable, see "Same encoder"). On `window`
the moving window spends part of its time in the unseen half, so more
than half is saved there.

**Presets** (`--set preset=p1|p2|p3`; encode p50 / p99 ms from 600 frames
without decoding, PSNR-Y dB / SSIM-Y from 300 frames decoded and compared
with the source, `plasma` / `scroll`):

| codec, size | P1 | P2 | P3 |
|---|---|---|---|
| H.264 1080p | 1.83 / 5.06, 2.27 / 14.93; 52.57 / 0.9955, 42.86 / 0.9985 | 1.92 / 7.37, 2.30 / 12.51; 52.57 / 0.9955, 42.80 / 0.9985 | 2.74 / 10.82, 3.81 / 14.63; 52.60 / 0.9955, 43.47 / 0.9991 |
| H.264 4K | 5.59 / 14.09, 4.26 / 6.17; 52.51 / 0.9955, 40.18 / 0.9900 | 5.34 / 7.77, 4.30 / 6.52; 52.51 / 0.9955, 40.33 / 0.9898 | 6.19 / 8.58, 10.51 / 12.35; 52.54 / 0.9955, 40.52 / 0.9908 |
| HEVC 1080p | 0.92 / 2.00, 0.97 / 6.01; 52.34 / 0.9951, 45.70 / 0.9990 | 1.35 / 3.09, 1.91 / 15.48; 52.40 / 0.9952, 45.69 / 0.9990 | 1.48 / 2.82, 2.39 / 9.88; 52.28 / 0.9951, 45.48 / 0.9989 |
| HEVC 4K | 4.11 / 27.51, 2.64 / 11.01; 52.33 / 0.9951, 43.62 / 0.9932 | 4.90 / 20.35, 4.03 / 8.74; 52.38 / 0.9952, 43.64 / 0.9931 | 4.63 / 24.72, 4.39 / 12.33; 52.26 / 0.9951, 43.41 / 0.9931 |
| AV1 1080p | 0.88 / 1.98, 0.96 / 3.54; 52.07 / 0.9954, 44.08 / 0.9988 | 0.95 / 2.06, 1.26 / 4.34; 52.07 / 0.9954, 44.47 / 0.9989 | 1.10 / 2.06, 1.51 / 3.46; 52.52 / 0.9958, 45.17 / 0.9995 |
| AV1 4K | 2.88 / 4.92, 2.67 / 7.19; 52.07 / 0.9954, 45.02 / 0.9985 | 2.58 / 4.17, 3.20 / 27.71; 52.07 / 0.9954, 44.81 / 0.9983 | 3.32 / 7.65, 3.54 / 19.79; 52.52 / 0.9959, 45.08 / 0.9988 |

P1 stays. P2 gains nothing measurable on H.264 and HEVC and +0.4 dB on
AV1 scrolling text at 1080p, -0.2 at 4K, for up to 50 % more encode time
(HEVC). P3 gains up to 0.6 dB (H.264) and 1.1 dB (AV1) on scrolling text
at 1080p and less at 4K, but takes H.264 at 4K from 4.3 to 10.5 ms a
frame, past the 6 ms a 165 fps stream has, and HEVC from 1.0 to 2.4 ms at
1080p; the bytes stay within a few per cent.

## Video colour matrix

This PC (RTX 4080 SUPER), Edge 156. Flat colour patches encoded by each
path, decoded, and compared at the patch centres (largest channel error,
levels of 255):

| path | YUV it produces | what the stream signals | read as BT.709 | read as BT.601 |
|---|---|---|---|---|
| NVENC, D3D11 BGRA or system-memory BGRZ (H.264, HEVC, AV1) | BT.601 | BT.601 (H.264) | 39 | 1 |
| x264 with swscale's default (before) | BT.601 | BT.709 | 39 | 1 |
| x264 with swscale set to BT.709 | BT.709 | BT.709 | 1 | 42 |
| NVENC fed NV12 from the D3D11 video processor (BT.709) | BT.709 | BT.709 | 1-3 | 42-43 |

Edge decoding NVENC's streams with WebCodecs and drawing them through
WebGL `texSubImage2D` (the client's path), hardware and software decoders:
every codec came out as BT.709 (`VideoFrame.colorSpace.matrix`), whatever
the stream signals, 39 levels off on saturated colours (0,248,0 shown as
0,209,0; 184,72,48 as 194,81,45), greys exact. With `colorSpace` set to
BT.601 in the decoder's configuration: H.264 and AV1 exact (error at most
1); HEVC unchanged (the setting is ignored).

Cost of converting to BT.709 ourselves, ms per frame with the whole
picture changing every frame (p50 / p99, two runs; this PC's own AV1 4K
stream shared NVENC meanwhile):

| codec | size | NVENC converts BGRA (BT.601) | video processor to NV12 (BT.709) |
|---|---|---|---|
| AV1 | 1920x1080 | 0.87-0.89 / 2.5 | 0.91-0.95 / 2.7 |
| AV1 | 3840x2160 | 2.18-2.25 / 5.1-9.4 | 2.24-2.37 / 5.7-8.8 |
| H.264 | 1920x1080 | 1.66-1.70 / 3.0 | 1.71-1.77 / 3.5 |
| H.264 | 3840x2160 | 4.39-4.46 / 5.8-6.0 | 5.46-5.52 / 7.1-7.4 |

Frames came out 3 % smaller through NV12. swscale BGRZ to YUV420P (x264):
2.0-2.4 ms at 1080p and 8.6-10.1 ms at 4K with either matrix, x264's
frames 8 % smaller with BT.709.

## HDR

This PC's main display (3840x2160) in HDR mode, SDR content brightness
280 nits, peak 465 nits, Edge 156. A tool outside the browser read what
reached the screen by duplicating the display in 16-bit float (linear
scRGB, 1.0 = 80 nits) and averaging each patch; values in nits.

**Windows' own 8-bit conversion** (what the capture got before): it cuts
off at 80 nits.

| on the desktop | received |
|---|---|
| grey 32 / 64 / 96 / 128 | 64 / 117 / 171 / 225 |
| grey 160 and above, anything brighter than white | 255 |
| pure red, green, blue | unchanged |

In float, SDR white reads 3.50 (= 280 / 80), grey 128 0.755; divided by
the level the nine greys and three pure colours of the test page came back
to their 8-bit values. All 256 levels round-trip through our SDR
conversion at 80, 200, 280 and 480 nits (computed with the float16
composition).

**Canvas** (patches at 1, 1.5, 3 times SDR white: 280 / 420 / 840 is
right; one canvas per page, browser restarted per row):

| canvas | on screen |
|---|---|
| WebGL2 8-bit, or float with sRGB, on the page | 280 / 280 / 280 |
| WebGL2 float display-p3, or float sRGB next to a display-p3 or WebGPU `extended` canvas | 280 / 419 / 839 |
| WebGL2 opaque, float asked | float buffer refused (`INVALID_OPERATION`) |
| in a worker, alone (WebGL2 float or WebGPU `extended`) | 280 / 280 / 280 |
| worker WebGL2 float + visible display-p3 canvas on the page (1 px is enough) | 280 / 419 / 839 |
| the same, that canvas hidden, off-window or `opacity: 0` | 280 / 280 / 280 |
| worker WebGL2 float, `desynchronized`, alone | 80 / 95 / 129 (raw scRGB) |
| worker WebGL2 float, `desynchronized` + WebGPU `extended` canvas on the page | 280 / 419 / 839 |
| `RGB10_A2` tiles with our curve, last setup (138 / 280 / 552 / 1379 / 5517 wanted) | 138 / 280 / 554 / 1377 / 5494 |

**Decoded video signalled as HDR** (BT.2020 PQ bars at 100 / 203 / 400 /
1000 / 4000 nits): every way onto the page maps it differently.

| way onto the page | hardware HEVC | software AV1 |
|---|---|---|
| `<video>` | 71 / 145 / 244 / 325 / 463 | 83 / 170 / 307 / 465 / 465 |
| 2D canvas | 68 / 139 / 175 / 210 / 280 | 68 / 139 / 207 / 277 / 280 |
| WebGL `texImage2D` | 61 / 83 / 107 / 147 / 221 | 134 / 280 / 280 / 280 / 280 |
| WebGPU `importExternalTexture` | 141 / 162 / 182 / 210 / 252 | 123 / 143 / 154 / 162 / 162 |

The same samples signalled as SDR with the curve applied in our shader
came out 2-4 % low and in order (133 / 275 / 537 / 1323 / 5369 against
138 / 280 / 552 / 1379 / 5517), on WebGL and WebGPU, hardware and
software. A decoded picture reaches WebGL with 8 bits whatever was sent
(254-256 distinct levels of 877 sent); hence 8-bit video with our own
curve.

The finished feature was checked by the user by hand on this PC
(2026-11-11): SDR picture of the HDR display, and HDR to an HDR client.

## Sound

A 3 s sine played by a program of the session, received in headless
Chromium (pieces handed to the AudioContext, measured in the page):

| server | capture path | codec | tone received | queued ahead of the clock |
|---|---|---|---|---|
| Windows 11 | WASAPI loopback | Opus, 10 ms packets | 3.0 s, level as played | 59-61 ms |
| Linux container | own PulseAudio, `parec` | Opus | 3.00 s at 1000 Hz | 59-71 ms |
| Linux, PipeWire only | own PipeWire, `parec` via pipewire-pulse | Opus | 3.01 s at 1001 Hz | 60-82 ms |
| Linux, PipeWire without pulseaudio-utils | `pw-record` | Opus | 3.02 s | 31-72 ms |
| Linux, a user's PulseAudio, server as root | `parec` on `/run/user/1000` | Opus | 3.00 s at 998 Hz | 59-71 ms |
| Linux, no sound server and none installed | - | - | client and admin show what was tried | - |

With FFmpeg built without libopus the same Windows capture went out as AAC
(141 kbit/s measured). The ALSA loopback path was not exercised (no
snd-aloop module in the test kernel).

## Microphone

A sine as the client's microphone (a synthetic `MediaStream` in headless
Chromium), recorded for 3-6 s by a program on the server from the default
microphone:

| server | path | sent as | tone recorded |
|---|---|---|---|
| Linux container | own PulseAudio: `remothing-mic`, `pacat` | Opus | 2.99 s of 3.00 at 440 Hz, level as sent |
| Linux container | the same | plain 16 kHz samples | 2.99 s of 2.99 at 660 Hz |
| Linux, PipeWire only | own PipeWire, `pactl` + `pacat` via pipewire-pulse | Opus | 3.07 s of 3.07 at 440 Hz |
| Linux, PipeWire without pulseaudio-utils | `pw-loopback` + `pw-cat` | Opus | 2.98 s of 2.99 at 440 Hz |
| Linux, a user's PulseAudio with its own default microphone, server as root | pair added while talking, default source switched and put back | Opus | 2.98 s of 2.99 (pitch read as 453 Hz: that server resamples between 44.1 and 48 kHz) |
| Windows 11 | VB-CABLE (`CABLE In 16ch` → `CABLE Output`) | Opus | 5.99 s of 6.00 at 435-440 Hz, level as sent |
| Windows 11 | the same | plain 16 kHz samples | 5.99 s of 6.00 at 660 Hz |

A second client's button was disabled while the first talked and enabled
when it stopped or its tab closed. On Windows the cable was installed from
the client's question (13 s, with the administrator prompt accepted on the
server): its two-channel playing end refused to open before any restart
(0x8889000A) and the 16-channel end of the same cable was used; the default
playback device, which the new cable had taken, was put back. The default
recording device was `CABLE Output` while a client talked and the previous
one afterwards. The restart offer (cable installed but not there yet) did
not come up and was not exercised. Latency from mouth to program was not
measured.

## Frames held to the client's refresh

Container bench (`--fps 240`, x264), headless Edge on this PC reporting
165 Hz, per-frame trace (`debug.frame_trace`); intervals between frames
built (sent for the first row), after the first second:

| scene | server | capture fps | sent fps | shown / superseded per s | interval p10 / p50 ms |
|---|---|---|---|---|---|
| scroll 1280x720 | only frames with new changes held to the grid, two catch up after a gap | 240 | 165 | 126–161 / 4–39 | 5.3 / 6.1 |
| scroll 1280x720 | every frame held, grid lags at most one refresh | 240 | 165 | 164 / 1 | 6.0 / 6.1 |
| box 1920x1080 | every frame held, grid never behind now | 310 | 126 | – | 7.1 / 7.5 |
| box 1920x1080 | every frame held, grid lags at most one refresh | 283 | 128 | 125 / 0 | 7.2 / 7.7 |

In the box scene no frame was built less than 7 ms after the previous one
with either grid: the limit there is elsewhere (x264 in the container,
not measured). That a grid never behind now loses rate to late wake-ups
comes from a simulation, not from these runs.

Real session (Mac, Helium, Wi-Fi, 120 Hz, WebSocket) with the first policy:
1 700 frames of ~340 bytes in the first 3.5 s (tiles returning to lossless
after the keyframe, ~500 a second), then a change held 9.35 s while
captures kept coming. In a later 20 s window dragging a window: 115 frames
drawn a second, 69 % of refreshes with a new picture, 30 refreshes a second
with two or more frames; of the time draw gaps exceeded a refresh, 146 ms a
second came from the server's send gaps, 75 from the network, 20 from the
client. 61 of the server's gaps over two refreshes held a captured change,
47 had nothing new captured.

## Phone: refresh rate and video size

OnePlus CPH2653 (Android 16, Brave/Chromium 157, 120 Hz screen), Wi-Fi,
WebTransport, AV1 (NVENC), driven over the DevTools protocol through adb.
Bench container with the GPU (`--gpus all`), `--mode full --fps 120`;
the phone shows the display at 1927x1084 device pixels. p50 / p99 ms.

| display | client | fps | capture→draw | server wait | decode |
|---|---|---|---|---|---|
| 1920x1080 | refresh measured once on the page (60 Hz) | 60 | 27–33 / 44–88 | 13–17 | 6.1 |
| 1920x1080 | refresh from the worker's frames (120 Hz) | 110–118 | 15–16 / 32–54 | 0.9 | 4.8 |
| 3840x2160 | video 1926x1084, scaler one thread | 33 | 70–73 / 92–113 | 28–30 | 7.2–7.3 |
| 3840x2160 | video 1926x1084, scaler 8 threads | 52–60 | 38–42 / 156–176 | 13–15 | 6.1–6.3 |

The page's animation frames ran at 60 Hz (16.6 ms apart) while the
worker's ran at 120. At 3840x2160 the container's encode (swscale of the
whole picture, then NVENC) took 23.6 ms a frame with one scaler thread and
9.8 with 8; the rest of the gap to 120 fps is that path's copy of the
capture (4.5 ms). The same phone on the Windows demo (3840x2160 display,
video at the display's size outside motion mode) decoded each AV1 frame in
11–13 ms; capture→draw went from 6–10 ms without video to 18–26 with it.

Windows, `d3d11_bench --size 3840x2160 --scene plasma --enc nvenc-gpu
--fps 120 --no-check` (GPU video processor scaling), encode p50 / p99 ms
and KiB a frame:

| codec | full size | half size |
|---|---|---|
| AV1 | 2.44 / 3.57, 17.5 | 1.44 / 2.65, 6.9 |
| HEVC | 2.59 / 3.70, 40.3 | 1.45 / 2.62, 15.7 |
| H.264 | 5.31 / 6.44, 38.9 | 1.95 / 2.84, 14.4 |

Codecs on the phone, the same bench (1920x1080, `full`, 120 fps), 1200
video frames each:

| codec | fps | decode p50 / p90 / p99 ms | KB a frame | capture→draw p50 / p99 ms |
|---|---|---|---|---|
| AV1 | 116 | 4.8 / 6.3 / 10.2 | 5.1 | 15.9 / 34.1 |
| HEVC | 111 | 4.8 / 6.2 / 9.7 | 10.9 | 15.9 / 39.0 |
| H.264 | 101 | 5.3 / 7.0 / 10.4 | 10.6 | 17.3 / 44.8 |

Within half a millisecond of each other: choosing the codec per client by
measured decode time would gain nothing on this phone.

## Drawing at most once per display refresh

The phone, the same bench, 25 s per run, two runs each (from the trace:
draw times against the worker's animation frames):

| client | refreshes with a new picture | draws replaced before a refresh | capture→draw p50 / p90 / p99 ms |
|---|---|---|---|
| draw on arrival (`?pace=0`) | 82.7 %, 83.4 % | 16.1, 14.9 a second | 14.8–14.9 / 20.6–20.7 / 34–38 |
| at most one draw per refresh | 92.4 %, 92.4 % | 1.9, 1.4 a second | 17.1–17.6 / 22.4–22.7 / 39 |

Gaps between draws over 25 ms (the Wi-Fi's) stayed at 1.0–1.6 a second.

## Phone: transports on Wi-Fi and 5G

The phone, the same bench (1920x1080, `full`, 120 fps), drawing once per
refresh, 20-25 s per transport (`?transport=`). On 5G Wi-Fi was off and the
container held the ports the router forwards (the demo was stopped).

| link | transport | frames/s | refreshes with a new picture | network p50 / p90 / p99 ms | estimate |
|---|---|---|---|---|---|
| Wi-Fi | WebTransport | 115 | 92.1 % | 3.7 / 5.7 / 11.6 | |
| Wi-Fi | WebRTC | 114 | 92.6 % | 3.5 / 5.3 / 13.0 | |
| Wi-Fi | WebSocket | 115 | 93.3 % | 3.7 / 5.7 / 10.9 | |
| 5G | WebTransport | 70 | 51.0 % | 17.6 / 40.6 / 93.7 | 1 Mbit/s |
| 5G | WebRTC | 78 | 55.6 % | 17.6 / 38.8 / 83.0 | 2 Mbit/s |
| 5G | WebSocket | 66 | 47.8 % | 19.0 / 44.6 / 156.9 | 1 Mbit/s |

On this Wi-Fi the transports are alike at this load. On 5G the link model
cut its estimate to the floor on delay the radio adds whatever is sent
(0.5-0.9 Mbit/s sent, delay p10 9-20 ms and p90 30-110 ms per second); see
the next section.

## Link model on 5G: undoing cuts that do not work

`pacer.rs` simulator, 50 Mbit/s, 15 ms, a delay level 15-60 ms above the
path's minimum drawn every 300 ms (the minimum only in a 50 ms dip every
7 s), 30 ms of jitter; 90 s from a fresh start, estimate median of the last
60 s:

| content | path | every cut kept: fps, estimate | cuts undone when the delay stays: fps, estimate |
|---|---|---|---|
| 6 KB at 120 fps | wandering delay | 44, 2.2 Mbit/s | 93-94, 8.9-11.3 Mbit/s |
| video following the estimate | wandering delay | 65, 1.0 Mbit/s | 94-95, 27.7-29.7 Mbit/s |
| 6 KB at 120 fps | + 250 ms stall every 3 s | 49, 2.8 Mbit/s | 92-94, 9.2-11.5 Mbit/s |
| video following the estimate | + 250 ms stall every 3 s | 73, 1.0 Mbit/s | 92-93, 24.7-28.4 Mbit/s |

The 30 s retry changed none of these. A download taking two thirds of a
30 Mbit/s link for 20 s: the estimate came down to 10-13 Mbit/s and was
back within 1-2 s of the end, with either rule.

Bench container, `tc netem` profiles of "Bad links", `?bench=10`, one run
each (fps, capture→draw p50 / p99 ms):

| link | scenario | every cut kept | cuts undone when the delay stays |
|---|---|---|---|
| WAN 50M/20ms | box / scroll / full | 230 / 174 / 179 fps; 18/25, 23/26, 23/27 | 236 / 173 / 181 fps; 16/26, 19/27, 18/25 |
| Funnel-like 18M/40ms | box / scroll / full | 225 / 164 / 175 fps; 27/34, 30/59, 31/36 | 220 / 163 / 176 fps; 28/36, 32/63, 31/36 |
| Mobile 6M/50ms±10 0.5%loss | box | 167 fps; 46 / 141 | 186 fps; 50 / 145 |
| Mobile 6M/50ms±10 0.5%loss | scroll | 126 fps; 64 / 219 | 132 fps; 78 / 255 |
| Mobile 6M/50ms±10 0.5%loss | full | 72 fps; 71 / 184 | 84 fps; 67 / 155 |

No cut at all on the first two links. The phone on 5G, the same bench
(`full`, 1920x1080, 120 fps), one image after the other, 45 s each, on a
bad spell of the link (network p99 640-870 ms, stalls up to 1.2 s):

| server | transport | frames/s | refreshes with a new picture | cuts / undone / retried | estimate at the end |
|---|---|---|---|---|---|
| every cut kept | WebTransport | 66 | 49.1 % | 28 / 0 / 0 | 1.3 Mbit/s |
| cuts undone when the delay stays | WebSocket (WebTransport fell back) | 75 | 52.9 % | 27 / 10 / 0 | 2.6 Mbit/s |

On the real link most cuts still stand: its stalls end within the round
trip after the cut, so the delay has fallen by the time the cut is judged.

## Link model: one-way delay

`pacer.rs` simulator, video following the estimate (70 % of it, 120 fps),
60 s from a fresh start: frames a second and median estimate of the last
40 s. "Round trip": the ACK carries no arrival time, the model measures
queue from round trips (as before, with cuts undone when the delay stays);
"one way": the ACK carries the client's arrival time.

| path | round trip | one way |
|---|---|---|
| 5G-like (50 Mbit/s, 15 ms, delay 15-60 ms redrawn every 300 ms, 30 ms jitter, 250 ms stall every 3 s), all on the way out | 92 fps, 43.0 Mbit/s | 91 fps, 45.5 Mbit/s |
| the same without stalls | 95 fps, 74.0 Mbit/s | 94 fps, 51.6 Mbit/s |
| the same delays and stalls on the way back only | 92 fps, 43.0 Mbit/s | 111 fps, 157.4 Mbit/s |
| Wi-Fi power save (100 Mbit/s, 5 ms, 30 ms stall every 220 ms) | 112 fps, 311.5 Mbit/s | 112 fps, 312.9 Mbit/s |
| selkies' Wi-Fi model (20 ms + up to 40 ms redrawn every 20 ms) | 88 fps, 118.4 Mbit/s | 90 fps, 112.4 Mbit/s |
| the same with a 120 ms stall every 5 s | 89 fps, 96.4 Mbit/s | 89 fps, 103.1 Mbit/s |
| a real 8 Mbit/s link with the 5G delays | 91 fps, 10.5 Mbit/s, worst queue 138 ms | 91 fps, 10.2 Mbit/s, worst queue 132 ms |

Delay on the way back is what one-way measurement removes; delay on the
way out still costs frames through the overdue rule. Relaxing that rule by
the recent delay on the way out (selkies allows 100 ms + 4x jitter) gave
110-118 fps on these paths but let a slow link's own queue reach 384-744
ms (four simulator tests), so it was left out. A rule that keeps the
estimate through stalls (selkies' "a stall is not a slower path") changes
nothing here: the stall cases above no longer cut it.

## WebTransport sending: pacing, adaptive parity, pointer moves

`MODE=bench` containers side by side (`--mode box --fps 120` unless
said), 1920x1080, x264, headless Edge 158 on this PC, link shaped with
netem in each container (server → client only); "before" is commit
`e1412ce` (one-way delay already in), "after" the change. Runs of 15-20 s,
alternating. The host was loaded by other builds during the later runs:
the same image ranged 63-160 fps between runs, so latency differences
under 20-30 ms are not resolved here.

| link | before | after |
|---|---|---|
| 20 ms, no loss (fps, total p50/p95/p99 ms, 2 runs each) | 158 fps, 13/18/21 | 160 fps, 13/19/23 and 14/20/26 |
| 20 Mbit/s, 40-packet buffer, 1 % loss, `--mode full` (fps, drops at the buffer) | 76 fps, 173 drops | 78 fps, 148 drops |
| 20 ms, 3 % loss: WebTransport sessions given up in 4 runs of 20 s | 5 | 1 |

- Parity: on a clean path it fell to one in 16 pieces within 6 s; at 3 %
  loss it rose to one in 2 within 2 s, and then 0.6-2.4 % of the pieces
  were still asked for again each second.
- Before two client fixes the same 3 % loss had 10-17 % of the pieces
  asked for again even at one parity per 2: frames already complete but
  waiting behind an incomplete one were asked for whole every 15 ms, and
  holes were asked for before the parity that rebuilds them arrived.
- At the bottleneck the estimate stayed above the link (39-44 Mbit/s on
  20), so pacing at 2.5x of it spread nothing there.
- Stream socket on a link that goes silent (netem 100 % loss for 7 s):
  the client dropped it after 4.98 s and was streaming again 0.6 s after
  the link came back. After `docker restart` of the server the client was
  back 1.1 s after the restart returned.
