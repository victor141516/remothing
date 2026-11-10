# Games: stalls of 200-450 ms, and a full-screen video mode

- Status: under discussion
- Created: 2026-11-10

## Goal

Playing a game through the client goes badly: frame-rate drops and lag. The
same games on a phone through Sunshine do not show it. The user wants to
know the cause, and asks whether streaming the whole screen as lossy video,
with no composition and no detection of changed areas, would solve it.

## Open questions

1. **Which client and network had the problem?** The session in the log was
   a client 4 ms away on the WebSocket with 2 ms decode times, which looks
   like a computer; the Sunshine comparison was a phone. On a phone the
   decoder limit below may matter as much as the stalls.
2. **Order of work**: measure first which stage stalls (step 1 below, small)
   and then fix what it shows, or build the video-only mode right away?
   Recommended: measure first. The video-only mode does not touch the two
   most likely causes.
3. **Video-only mode, how is it switched on?** A button in the client bar, a
   server setting, automatic when the whole screen has been video for some
   seconds, or several of these?
4. **Video-only mode, when the picture stops**: stay lossy (as Sunshine),
   or refine to pixel-exact after a pause as today? The second keeps text
   sharp in menus and costs a burst of lossless data each time.
5. **Resolution and frame rate per client**: may the game mode scale the
   picture down and cap the rate to what the client asks for (for example
   1080p at 60 or 120)? Today every client gets the display's own size at
   the capture rate.
6. **Transport without retransmission stalls** (a project of its own, see
   `docs/SUNSHINE.md` item 10): decide only after step 1 shows how much of
   the problem is the network.

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
- 3168x1440 at 144 fps is 2.57 million macroblocks per second; H.264 level
  5.2 ends at 2.07 million (about 116 fps at this size). Hardware decoders
  in phones may not keep up above that. Not checked on any phone.

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

## The user's idea: whole screen as lossy video

In a game every tile changes every frame, so the whole screen already goes
through the video encoder; the lossless layer only carries what stops
moving. A video-only mode would remove:

- the per-frame tile comparison, whose result is read back from the GPU
  each frame (a point where the capture thread waits for a busy GPU);
- the lossless bursts when part of the picture stops (HUD, menus, pause),
  which compete with the video for the link and cost CPU on both sides;
- tiles moving between the two layers.

It would not remove a TCP retransmission stall, nor the encoder waiting for
a GPU the game keeps busy. So it is worth having as a game mode, but alone
it is unlikely to make the stalls go away.

## Proposed steps

1. **Find the stage.** Put the server's own split into the late-frame
   report: waiting for the window, upload, encode (the client already has
   the three numbers per frame), and whether a frame was waiting for an
   ACK. Then play a real match for some minutes and read the event log.
   Small: the report is built in `client/src/worker.ts`.
2. **If it is the GPU**: items 1, 2 and 4 of `docs/SUNSHINE.md` (process
   and thread priority, GPU priority, MMCSS for the compositor). Small.
   GPU priority above normal may need the elevated or service process.
3. **If it is TCP**: no cheap fix. Options are a WebRTC video track (the
   browser brings RTP, loss recovery and its own jitter buffer; `rtc.rs`
   uses str0m for data channels only today) or WebTransport datagrams with
   our own error correction (`docs/SUNSHINE.md` item 10). Either is a
   third transport and a task of its own.
4. **Video-only mode**, as decided in open questions 3-5: the whole frame
   to the encoder on every capture, no tile comparison, no lossless layer
   while it is on, optional downscale and frame-rate cap per client, steady
   bitrate. Needs a path in `crates/core/src/shared.rs` and `stream.rs`
   that bypasses the tile policy, and a client that draws video only.
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

## Not verified

- Which of the two readings of "server" time is the real one. Everything
  after step 1 depends on it.
- That the stalls in the log happened during a match and not in the
  launcher or on the desktop.
- Phone decoder limits, and how Moonlight was configured (resolution, rate,
  bitrate) in the sessions the user compares with.
