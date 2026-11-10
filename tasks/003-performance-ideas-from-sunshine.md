# Performance techniques from Sunshine that we could adopt

- Status: under discussion
- Created: 2026-11-10

## Goal

The user asked to study LizardByte/Sunshine, a project similar to this one,
for what it does for performance that remothing could do too. Only the
analysis was asked for: nothing here is implemented. This file holds what
was found and the candidate improvements, so that each can be decided on.

`tasks/002-game-stalls-video-mode.md` is the first place these apply: its
step 2 is candidates 1, 2 and 4 here, and its step 3 is candidate 10.
Whatever it measures about which stage stalls decides the order here too.

## Open questions

1. **Which candidates to do, and in what order.** Recommended: 1, 2 and 4
   together (small, one measurement run covers them); try 3 and 7 by hand
   before writing code; decide 5 and 6 with those numbers; 12 whenever sound
   is next touched. 10 and 11 are projects of their own.
2. **One task per candidate?** They are independent. Recommended: when a
   candidate is agreed, move it into its own task file and trim it here.
3. **Measuring under load.** Candidates 1, 2 and 4 only show with the host's
   CPU or GPU busy. That means loading this PC on purpose while the user
   may be working on it: when is that acceptable?
4. **Candidate 2**: `HIGH` GPU scheduling class only, or `REALTIME` as an
   option despite the NVIDIA driver bug Sunshine documents?
5. **Candidate 3** changes a setting of the NVIDIA driver on the host
   (more power drawn while idle). Acceptable? The manual trial needs the
   NVIDIA Control Panel: done by the user, or by the agent?
6. **Candidate 6**: which browsers and devices must keep working decides
   between HEVC and AV1, or neither.
7. **Candidate 9** cannot be verified here (no AMD or Intel GPU). Is there
   a machine to test on, or should it wait?
8. **`docs/SUNSHINE.md`** holds the same analysis in Spanish (candidates 1
   to 11, same numbers; 12 is only here). Keep both, or delete the doc now
   that this task exists? `AGENTS.md` lists it and task 002 refers to its
   item numbers, so both would have to point here instead.

## Source and method

- Sunshine at commit `ec2676b` (2026-11-10), cloned to `.tools/ref/Sunshine`
  (git-ignored). Read: `src/video.cpp`, `src/stream.cpp`, `src/input.cpp`,
  `src/audio.cpp`, `src/nvenc/*`, `src/platform/windows/*`,
  `src/platform/linux/*`, `src/config.cpp`, `docs/configuration.md`.
- Sunshine was neither built nor run. Everything said about it comes from
  reading the code. `file:line` references are relative to its root; those
  marked `~` may be off by a few lines.
- **Benefits stated for remothing are expected, not measured**, unless a
  number is given with where it came from.

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

| # | Improvement | What it would give | Cost | Risk |
|---|---|---|---|---|
| 1 | Thread and process priority; our own timer resolution | Fewer latency spikes with a busy CPU; pacing that does not depend on other programs | Low | Low |
| 2 | GPU priority and `SetMaximumFrameLatency(1)` | Steady capture and encode with a busy GPU | Low | Medium: known NVIDIA driver bug with HAGS |
| 3 | NVIDIA "prefer maximum performance" profile for the process | Shorter encode with a nearly idle GPU, our usual case | Medium (NvAPI) | Low; more power |
| 4 | `DwmEnableMMCSS` while clients are connected | More regular Windows composition under load | Very low | Low |
| 5 | Native NVENC: bitrate change without a keyframe | Removes the 2-3 keyframes of every start and the one of every cap change | High | Medium |
| 6 | Optional HEVC / AV1, with split-frame encoding | Less bitrate for the same quality on slow links; shorter encode at 4K | Medium-high | Medium: browser support |
| 7 | Rate-control trial: one-frame VBV, quarter-resolution two-pass | Tighter frame sizes, fewer late frames on slow links | Low (a trial) | Low |
| 8 | QoS: DSCP on our sockets, Wi-Fi streaming mode on the host | Less jitter on a local Wi-Fi | Low-medium | Low; some routers mishandle it |
| 9 | AMD and Intel: a bitrate cap, with Sunshine's option sets | Video would follow the link on those GPUs; today it is constant quality | Medium | Medium: no such hardware here |
| 10 | Video as datagrams with FEC (no head-of-line blocking) | Steady latency under loss | Very high | High |
| 11 | Linux real desktops: NvFBC / KMS / PipeWire | Capture without the CPU; Wayland | Very high | Medium |
| 12 | Sound: event-driven loopback, MMCSS on the capture thread | Up to 5 ms less and steadier packets on Windows | Low | Low |

### 1. Thread and process priority, our own timer resolution

Sunshine:
- Capture and control threads at `THREAD_PRIORITY_HIGHEST`; encode and send
  at `THREAD_PRIORITY_ABOVE_NORMAL` (`src/platform/windows/misc.cpp:1149-1175`,
  `src/video.cpp:1703, 2905`, `src/stream.cpp:1337, 1620`).
- `HIGH_PRIORITY_CLASS` for the process while a session runs, back to normal
  after (`misc.cpp:1227-1228, 1303-1304`).
- System timer resolution set to 0.5 ms with `NtSetTimerResolution`,
  `timeBeginPeriod(1)` as fallback (`misc.cpp:110-126, 1218-1225`).
- Linux: nice −15 / −10 through RealtimeKit, else `setpriority`
  (`src/platform/linux/misc.cpp:523-586`). No `SCHED_FIFO`.

Ours: no thread or process priority is set anywhere in `crates/`. Each
client's producer thread paces with `std::thread::park_timeout`
(`crates/core/src/stream.rs:655`), which on Windows follows the system timer
resolution. Only capture has its own high-resolution timer
(`PollTimer` in `crates/platform-windows/src/dxgi.rs`).

Measured on this PC (2026-11-10): the system timer was at 0.75 ms because
some other program had asked for it, and waits of 1-5 ms overshot by at
most 0.8 ms. Nothing is lost right now, by luck. With nobody asking, the
Windows default is 15.6 ms; that case was not measured.

Proposal:
- High priority for the capture thread; above normal for client threads,
  the WebRTC hub and the network runtime.
- Ask for the timer resolution ourselves while clients are connected.
- High process class only while clients are connected.
- Linux: negative nice; the container needs `--cap-add SYS_NICE`.

Measure: the usual `box` / `scroll` / `full` runs with the host CPU loaded
on purpose (a build on every core); compare `srv wait` and `capture→draw`
p99. For the timer: browser on another machine, nothing else open on the
host.

### 2. GPU priority

Sunshine:
- `D3DKMTSetProcessSchedulingPriorityClass(REALTIME)` after enabling
  `SE_INC_BASE_PRIORITY_NAME`. On NVIDIA with hardware-accelerated GPU
  scheduling (HAGS) and `nvenc_realtime_hags` off it uses `HIGH`: realtime
  "can cause unrecoverable encoding freeze or outright driver crash"
  (`src/platform/windows/display_base.cpp:~629-708`).
- `IDXGIDevice::SetGPUThreadPriority(7)` on the capture and the encode
  device (`display_base.cpp:~718`, `display_vram.cpp:900`).
- `IDXGIDevice1::SetMaximumFrameLatency(1)` on the capture device
  (`display_base.cpp:~724-736`).

Ours: nothing in the server. `crates/encoder/examples/d3d11_bench.rs`
already takes `--gpu-priority`, but no result is recorded in
`docs/RESULTS.md`.

Proposal: priority 7 on our D3D11 device (capture and encoder share it) and
the `HIGH` class; `REALTIME` at most as an option. Needs an elevated
process: the service (LocalSystem) can; a non-elevated instance stays as it
is and says so in the log.

Measure: `encode` and `capture→draw` p99 with the GPU busy (a game or 4K
video on the other monitor). No difference expected with an idle GPU.

### 3. NVIDIA driver profile

Sunshine creates an application profile with
`PREFERRED_PSTATE = PREFER_MAX`
(`src/platform/windows/nvprefs/driver_settings.cpp:267-302`, option
`nvenc_latency_over_power`, on by default). Its docs: NVIDIA's adaptive
P-state algorithm "doesn't work well with low latency streaming", and
turning the option off "can lead to significantly increased encoding
latency". It also forces OpenGL and Vulkan to present on DXGI
(`driver_settings.cpp:~153-208`); that only matters for fullscreen games.

Why it matters to us: a desktop leaves the GPU nearly idle, which is when
the driver clocks down. Our Windows encode times are 2.4-3.5 ms p50 and
4-6 ms p99 (`docs/RESULTS.md`).

Proposal: first, by hand, set "Power management mode: prefer maximum
performance" for `remothing.exe` in the NVIDIA Control Panel and compare
`encode` p50 / p99 in `box`. If it helps, create the profile from the
server through NvAPI and undo it on exit, as Sunshine does (it keeps an
undo file for crashes).

### 4. MMCSS for the compositor

Sunshine calls `DwmEnableMMCSS(true)` when streaming starts and `false`
when it stops (`src/platform/windows/misc.cpp:1215-1216, 1316-1317`).

On Windows the 5-10 ms between the application drawing and duplication
delivering the frame (`docs/RESULTS.md`) is the compositor's. We do not
control it, but can ask for it to be scheduled as multimedia.

Measure: `app→draw` minus `capture→draw`, CPU loaded. One call; if it does
not show, leave it out.

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
2. Reference invalidation when the data channel is given up with video
   frames undelivered, instead of today's keyframe.
3. Split-frame encoding, if HEVC or AV1 are adopted too.

Cost and risk: high. `unsafe` code against the SDK with run-time version
selection (Sunshine builds against four SDK versions,
`src/nvenc/nvenc_dynamic_factory.cpp:66-113`), and the FFmpeg path stays
for AMD, Intel and x264. Only worth it if point 1 really improves starts.

Measure: before implementing, count in the event log how many keyframes
come from cap changes in a real session and how many late frames follow
them. After: the start-up runs (first 2.5 s) on the 18 Mbit/s and mobile
profiles.

### 6. HEVC and AV1

Sunshine offers H.264, HEVC and AV1 and tests each at start with a real
encode (`src/video.cpp:3029-3464`).

Ours: H.264 only; the client configures `avc1.640033`
(`client/src/worker.ts:421`).

Proposal: a negotiated codec. The client asks WebCodecs what it decodes in
hardware and the server chooses. At the same bitrate HEVC and AV1 look
better, which is what the mobile profile (6 Mbit/s) lacks. With either,
NVENC's split-frame encoding could be switched on as well.

To check first, none of it verified: which browsers decode HEVC in
hardware with `optimizeForLatency`; whether decode latency matches
H.264's; whether our FFmpeg exposes split-frame encoding. The flag that
tells the browser not to reorder frames is H.264-specific
(`mark_no_reorder` in `crates/encoder/src/ffmpeg.rs`); the other codecs
need their equivalent.

Measure: the same runs watching `decode` and `capture→draw`, plus a quality
comparison at a fixed bitrate on the mobile profile.

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

Proposal: a trial, no commitment. One-frame VBV and `multipass=qres`; look
at the frame-size distribution and the late frames on the 18 Mbit/s and
mobile profiles with `full`. On the mobile profile we saw frames arriving
217-455 ms late.

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

Proposal: DSCP on the WebRTC UDP sockets and the stream's TCP sockets, and
Wi-Fi streaming mode when the host is on Wi-Fi. The effect ends at the
local network: the Internet usually clears the marks, and through a proxy
they reach only the first hop. It makes sense for a client on the home
Wi-Fi.

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

What it would take: a third transport, not a technique. The route seen is
WebTransport: datagrams over QUIC, where the sender's congestion control
is our server's and could be our own link model. Video as datagrams with
FEC, the lossless layer on a reliable stream. This is an idea drawn from
what Sunshine does, not something Sunshine does in browsers.

Cost and risk: very high. HTTP/3 with a valid certificate; a direct UDP
path only (not through the proxy or Funnel, like the data channel); Safari
support unchecked; and it needs the reference invalidation of candidate 5.
A line of research for when loss is the main problem.

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

### 12. Sound on Windows

Sunshine:
- WASAPI loopback opened with `AUDCLNT_STREAMFLAGS_LOOPBACK |
  AUDCLNT_STREAMFLAGS_EVENTCALLBACK` and woken by the engine's event
  (`src/platform/windows/audio.cpp:395-401, 692, 778-833`).
- The capture thread joins MMCSS "Pro Audio"
  (`AvSetMmThreadCharacteristics`, `audio.cpp:700-704`) on top of its
  thread priority; capture and Opus encode are separate threads.
- Opus `RESTRICTED_LOWDELAY`, constant bitrate, packet length chosen by
  the client (5 ms by default), 96 kbit/s stereo
  (`src/audio.cpp:53-143`, `src/rtsp.cpp:~1079`).
- Over UDP with a fixed Reed-Solomon (4 data, 2 parity) code
  (`src/stream.cpp:~2001-2027`).

Ours: loopback polled every 5 ms, with the comment "Loopback has no usable
event" (`crates/platform-windows/src/audio.rs`); libopus low-delay mode,
10 ms frames, 128 kbit/s; over the WebSocket; the client schedules 60 ms
ahead (ARCHITECTURE, sound).

Proposal: try the event-driven loopback the way Sunshine opens it, and
MMCSS for the capture thread. Sunshine's code contradicts our comment;
which Windows versions deliver the event in loopback mode was not checked.
The gain is small next to the client's 60 ms: up to 5 ms and steadier
packets under CPU load. Shorter Opus frames are not worth it over TCP.

Measure: packet arrival jitter at the client with the host CPU loaded.
Mind `AGENTS.md`: nothing audible on this PC.

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

## Suggested order

1. Measure the baseline under load: busy CPU, busy GPU, and a host with
   nothing else open. We have none of those numbers and they decide 1, 2
   and 4.
2. Implement 1, 2 and 4; repeat.
3. Manual trials of 3 (NVIDIA Control Panel) and 7 (encoder options).
4. Count keyframes per cap change in real sessions, to decide 5.
5. Check HEVC / AV1 support and latency in the browsers we use, to
   decide 6.

Numbers go to `docs/RESULTS.md`; what is adopted, to ARCHITECTURE.

## Not verified

- Sunshine's behaviour at run time: only its code was read.
- Lines marked `~`.
- That `nvEncReconfigureEncoder` changes the bitrate without a keyframe.
- Which browsers decode HEVC in hardware in low-latency mode; AV1 decode
  latency.
- Whether our FFmpeg build exposes NVENC split-frame encoding.
- Our pacing with the system timer at its 15.6 ms default.
- Event-driven WASAPI loopback on the Windows versions we target.
- Anything about AMF, QuickSync or VAAPI: no such hardware here.
