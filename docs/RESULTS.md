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

## NVENC rate control trial (not adopted)

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
frame rate and latency. Kept the current settings.

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
