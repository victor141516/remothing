# Games: stalls of 200-450 ms, and a motion mode (whole screen as video)

- Status: agreed
- Created: 2026-11-10

## Goal

Playing a game through the client goes badly: frame-rate drops and lag. The
same games on a phone through Sunshine do not show it. The user wants to
know the cause, and asks whether streaming the whole screen as lossy video,
with no composition and no detection of changed areas, would solve it.

## Open questions

None. All were answered on 2026-11-10 and are under "Decided".

## Decided (user, 2026-11-10)

- **Where it was seen**: a MacBook on Wi-Fi. The phone that plays well
  through Sunshine was on the same Wi-Fi. That network, the MacBook and
  the server are all in Madrid. So the client is powerful, the distance is
  short, and both comparisons used the same network.
- **Video over UDP first, no measuring before it.** A lost packet on TCP
  shows as a hard stall; over UDP what is lost does not hold up what comes
  after it. Try UDP at every connection, direct to the server, without
  relay servers. If it cannot connect for whatever reason (a proxy that
  does not carry UDP, a closed port), fall back to the other transports,
  down to TCP.
- **UDP video is a WebRTC video track.** WebTransport datagrams were the
  other way and are left out; what they would give, and when to look at
  them again, is under "Alternative left out" below.
- **Measuring which stage stalls** is still wanted as analysis, but it
  does not come before UDP any more.
- **"The whole screen is moving a lot"**: at least half of the screen in
  the video layer during each second, as the first implementation; tune
  later.
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
- **Size per client**: the size of the browser window is the least the
  stream may be, not what it always is. When the link carries more, stream
  larger than the window, up to the display's own size: adaptive. A window
  being resized passes through many sizes; follow it only once it has
  settled (about 3 s), not at every step of the drag.
- **Frame rate per client**: may go down to what the client shows, never
  below.

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

`tasks/003-performance-ideas-from-sunshine.md` has the full comparison, from
its source (the candidate numbers below are its). The parts that match what
was found here:

- No retransmission: UDP with forward error correction, so a lost packet
  delays nothing (candidate 10). We use TCP, and the WebRTC data channel is
  reliable and ordered too.
- Process, thread and GPU priority raised, so a game cannot starve capture
  and encoding (candidates 1, 2, 4). We do none of it.
- Fixed bitrate agreed at connect, no keyframe for rate changes (candidates
  5 and 7). Our cap changes cost a keyframe each with NVENC through FFmpeg.
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
- Each second is judged "moving" (at least half of the screen's area in
  the video layer) or "quiet".
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
- **Size per client**, between two limits:
  - least: the picture's size in the client's window, in device pixels.
    The client reports it 3 s after the last resize, so dragging a window
    edge changes the stream once.
  - most: the display's own size.
  - in between: the largest size the link affords. A size needs pixels x
    frames per second x a bits-per-pixel constant (to be tuned); it is
    afforded when that fits in the video share of the link estimate. Sizes
    come from a short ladder (the least, the display's, and fixed fractions
    of it between them), since every change is an encoder restart and a
    keyframe.
  - larger only after the link has had room for 10 s; smaller as soon as
    it has not. Never below the least: there the bitrate drops instead.
  - Pixels beyond the window's are scaled away by the client. What a
    larger stream buys is colour detail (4:2:0 video stores colour at half
    size, so coloured edges and text are sharper after scaling down); what
    it costs is encoder and decoder work. The user chose it knowing that
    the stream may exceed the window.
- **Frame rate per client**: capped at the client's refresh rate, never
  below it. Frames the client cannot show are not encoded.
- The client reports its picture size and refresh rate; encoders are per
  client already.

### What it removes and what it does not

In a game every tile changes every frame, so the whole screen already goes
through the video encoder. Motion mode removes:

- the per-frame tile comparison, whose result is read back from the GPU
  each frame (a point where the capture thread waits for a busy GPU);
- the lossless bursts when part of the picture stops (HUD, menus, pause),
  which compete with the video for the link and cost CPU on both sides;
- tiles moving between the two layers;
- with the per-client rate: encoding and decoding frames the client never
  shows.

By itself it does not remove a TCP retransmission stall (that is what
"Video over UDP" is for), nor the encoder waiting for a GPU the game keeps
busy (step 5).

### Costs and risks

- Scaling needs a GPU scaler in front of each encoder (D3D11 on Windows,
  CUDA on Linux, a software one for the system-memory path).
- Text and thin lines are soft in 4:2:0 video. A static interface in motion
  mode looks worse than today until the 5 s exact picture arrives. Accepted
  by the user.
- The exact picture of a whole 4K screen is large; on a slow link it takes
  seconds and is the first thing dropped when the screen moves again.

## Video over UDP

Decided: UDP first, direct, fallback to what exists, as a WebRTC video
track carrying the video of motion mode.

Why it helps, and what it does not do by itself: compressed video frames
build on the previous ones, so a frame that lost a packet cannot just be
skipped; the following ones need it. Over UDP nothing waits for TCP's
timer, but the loss still has to be dealt with: ask again for that one
packet (a round trip, here a few ms), rebuild it from redundancy sent
along, or give up and ask for a keyframe.

| | WebRTC video track | WebTransport datagrams |
|---|---|---|
| what it is | the browser's own real-time video path (RTP): it asks again only for the missing packet, can use redundancy, asks for a keyframe when it gives up, decodes and shows the frames itself | bare unreliable packets over QUIC; everything about loss (redundancy, skipping, keyframes) is ours to write, on both sides |
| connects through | the UDP port and direct path the data channel already uses (`rtc.rs`, str0m) | a new HTTP/3 listener on a UDP port |
| browsers | all, phones included | Chrome and Firefox; Safari to be checked |
| certificate | nothing new | a CA certificate, or a self-signed one pinned by hash and renewed every two weeks |
| our pipeline | the browser shows the video (a `<video>` element), outside our decoder and compositor: fits motion mode, where the whole picture is video | stays ours: decoder, compositor, link model |
| cost | medium: media in str0m, keyframes on request, bitrate from its feedback | very high: a third transport, plus error correction in the client |
| main risk | the browser's own buffering adds delay; to be measured (cloud gaming in browsers uses this path) | each loss the redundancy cannot repair costs a keyframe until the encoder can invalidate references (task 003, candidate 5) |

### Alternative left out: WebTransport datagrams

The table shows its costs. What it would give that the video track does
not:

- **Possibly less delay.** The video track goes through the browser's own
  buffer, which holds frames a little to smooth the network; with
  datagrams the frames would stay in our decoder, which shows each one as
  soon as it is complete. How much the browser's buffer adds is not
  measured, so neither is this gain.
- **Video in tile mode too.** Frames would stay in our compositor, so the
  video areas of tile mode could also travel without retransmission
  stalls, in step with the lossless tiles. The video track only serves
  motion mode.
- **Our own link model** would set the sending rate, instead of the
  estimate WebRTC derives from the browser's feedback.

Look at it again if the video track's delay, once measured, turns out too
high, or if stalls in tile mode's video areas become the complaint.
Before that it would need the Safari check and the encoder able to
invalidate references (task 003, candidate 5).

### How the video track is used

- **Direct, no relay**: TURN is a relay server; none is used and none is
  added. STUN only lets the server find its public address by itself and is
  optional (`webrtc.stun_servers`); `webrtc.advertise` with a forwarded UDP
  port does the same by hand. Either way the browser sends straight to the
  server's address and port.
- **Order of attempts**: the UDP path is tried at every connection, as
  today. When it is up, motion-mode video uses it: no comparing round
  trips first (today the data channel is only chosen when it is clearly
  shorter, because a reliable channel over the same path gains nothing).
  When it does not come up within a few seconds, or dies (the existing
  dead-path detection), motion-mode video goes as today's frames over the
  data channel or the WebSocket.
- **Tile mode** keeps the reliable transports: lossless data needs every
  byte. Video areas inside tile mode (a video in a window, or a game in
  its first 10 s) therefore still ride on them.
- **Already tried, do not repeat**: an unreliable data channel with our
  own retransmission. The browser's SCTP counts what it abandons as loss
  and shrinks its window; 0 fps at 1 % loss (`docs/RESULTS.md`).

## Proposed steps

1. **Motion mode** over the transports that exist: the mode decision next
   to the capture loop, a path in `crates/core/src/shared.rs` and
   `stream.rs` that bypasses the tile policy, the exact picture after 5 s
   still, and a client that draws video only while the mode is on.
2. **Video over UDP** for motion mode, as open question 1 is answered,
   with the fallback order above.
3. **Per-client size and rate**: the client's report (debounced), the
   ladder, scalers in `crates/encoder`.
4. **Find the stage of what is left** (analysis, small, any time): put the
   server's own split into the late-frame report: waiting for the window,
   upload, encode (the client already has the three numbers per frame).
   The report is built in `client/src/worker.ts`. It tells whether the
   "almost all server" stalls are the GPU.
5. **If it is the GPU**: candidates 1, 2 and 4 of task 003 (process and
   thread priority, GPU priority, MMCSS for the compositor). Small.
   GPU priority above normal may need the elevated or service process.
6. **Estimate without bound** on short paths: reproduce on the current
   build and bound it (separate small fix, whatever the rest).

## Testing

- A real game on the Windows machine is the only faithful load; the user
  plays, the event log is read afterwards. `AGENTS.md`: do not type or
  click into the live session.
- For repeatable numbers: a GPU-bound load next to `rt-testapp --mode
  full` on a private instance, before and after steps 1 and 5, compared
  with the profiles in `docs/RESULTS.md`. Task 003 holds the baseline
  under load measured so far ("Measured under load").
- Loss: the Linux bench container with `netem loss` on UDP and on TCP
  separately (`tc` filters by protocol), 2 % Wi-Fi profile:
  late frames and worst capture-to-draw with the UDP video against the
  same video on the WebSocket. Also UDP blocked from the start and cut in
  the middle of a session: the stream must go on over the fallback.
- Mode switching is logic that can be judged without a machine: a unit
  test of the decision with the user's own case (10 s of video, some
  seconds of folders, again and again) must show no flip-flopping, plus
  the two thresholds and the reset of each count.
- Still picture: stop `rt-testapp` for 6 s and check the client's picture
  is pixel-exact; move again at 5.5 s and check the video resumes at once.
- Per-client size and rate: a small window and a 60 Hz client next to a
  full-size one, check what each receives; drag a window edge for some
  seconds and check one size change; shape the link down and up and check
  the ladder moves, never below the window.

## Not verified

- Which of the two readings of "server" time is the real one (step 4).
- How much delay the browser's own buffering adds to a WebRTC video track
  on each browser: measure it when step 2 is done, it is the one thing
  that could reopen the WebTransport alternative. Whether Safari has
  WebTransport with pinned certificates. Both from general knowledge, not
  tried here.
- That the stalls in the log happened during a match and not in the
  launcher or on the desktop.
- Decoder limits on the clients, and how Moonlight was configured
  (resolution, rate, bitrate) in the sessions the user compares with.
