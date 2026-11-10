# Performance techniques from Sunshine that we could adopt

- Status: under discussion
- Created: 2026-11-10

## Goal

The user asked to study LizardByte/Sunshine, a project similar to this one,
for what it does for performance that remothing could do too. This file is
the catalogue of every improvement that study found, with what was measured
since. Nothing here is implemented, and nothing is to be implemented from
this file: the user will later turn this and the other investigations into
separate tickets. Only what came out of the Sunshine study belongs here.

## Open questions

None block this file. These belong to single candidates and are to be
settled when each becomes a ticket:

1. **Candidate 2**: the realtime GPU scheduling class is the only setting
   that helped (see "Measured under load"). Sunshine documents NVIDIA driver
   freezes with it when hardware-accelerated GPU scheduling (HAGS) is on.
   Always on, an option, or only with HAGS off?
2. **Candidate 3** changes a setting of the NVIDIA driver on the host (more
   power drawn while idle), and the measurements here do not support it.
   Drop it, or try it on a machine whose GPU is really idle?
3. **Candidate 6**: which browsers and devices must keep working decides
   between HEVC and AV1, or neither.
4. **Candidate 9** cannot be verified here (no AMD or Intel GPU).

## Decided (user, 2026-11-10)

- This file documents the improvements found; it is not a work order.
  Tickets come later, for all related investigations together.
- Loading the CPU and GPU of this PC for measurements was allowed on
  2026-11-10; the results are under "Measured under load".
- `docs/SUNSHINE.md` (the same analysis in Spanish) was deleted; this file
  replaces it.
- The sound candidate (was 12) is dropped as a separate item: see "Sound".

## Source and method

- Sunshine at commit `ec2676b` (2026-11-10), cloned to `.tools/ref/Sunshine`
  (git-ignored). Read: `src/video.cpp`, `src/stream.cpp`, `src/input.cpp`,
  `src/audio.cpp`, `src/nvenc/*`, `src/platform/windows/*`,
  `src/platform/linux/*`, `src/config.cpp`, `docs/configuration.md`.
- Sunshine was neither built nor run. Everything said about it comes from
  reading the code. `file:line` references are relative to its root; those
  marked `~` may be off by a few lines.
- Benefits stated for remothing are expected, not measured, except where
  "Measured under load" gives numbers.

## How the two differ

| | Sunshine | remothing |
|---|---|---|
| What is sent | Every frame, whole, as video | Lossless 64×64 tiles, video only for what moves |
| Rate | Fixed bitrate agreed at connect; never changes in a session | Continuous link estimate; the encoder cap follows it |
| Static screen | Re-sends duplicates at half the client's fps | Sends nothing |
| Transport | Its own UDP: RTP + Reed-Solomon, no retransmission | WebSocket (TCP) or WebRTC data channel (reliable SCTP) |
| On a loss | FEC; else invalidate reference frames; else keyframe | The transport retransmits; everything behind waits |
| Congestion control | None: the client's loss reports are only logged | Its own, BBR-like, from frame ACKs |
| Changed regions | Unused: copies and encodes the whole frame | DXGI dirty and move rects / XDamage |
| Client | Native (Moonlight) | Browser (WebCodecs + WebGL2) |
| Codecs | H.264, HEVC, AV1; 4:2:0 and 4:4:4; HDR | H.264 4:2:0 (the lossless layer gives sharp text) |

Evidence: fixed bitrate (`src/rtsp.cpp:1202-1226`; no `reconfigure` in
`src/`); loss stats only logged (`src/stream.cpp:~1208-1222`); duplicates
(`src/video.cpp:2437-2440, 2478-2490`); `GetFrameDirtyRects` appears
nowhere in `src/`.

So much of Sunshine does not apply, and in several places we are ahead (see
"Not worth copying"). Where it is ahead: host scheduling, encoder control,
and loss without retransmission.

## Candidates

| # | Improvement | What it would give | Cost | Risk | Evidence so far |
|---|---|---|---|---|---|
| 1 | Thread and process priority; our own timer resolution | No stalls when the host CPU is busy | Low | Low | Measured: removes them |
| 2 | Realtime GPU scheduling class | Encode and capture unaffected by a busy GPU | Low | Medium: NVIDIA driver bug with HAGS | Measured: removes the effect; GPU thread priority 7 and the high class do nothing |
| 3 | NVIDIA "prefer maximum performance" profile | Shorter encode with an idle GPU | Medium (NvAPI) | Low; more power | Measured: no sign of it here |
| 4 | `DwmEnableMMCSS` while clients are connected | More regular Windows composition under load | Very low | Low | Not measured |
| 5 | Native NVENC: bitrate change without a keyframe | Removes the 2-3 keyframes of every start and the one of every cap change | High | Medium | Keyframe sizes measured |
| 6 | Optional HEVC / AV1, with split-frame encoding | Less bitrate for the same quality on slow links; shorter encode at 4K | Medium-high | Medium: browser support | Support checked in Edge on this PC |
| 7 | Rate-control trial: one-frame VBV, quarter-resolution two-pass | Tighter frame sizes, fewer late frames on slow links | Low (a trial) | Low | Not tried |
| 8 | QoS: DSCP on our sockets, Wi-Fi streaming mode on the host | Less jitter on a local Wi-Fi | Low-medium | Low; some routers mishandle it | Not measured |
| 9 | AMD and Intel: a bitrate cap, with Sunshine's option sets | Video would follow the link on those GPUs; today it is constant quality | Medium | Medium: no such hardware here | Read in our code |
| 10 | Video as datagrams with FEC (no head-of-line blocking) | Steady latency under loss | Very high | High | Being designed in task 002 |
| 11 | Linux real desktops: NvFBC / KMS / PipeWire | Capture without the CPU; Wayland | Very high | Medium | None |

`tasks/002-game-stalls-video-mode.md` refers to these numbers: its GPU step
is candidates 1, 2 and 4, its UDP video is candidate 10, and it needs the
reference invalidation of candidate 5.

### 1. Thread and process priority, our own timer resolution

Sunshine:
- Capture and control threads at `THREAD_PRIORITY_HIGHEST`; encode and send
  at `THREAD_PRIORITY_ABOVE_NORMAL` (`src/platform/windows/misc.cpp:1149-1175`,
  `src/video.cpp:1703, 2905`, `src/stream.cpp:1337, 1620`).
- `HIGH_PRIORITY_CLASS` for the process while a session runs, back to normal
  after (`misc.cpp:1227-1228, 1303-1304`).
- System timer resolution set to 0.5 ms with `NtSetTimerResolution`,
  `timeBeginPeriod(1)` as fallback (`misc.cpp:110-126, 1218-1225`).
- The audio capture thread also joins MMCSS "Pro Audio"
  (`src/platform/windows/audio.cpp:700-704`).
- Linux: nice −15 / −10 through RealtimeKit, else `setpriority`
  (`src/platform/linux/misc.cpp:523-586`). No `SCHED_FIFO`.

Ours: no thread or process priority is set anywhere in `crates/`. Each
client's producer thread paces with `std::thread::park_timeout`
(`crates/core/src/stream.rs:655`), which on Windows follows the system timer
resolution. Only capture has its own high-resolution timer (`PollTimer` in
`crates/platform-windows/src/dxgi.rs`).

Measured (details below): with 48 busy processes on 24 logical CPUs the
Windows server stalls for 70-140 ms many times a second; with the process
at the high priority class it behaves as on an idle machine. Timer: on
2026-11-10 the system timer was at 0.75 ms because some other program had
asked for it, and waits of 1-5 ms overshot by at most 0.8 ms. With nobody
asking the Windows default is 15.6 ms; that case was not measured.

Proposal:
- High priority for the capture thread; above normal for client threads,
  the WebRTC hub, the network runtime and the `audio` thread (a stalled
  sound thread is a dropout: the client plays only 60 ms ahead).
- High process class while clients are connected.
- Ask for the timer resolution ourselves while clients are connected.
- Linux: negative nice needs `--cap-add SYS_NICE` in a container, and the
  measurement there showed little from it.

### 2. GPU scheduling class

Sunshine:
- `D3DKMTSetProcessSchedulingPriorityClass(REALTIME)` after enabling
  `SE_INC_BASE_PRIORITY_NAME`. On NVIDIA with HAGS and `nvenc_realtime_hags`
  off it uses `HIGH`: realtime "can cause unrecoverable encoding freeze or
  outright driver crash" (`src/platform/windows/display_base.cpp:~629-708`).
- `IDXGIDevice::SetGPUThreadPriority(7)` on the capture and the encode
  device (`display_base.cpp:~718`, `display_vram.cpp:900`).
- `IDXGIDevice1::SetMaximumFrameLatency(1)` on the capture device
  (`display_base.cpp:~724-736`).

Ours: nothing in the server. `crates/encoder/examples/d3d11_bench.rs` has
`--gpu-priority`.

Measured (details below), GPU kept busy by another program: encode p99
goes from 5 to 17 ms. `SetGPUThreadPriority(7)` changes nothing; neither
does the `HIGH` class. The `REALTIME` class brings encode back to idle
values (p99 4-6 ms). It was accepted for a non-elevated process here. HAGS
is off on this PC, so Sunshine's caveat was not exercised.

Proposal: the realtime class for the server process, with a way to turn it
off (open question 1) and a check for HAGS as Sunshine does.
`SetMaximumFrameLatency(1)` was not measured.

### 3. NVIDIA driver profile

Sunshine creates an application profile with
`PREFERRED_PSTATE = PREFER_MAX`
(`src/platform/windows/nvprefs/driver_settings.cpp:267-302`, option
`nvenc_latency_over_power`, on by default). Its docs: NVIDIA's adaptive
P-state algorithm "doesn't work well with low latency streaming". It also
forces OpenGL and Vulkan to present on DXGI
(`driver_settings.cpp:~153-208`); that only matters for fullscreen games.

Measured on this PC: while an NVENC session is open the GPU stays at
2550 MHz (P2) whether it encodes 5 or 144 frames a second, and the encode
p50 is 3.4-3.5 ms at 5 fps against 3.2 ms at 144. At rest the same GPU sits
at 1200-1700 MHz. So the clock does not drop under an open session here.
The GPU was 20-35 % busy with other programs all along; a host whose GPU is
truly idle was not available.

Proposal: none for now (open question 2). If tried: by hand first, "Power
management mode: prefer maximum performance" for `remothing.exe` in the
NVIDIA Control Panel, comparing `encode`.

### 4. MMCSS for the compositor

Sunshine calls `DwmEnableMMCSS(true)` when streaming starts and `false`
when it stops (`src/platform/windows/misc.cpp:1215-1216, 1316-1317`).

On Windows the 5-10 ms between the application drawing and duplication
delivering the frame (`docs/RESULTS.md`) is the compositor's. We do not
control it, but can ask for it to be scheduled as multimedia.

Not measured: it needs `app→draw`, which only the barcode probe gives, and
the probe moves the mouse of the live session. Measure with the user away,
CPU loaded: `app→draw` minus `capture→draw`.

### 5. Native NVENC

Sunshine does not use FFmpeg for NVENC on Windows but its own wrapper of
the SDK (`src/nvenc/`). The code does not say why; it shows what it gets:
- **Reference frame invalidation.** When the client reports lost frames it
  calls `nvEncInvalidateRefFrames` for the range and the next P-frame
  predicts from an older good reference. A keyframe only when the range
  does not fit the reference buffer (`src/nvenc/nvenc_base.cpp:799-834`).
- For that it keeps 5 references stored while each frame uses one
  (`nvenc_base.cpp:284-299`).
- **Intra refresh** instead of keyframes, on the client's request
  (`nvenc_base.cpp:329-348`).
- **Split-frame encoding** over the card's two NVENC engines, HEVC and AV1
  only; docs: "significantly reduces encoding latency"
  (`nvenc_base.cpp:226-247`).
- Bounded wait: asynchronous encode with an event and a 100 ms limit
  (`nvenc_base.cpp:744-761`).
- Encoder torn down on a detached thread so a restart is not held up
  (`src/video.cpp:2415-2431`).

Ours: NVENC through FFmpeg. Changing the bitrate cap forces a keyframe
(ARCHITECTURE, link model). With slow start that shows: two or three cap
changes in the first second of every connection, each with its keyframe.
Measured, 50 Mbit/s profile: 69 KB, then 143 KB, then 387 KB.

Proposal, by interest:
1. **Bitrate change without a keyframe** through
   `nvEncReconfigureEncoder`. Sunshine does not do this (its bitrate is
   fixed); it is what the native path would allow us. That the SDK accepts
   it without resetting the encoder comes from NVIDIA's documentation and
   was not tried.
2. Reference invalidation when frames are lost for good: today when the
   data channel is given up, and for every unrepaired loss once video
   travels as datagrams (task 002).
3. Split-frame encoding, if HEVC or AV1 are adopted too.

Cost and risk: high. `unsafe` code against the SDK with run-time version
selection (Sunshine builds against four SDK versions,
`src/nvenc/nvenc_dynamic_factory.cpp:66-113`), and the FFmpeg path stays
for AMD, Intel and x264.

Measure: count in the event log how many keyframes come from cap changes
in a real session and how many late frames follow them; then the start-up
runs (first 2.5 s) on the 18 Mbit/s and mobile profiles.

### 6. HEVC and AV1

Sunshine offers H.264, HEVC and AV1 and tests each at start with a real
encode (`src/video.cpp:3029-3464`).

Ours: H.264 only; the client configures `avc1.640033`
(`client/src/worker.ts:421`).

Checked on this PC, Edge 156 and 157, `VideoDecoder.isConfigSupported` at
3168x1440 with `optimizeForLatency`: H.264 and AV1 in hardware and in
software; HEVC in hardware only. Whether they decode as fast as H.264 was
not measured, and no other browser or device was checked.

Proposal: a negotiated codec. The client asks WebCodecs what it decodes in
hardware and the server chooses. At the same bitrate HEVC and AV1 look
better, which is what the mobile profile (6 Mbit/s) lacks. With either,
NVENC's split-frame encoding could be switched on as well.

To check first: decode latency against H.264; whether our FFmpeg exposes
split-frame encoding. The flag that tells the browser not to reorder
frames is H.264-specific (`mark_no_reorder` in
`crates/encoder/src/ffmpeg.rs`); the other codecs need their equivalent.

### 7. Rate control: a trial

Sunshine: constant bitrate with a VBV of **one frame** (`bitrate / fps`)
and, by default, two-pass at quarter resolution
(`src/nvenc/nvenc_base.cpp:251-274`, `src/nvenc/nvenc_config.h:36`). Its
docs: without two-pass there is "occasional bitrate overshoot and
subsequent packet loss". For them an oversized frame is loss; for us it is
queue.

Ours: capped VBR, QP floor 20, VBV of two frames at 60 fps (33 ms of link
per frame), no two-pass (`crates/encoder/src/ffmpeg.rs:50-66, 160-172`).
The comment there says two-pass does not change encode time; it says
nothing about frame sizes, which is the point here.

Proposal: a trial. One-frame VBV and `multipass=qres`; look at the
frame-size distribution and the late frames on the 18 Mbit/s and mobile
profiles with `full`. On the mobile profile we saw frames arriving
217-455 ms late. Not tried: the server has no setting for encoder options
(`d3d11_bench --set` has, but reports only the mean frame size).

### 8. Network quality of service

Sunshine:
- Marks traffic. Linux: `IP_TOS` with DSCP CS5 for video and `SO_PRIORITY`
  5 (`src/platform/linux/misc.cpp:1061-1122`). Windows: qWAVE,
  `QOSAddSocketToFlow(..., QOSTrafficTypeAudioVideo)`
  (`src/platform/windows/misc.cpp:~1678-1775`). Option `dscp_tagging`, on
  by default, because "some routers handle tagged traffic poorly".
- Puts the host's Wi-Fi interfaces into media streaming mode, against the
  background scans that cause loss and jitter (`misc.cpp:~1240-1268`).
- 1 MB UDP send buffer (`src/stream.cpp:~2062-2067`).

Ours: only `TCP_NODELAY` (`crates/core/src/app.rs:177`). The WebRTC UDP
sockets use system defaults.

Proposal: DSCP on the UDP sockets and the stream's TCP sockets, and Wi-Fi
streaming mode when the host is on Wi-Fi. The effect ends at the local
network: the Internet usually clears the marks, and through a proxy they
reach only the first hop. It makes sense for a client on the home Wi-Fi.

Measure: not possible with `netem` on loopback; needs a real Wi-Fi with
background traffic, comparing `net` p99.

### 9. AMD and Intel: a bitrate cap

Sunshine has low-latency option sets with a capped rate for both:
- AMF: `usage=ultralowlatency`, `rc=vbr_latency`, `async_depth=1`, no
  filler, no frame skipping (`src/video.cpp:979-1087`, defaults in
  `src/config.cpp:762-777`).
- QuickSync: `low_delay_brc=1`, `async_depth=1`, `low_power=1` with a
  retry without it, VBR with the maximum equal to the target
  (`src/video.cpp:865-973`).
- VAAPI on Linux: VBR with a one-frame VBV on Intel, CBR elsewhere
  (`src/platform/linux/vaapi.cpp:339-395`).

Ours: AMF and QuickSync encode at constant QP 20 with no bitrate cap; the
code says "their capped modes are untested here"
(`crates/encoder/src/lib.rs:59-62`). On those GPUs the video layer does not
follow the link estimate, so on a slow link video can ask for more than
fits. There is no VAAPI backend.

Proposal: adopt those options and let `caps_bitrate` cover AMF and
QuickSync. Cannot be validated here.

### 10. Video that does not wait for retransmissions

Sunshine splits each frame into packets and adds Reed-Solomon parity: 20 %
by default, a minimum number of parity packets asked by the client, up to
four blocks per frame (`src/stream.cpp:808-953, 1697-1739`). Data packets
are not copied to compute parity (`stream.cpp:~886-933`). When FEC is not
enough the client asks for reference invalidation or a keyframe. Nothing
is ever retransmitted, so a loss does not delay the frames after it.

Ours: reliable transports; a loss holds everything behind it until it is
retransmitted. It is our worst measured case: Wi-Fi with 2 % loss, `full`
drops to 96 fps with 146 ms p99 (`docs/RESULTS.md`). Unreliable delivery
with our own resend over the data channel was tried and dropped, because
SCTP's congestion control counts what is abandoned as lost too
(ARCHITECTURE, transport).

What it takes is a third transport, not a technique. Task 002 has taken
this up and holds the design (WebTransport datagrams, redundancy, the
certificate); what Sunshine contributes is the list above.

### 11. Linux with a real desktop

Sunshine:
- **KMS**: takes the plane's framebuffer as a DMA-BUF, imports it as an EGL
  image, converts to YUV in a shader and writes straight into the
  encoder's surface (VAAPI) or memory shared with CUDA. Pixels never touch
  the CPU (`src/platform/linux/kmsgrab.cpp:~2130-2166`,
  `graphics.cpp:773-793, 1494-1559`, `vaapi.cpp:420-466`,
  `cuda.cpp:1045-1120`). Needs `CAP_SYS_ADMIN`, confined to one thread
  (`kmsgrab.cpp:52-200`).
- **NvFBC** on X11 with NVIDIA: capture straight to CUDA
  (`cuda.cpp:1583-1905`).
- **PipeWire** and **wlroots** for Wayland, also with DMA-BUF.
- **VAAPI** encoder with the low-power entry point, for Intel and AMD
  (`vaapi.cpp:195-199, 285-290`).

Ours: X11 with XShm and XDamage, reading only what changes, partial upload
to CUDA. No Wayland, no VAAPI.

Assessment: on X11 our capture is better than Sunshine's, which copies the
whole frame every tick and uses no XDamage (`x11grab.cpp:840-853`). What
is missing is the CPU-free path for fullscreen video on a real desktop,
and Wayland. For today's use (containers with Xvfb, which have no GPU
output) it gives nothing. A large project: it would also need the GPU tile
comparison we have on Windows brought to Linux.

## Sound

Sound was implemented after the Sunshine study, so it was compared
afterwards. What Sunshine does differently on Windows:
- WASAPI loopback woken by the engine's event
  (`AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK`,
  `src/platform/windows/audio.cpp:395-401, 692, 778-833`). We poll every
  5 ms (`crates/platform-windows/src/audio.rs`).
- The capture thread in MMCSS "Pro Audio" with a raised thread priority
  (`audio.cpp:700-704`, `src/audio.cpp:235`). Ours has neither.
- Opus in 5 ms packets at 96 kbit/s, over UDP with a fixed Reed-Solomon
  (4 data, 2 parity) code (`src/audio.cpp:53-143`,
  `src/stream.cpp:~2001-2027`). Ours: 10 ms at 128 kbit/s over the
  WebSocket, the client playing 60 ms ahead.

What we implemented does not do either of the first two, so it does not
make the item moot by itself. The item is dropped all the same:
- **The event** would save at most 5 ms out of a path that buffers 60 ms
  in the client alone. Measured on this PC with the polling: packets
  arrive 10.6 ms apart at p50, 11.4 at p99, 12 at most. Nothing to gain.
- **The priority** matters, but it is candidate 1: a sound thread at
  normal priority stalls with the others when the CPU is busy, and a
  stall over 60 ms is a dropout. The `audio` thread is in that proposal
  now. Sound under CPU load was not measured (nothing kept playing on the
  host and nothing audible may be played); the 70-140 ms stalls of the
  video threads in the same process are the evidence.
- Shorter packets and parity only make sense over UDP.

Our comment in `audio.rs` says "Loopback has no usable event"; Sunshine
uses one. Which is right on the Windows versions we target was not
checked.

## Measured under load

2026-11-10, this PC (24 logical CPUs, RTX 4080 SUPER, HAGS off, League of
Legends client open: GPU 20-35 % busy before any test), build `0bf2716`,
server not elevated.

How:
- **CPU load**: 48 processes spinning at normal priority (24 where noted).
- **GPU load**: a heavy fragment shader drawn into a 4096x4096 target in a
  headless Edge, a few frames queued at a time, as a game at uncapped
  frame rate would: 93-95 % utilization next to the encoder bench, 75-88 %
  next to the server.
- **Priorities were set from outside**: the process priority class with
  `Get-Process`, the GPU class with
  `D3DKMTSetProcessSchedulingPriorityClass` on the process handle. The
  process class raises every thread; per-thread priorities as proposed in
  candidate 1 were not tried.
- **Encoder alone**: `d3d11_bench --size 3168x1440 --enc nvenc-gpu --fps
  144`, 1440 frames per scene.
- **Server alone**: a private instance streaming `rt-testapp` at 144 fps on
  the second display, and a client that draws nothing: it acknowledges
  every frame at once and keeps the timings the server writes into each
  frame. `rt-testapp` and the client ran at high priority so that they
  were not what starved. 12 s per row. No input was sent.

Encoder alone, encode time in ms, p50 / p90 / p99:

| condition | `box` | `plasma` (whole screen) |
|---|---|---|
| as is (two runs) | 2.8-3.1 / 3.8 / 5.0-5.2 | 4.0-4.1 / 5.0-5.2 / 6.2-6.5 |
| CPU: 24 busy | 3.1 / 4.3 / 13.2 | 3.9 / 5.1 / 8.0 |
| CPU: 24 busy, high process class | 3.0 / 4.0 / 5.5 | 4.3 / 5.3 / 6.6 |
| CPU: 48 busy | 3.1 / 4.1 / 123 | 4.1 / 5.1 / 57.7 |
| CPU: 48 busy, high process class | 3.1 / 4.0 / 5.6 | 4.1 / 5.2 / 6.5 |
| GPU: 66 % busy | 3.2 / 4.7 / 6.3 | 3.7 / 5.3 / 6.7 |
| GPU: 93-95 % busy (two runs) | 6.0-6.2 / 8.9-9.8 / 16.0-16.3 | 6.4-6.5 / 9.9-10.3 / 16.9-17.6 |
| GPU busy, `SetGPUThreadPriority(7)` (two runs) | 6.1-6.2 / 9.5-9.7 / 17.2-17.8 | 6.3-6.4 / 9.6 / 16.1-16.6 |
| GPU busy, GPU class high | 6.2 / 9.8 / 17.7 | 6.6 / 10.3 / 17.2 |
| GPU busy, GPU class high and priority 7 | 6.2 / 9.7 / 17.1 | 6.3 / 9.9 / 16.3 |
| GPU busy, GPU class realtime | 2.9 / 3.9 / 4.4 | 3.5 / 4.5 / 4.8 |
| GPU busy, high process class (CPU) | 7.5 / 12.3 / 16.8 | 7.3 / 12.7 / 17.4 |

Encode time against frame rate (`box`, GPU clock sampled meanwhile, always
2550 MHz P2): 5 fps 3.4-3.5 ms p50; 20 fps 3.2-3.4; 60 fps 3.2; 144 fps
3.2.

Server alone on Windows, 3168x1440. `capture→sent` in ms, p50 / p99 /
max; stalls = gaps over 50 ms between frames in 12 s:

| scenario | condition | fps | capture→sent | encode p99 | stalls |
|---|---|---|---|---|---|
| `full` | as is | 120 | 4.8 / 9.2 / 11 | 8.9 | 4 |
| `full` | CPU: 48 busy | 87 | 4.8 / 11.6 / 132 | 7.4 | 18 |
| `full` | CPU: 48 busy, high process class | 103 | 4.8 / 9.0 / 41 | 8.8 | 2 |
| `full` | GPU busy | 94 | 5.0 / 17.9 / 32 | 17.5 | 2 |
| `full` | GPU busy, GPU class realtime | 94 | 4.3 / 6.4 / 47 | 5.8 | 5 |
| `box` | as is | 139 | 1.8 / 4.1 / 8 | – | 2 |
| `box` | CPU: 48 busy | 100 | 1.8 / 71.9 / 142 | – | 49 |
| `box` | CPU: 48 busy, high process class | 133 | 1.8 / 4.2 / 8 | – | 1 |
| `box` | GPU busy | 117 | 2.0 / 13.5 / 49 | – | 2 |
| `box` | GPU busy, GPU class realtime | 122 | 1.5 / 3.6 / 9 | – | 5 |

`box` stayed lossless here, so it has no encode time; its GPU wait is the
tile comparison and readback. A few gaps of about 65 ms appear in every
row, idle included, and are not the server's (`capture→sent` stays low).
With the GPU busy the frame rate is bound by `rt-testapp`, which shares
that GPU.

Server alone on Linux (Docker on WSL2, Xvfb 1920x1080 at 165 fps, NVENC
through CUDA), load = 64 busy loops in another container:

| scenario | condition | fps | capture→sent | stalls |
|---|---|---|---|---|
| `full` | as is | 165 | 5.2 / 6.8 / 11 | 0 |
| `full` | 64 busy, nice 0 | 88 | 11.6 / 55.7 / 96 | 10 |
| `full` | 64 busy, server threads at nice −10 | 99 | 10.3 / 46.0 / 132 | 4 |
| `box` | as is | 170 | 0.4 / 1.2 / 7 | 0 |
| `box` | 64 busy, nice 0 | 208 | 0.4 / 8.0 / 27 | 0 |
| `box` | 64 busy, server threads at nice −10 | 206 | 0.4 / 7.5 / 37 | 0 |

What they say:
- **CPU, Windows**: a busy host stalls the server for 70-140 ms, in the
  encoder and before it. The high process class removes that completely.
  This is candidate 1, and it is the cheapest fix found.
- **GPU, Windows**: a busy GPU triples the encode p99. Of the three knobs
  Sunshine turns, only the realtime scheduling class does anything, and it
  does all of it. This is candidate 2.
- **Linux**: the server suffers too, and nice −10 barely helps. The load
  was in another container, where the kernel shares CPU between
  containers before it looks at nice; a native Linux host was not tested.
- **GPU clock**: no sign that an open encoder session lets the GPU clock
  down (candidate 3).

Not covered: the two loads together (a game does both), a real game, and
the realtime class with HAGS on.

## Smaller findings

- **Capture timestamp.** Sunshine uses the present time the system reports
  (`LastPresentTime`, `src/platform/windows/display_vram.cpp:1386-1390`).
  We take the clock when the frame is collected
  (`crates/platform-windows/src/dxgi.rs:590`), so our measured latency
  leaves out the wait until collection (up to 250 µs of polling). No speed
  gained; a more honest number.
- **Merging late input.** When its input thread is behind, Sunshine merges
  the pending mouse moves and scroll before applying them
  (`src/input.cpp:1840-2121`). We apply every message. It would only show
  after a network stall, when many arrive at once.
- **Display kept awake.** `SetThreadExecutionState(ES_DISPLAY_REQUIRED)`
  while capturing (`display_base.cpp:254-261`).
- **Fallback capture with Windows.Graphics.Capture** when duplication
  fails (`display_wgc.cpp`). Robustness, not speed.

## Not worth copying

- **Duplicates on a static screen.** They exist because Moonlight expects
  a constant stream. We send nothing, which is an advantage.
- **Fixed pacer.** Limits sending to 80 % of 1 Gbit/s whatever the link
  (`src/stream.cpp:~1742-1743`). Ours follows the estimate.
- **No congestion control.** On slow or changing links Sunshine relies on
  the user picking the right bitrate.
- **Whole-frame copies.** Its CPU path does a GPU→staging copy and a full
  `memcpy` per frame (`display_ram.cpp:305-337`). We read only the changed
  tiles.
- **Polling capture on Linux.** KMS, X11 and NvFBC deliver a frame every
  tick even when nothing changed.
- **A queue emptied whole on overflow.** 32 frames behind, it drops all of
  them (`src/thread_safe.h:409-441`).
- **4:4:4.** Offered so that text looks sharp. The lossless layer gives us
  that.
- **Batched sends (USO / GSO).** Saves CPU at hundreds of Mbit/s over UDP.
  Our video is 5-30 Mbit/s and mostly over TCP, where the system already
  does it.
- **`SetGPUThreadPriority(7)` and the high GPU class.** Measured: no
  effect next to a busy GPU.

## Already equal or better

| Technique | Sunshine | remothing |
|---|---|---|
| No B-frames, infinite GOP, keyframes on demand | Yes | Yes |
| NVENC P1 + ultra low latency, no lookahead | Yes | Yes |
| Encoder input without a CPU copy | Shader into the encoder's texture | GPU→GPU copy of the changed regions only |
| D3D11 device free while waiting for a frame | Partly: waits up to 200 ms inside `AcquireNextFrame`, yields 10 ms when the screen is static | Yes: polls without waiting every 250 µs |
| High-resolution timer in capture | Yes | Yes |
| Key repeat on the host, everything released on disconnect | Yes | Yes |
| Encoders tested at start | Yes, with a real encode | Yes |
| Only the newest frame when the consumer is slow | One-slot mailbox | Pending changes merge into one frame |
| Adapts to the link | No | Yes |
| Sends only what changes | No | Yes |

## Not verified

- Sunshine's behaviour at run time: only its code was read.
- Lines marked `~`.
- Per-thread priorities (the measurements used the process class), and
  whether the realtime GPU class is safe with HAGS on.
- Our pacing with the system timer at its 15.6 ms default.
- `DwmEnableMMCSS`, `SetMaximumFrameLatency(1)`, DSCP and the Wi-Fi
  streaming mode: no measurement.
- That `nvEncReconfigureEncoder` changes the bitrate without a keyframe.
- HEVC and AV1 decode latency; any browser other than Edge on this PC;
  whether our FFmpeg build exposes NVENC split-frame encoding.
- The NVIDIA power profile on a host with an idle GPU.
- Sound under CPU load; event-driven WASAPI loopback.
- Anything about AMF, QuickSync or VAAPI: no such hardware here.
- Linux priorities on a native host.
