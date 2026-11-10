# Games: stalls of 200-450 ms, and a motion mode (whole screen as video)

- Status: under discussion (step 1 is agreed as the first thing to do)
- Created: 2026-11-10

## Goal

Playing a game through the client goes badly: frame-rate drops and lag. The
same games on a phone through Sunshine do not show it. The user wants to
know the cause, and asks whether streaming the whole screen as lossy video,
with no composition and no detection of changed areas, would solve it.

## Open questions

1. **Transport without retransmission stalls.** The user asked what this
   means; it was explained on 2026-11-10 (today a lost packet makes
   everything behind it wait for TCP to send it again; Sunshine never
   resends, it rebuilds the packet from redundancy or skips the frame). No
   decision yet. Decide after step 1 shows how much of the problem is the
   network. See step 3.
2. **"The whole screen is moving a lot"**, in numbers. Proposed: at least
   half of the screen's area is in the video layer, in every second. To be
   tuned with real sessions; confirm the idea, not the number.
3. **"The client's resolution"**: taken here as the client's screen in
   device pixels, not the size of its browser window (a window can be
   resized at any moment, and each size change costs a keyframe). Confirm.

## Decided (user, 2026-11-10)

- **Where it was seen**: a MacBook on Wi-Fi. The phone that plays well
  through Sunshine was on the same Wi-Fi. That network, the MacBook and
  the server are all in Madrid. So the client is powerful, the distance is
  short, and both comparisons used the same network.
- **Order**: measure first which stage stalls (step 1), as the initial
  analysis.
- **The mode is fully automatic**, no button, and deliberately slow to
  change. It is for anything that moves the whole screen: a game, a
  full-screen video.
  - On: after about 10 s without interruption of the whole screen moving
    a lot.
  - Off: only after 30 to 60 s in a row of very little movement.
  - Quick to enter, conservative to leave. Opening a 10 s video, browsing
    folders, opening another video and so on must not flip the mode back
    and forth.
  - Accepted cost: a static interface shown in this mode is lossy video
    whose quality follows the link. That is fine; it is the safer mode.
- **When the picture stops in this mode**: after 5 s completely still, send
  the exact picture to finish it, as today. If the screen changes while
  that is being sent, what is left of it no longer matters: go back to
  sending video of what is on the screen.
- **Size and frame rate per client**: they may go down to what the client
  has and never below. A 1080p client of a 4K server may get 1080p, not
  less. The same for frames per second.

## What was found

Read from the Windows demo's event log and status (`/api/status`) on
2026-11-10, build `d3e4a1d`, NVENC with D3D11 input, display 3168x1440 at
144 Hz. Nothing was changed on the server for this.

Measured:

- Between stalls the stream is fine: 98-127 fps, 10-40 Mbit/s, capture to
  draw 6-8 ms median and 19-29 ms p99, decode 2 ms median, network 4-5 ms.
- The complaint is the stalls: 28 late-frame reports (frames drawn more
  than 200 ms after their change), worst cases 200-490 ms, up to 6 frames
  in one 5 s report.
- The client's share in those reports is 0-19 ms. The client is not it.
- The reports split the delay in two ways:

  | split (ms) | examples | reading |
  |---|---|---|
  | almost all network | server 2 / network 203, 2 / 202, 2 / 201, 2 / 362 | on a 4 ms path, 200 ms is TCP's retransmission timeout: one lost packet holds everything behind it |
  | almost all server | 237 / 14, 271 / 7, 303 / 5, 361 / 80, 449 / 20 (most reports) | not separable with this log, see below |

- One client had a cap change in the middle: estimate cut to 103 and then
  72 Mbit/s (queue 100 and 363 ms), video cap 140 -> 72 -> 140 Mbit/s, two
  keyframes (344 and 224 KB), and a 488 ms late frame next to them.
- The link estimate of the long session read 3.2 million Mbit/s, with cuts
  "to 659 Mbit/s (queue 366 ms)" and "to 723 Mbit/s (queue 657 ms)": on
  this path the estimate has no meaning, so sending is not paced at all.
  Harmless on a LAN, but a bug. Re-check on the current build, whose pacer
  changed (`0bf2716`).
- `remothing.exe` runs at normal process priority, not elevated, and
  nothing in the server sets a GPU scheduling priority (the only
  `SetGPUThreadPriority` is in `crates/encoder/examples/d3d11_bench.rs`).
- At sampling time the League of Legends launcher was the busiest program
  (about 1.5 cores of 24; remothing 0.14), GPU utilization 34-36 %. No
  match was running then, so the load during the stalls was not observed.

Reasoned, not measured:

- "Server" time in a late report is capture to send. It contains two
  different things: waiting until the client has acknowledged earlier
  frames (a TCP stall seen from the next frame), and capture / upload /
  encode waiting for the GPU behind the game. The report does not say
  which. The stats overlay does (`wait`, `upload`, `encode`), but nobody
  read it during a stall.
- The phone with Sunshine and the MacBook with remothing shared one Wi-Fi.
  A network that loses a packet now and then would hurt us (TCP waits) and
  not Sunshine (no retransmission). That fits the "almost all network"
  rows, and says nothing about the "almost all server" ones.
- 3168x1440 at 144 fps is 2.57 million macroblocks per second; H.264 level
  5.2 ends at 2.07 million (about 116 fps at this size). Hardware decoders
  may not keep up above that. Not checked on any client.

## Why Sunshine does not show it

`docs/SUNSHINE.md` has the full comparison, from its source. The parts that
match what was found here:

- No retransmission: UDP with forward error correction, so a lost packet
  delays nothing (item 10). We use TCP, and the WebRTC data channel is
  reliable and ordered too.
- Process, thread and GPU priority raised, so a game cannot starve capture
  and encoding (items 1, 2, 4). We do none of it.
- Fixed bitrate agreed at connect, no keyframe for rate changes (items 5
  and 7). Our cap changes cost a keyframe each with NVENC through FFmpeg.
- Whole frames at the size and rate the client asked for (general
  knowledge of Moonlight, not read in the source).

## Motion mode

The user's idea, with their decisions above. Two modes per display:

- **Tile mode**: what exists. Changed tiles, a lossless layer, video for
  what moves.
- **Motion mode**: the whole frame goes to the video encoder on every
  capture. No tile comparison, no lossless layer, no composition.

### Switching

- The measure is a property of the screen, not of a client, so the mode is
  per display and every client of that display is in the same mode.
- Each second is judged "moving" or "quiet" (open question 2).
- Tile -> motion: 10 s of "moving" in a row. One quiet second starts the
  count again.
- Motion -> tile: 40 s of "quiet" in a row (the user's range is 30-60 s;
  start in the middle and tune). One moving second starts the count again.
- Both numbers are constants with the reason next to them, not settings.
- A switch costs each client one keyframe; with these times that is at
  most one every 10 s in the worst case and normally far less.

### While in motion mode

- Bitrate follows the link estimate, as the video cap does today: on a
  slow link the picture gets worse, not later.
- **Still picture**: after 5 s without any change, the exact picture is
  sent as lossless tiles, paced by the link like any other lossless data.
  A change on the screen while it is being sent drops what is left of it
  and video goes on; the next exact picture needs another 5 s of
  stillness. Pauses shorter than 5 s stay lossy.
- **Size per client**: scaled down to the client's screen when that is
  smaller than the display, never below it (open question 3). Pixels the
  client cannot show are not encoded, sent or decoded.
- **Frame rate per client**: capped at the client's refresh rate, never
  below it. Frames the client cannot show are not encoded.
- The client reports its screen size and refresh rate; encoders are per
  client already.

### What it removes and what it does not

In a game every tile changes every frame, so the whole screen already goes
through the video encoder. Motion mode removes:

- the per-frame tile comparison, whose result is read back from the GPU
  each frame (a point where the capture thread waits for a busy GPU);
- the lossless bursts when part of the picture stops (HUD, menus, pause),
  which compete with the video for the link and cost CPU on both sides;
- tiles moving between the two layers;
- with the per-client size and rate: decoding more pixels and frames than
  the client shows.

It does not remove a TCP retransmission stall, nor the encoder waiting for
a GPU the game keeps busy. Alone it is unlikely to make the stalls go away;
that is why step 1 comes first.

### Costs and risks

- Scaling needs a GPU scaler in front of each encoder (D3D11 on Windows,
  CUDA on Linux, a software one for the system-memory path).
- Text and thin lines are soft in 4:2:0 video. A static interface in motion
  mode looks worse than today until the 5 s exact picture arrives. Accepted
  by the user.
- The exact picture of a whole 4K screen is large; on a slow link it takes
  seconds and is the first thing dropped when the screen moves again.

## Proposed steps

1. **Find the stage** (agreed as the first analysis). Put the server's own
   split into the late-frame report: waiting for the window, upload, encode
   (the client already has the three numbers per frame), and whether a
   frame was waiting for an ACK. Then play a real match for some minutes
   and read the event log. Small: the report is built in
   `client/src/worker.ts`.
2. **If it is the GPU**: items 1, 2 and 4 of `docs/SUNSHINE.md` (process
   and thread priority, GPU priority, MMCSS for the compositor). Small.
   GPU priority above normal may need the elevated or service process.
3. **If it is TCP**: no cheap fix (open question 1). Options are a WebRTC
   video track (the browser brings RTP, loss recovery and its own jitter
   buffer; `rtc.rs` uses str0m for data channels only today) or
   WebTransport datagrams with our own error correction
   (`docs/SUNSHINE.md` item 10). Either is a third transport and a task of
   its own.
4. **Motion mode**, as described above. Needs a path in
   `crates/core/src/shared.rs` and `stream.rs` that bypasses the tile
   policy, the mode decision next to the capture loop, scalers in
   `crates/encoder`, and a client that draws video only while it is on.
5. **Estimate without bound** on short paths: reproduce on the current
   build and bound it (separate small fix, whatever the rest).

## Testing

- A real game on the Windows machine is the only faithful load; the user
  plays, the event log is read afterwards. `AGENTS.md`: do not type or
  click into the live session.
- For repeatable numbers: a GPU-bound load next to `rt-testapp --mode
  full` on a private instance, before and after steps 2 and 4, compared
  with the profiles in `docs/RESULTS.md`. `docs/SUNSHINE.md` asks for the
  same baseline under load ("Orden de trabajo sugerido", step 1).
- Loss: the Linux bench container with `netem loss`, where the 2 % Wi-Fi
  profile is the reference for step 3.
- Mode switching is logic that can be judged without a machine: a unit
  test of the decision with the user's own case (10 s of video, some
  seconds of folders, again and again) must show no flip-flopping, plus
  the two thresholds and the reset of each count.
- Still picture: stop `rt-testapp` for 6 s and check the client's picture
  is pixel-exact; move again at 5.5 s and check the video resumes at once.
- Per-client size and rate: a client with a smaller screen and a 60 Hz
  refresh next to a full-size one; check what each receives.

## Not verified

- Which of the two readings of "server" time is the real one. Everything
  after step 1 depends on it.
- That the stalls in the log happened during a match and not in the
  launcher or on the desktop.
- Decoder limits on the clients, and how Moonlight was configured
  (resolution, rate, bitrate) in the sessions the user compares with.
