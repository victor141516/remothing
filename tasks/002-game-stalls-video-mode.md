# Games: stalls of 200-450 ms, and a motion mode (whole screen as video)

- Status: in progress
- Created: 2026-11-10

## Progress

- Done: step 1, motion mode (`docs/ARCHITECTURE.md`, "Motion mode"). As
  built, entering the mode switches nothing by itself: a tile goes to video
  when it changes and stays there until the whole screen has been still
  for 5 s; tiles that are exact and do not change stay exact. The whole
  frame is not re-encoded from scratch either: the encoder input is updated
  where the screen changed, as before.
- Done: step 2, WebTransport (`docs/ARCHITECTURE.md`, "Transport"). As
  built, both modes use it (whatever video goes as pieces with parity; no
  frame is dropped without the existing repair). QUIC's congestion control
  is off and our link model paces, as planned. Not tried: a proxy that
  ends WebTransport (arrangement 3), Safari, a real phone or Wi-Fi.
- Done: step 3, size and frame rate per client (`docs/ARCHITECTURE.md`,
  "Motion mode"). As built, the frame-rate cap applies in both modes; the
  size only in motion mode. Slow start makes a new client begin at its
  window's size, so the larger-size hold is 0.5 s until the link is
  proven. Not checked: the GPU-scaled picture on Windows by eye or by
  decoding (only that it encodes, at what cost).
- Done: step 4, the late-frame report splits server time (wait, upload,
  encode, out). Reading it from a real game session is the user's part:
  it says whether the "almost all server" stalls are the link window or
  the GPU.
- Done: step 6, the estimate is bounded (`docs/ARCHITECTURE.md`, "Link
  model"; `docs/RESULTS.md`, "Estimate without bound"). The 4 Mbit/s
  squeeze after a path switch was not reproduced on the fixed build (the
  estimate is no longer raised by waits that are not the estimate's).
- Step 5, read from a real match (ARC Raiders at 3840x2160, the MacBook at
  120 Hz, 2026-11-11; numbers in `docs/RESULTS.md`, "A game at 4K for a
  120 Hz client"):
  - The scheduling of the Sunshine study is on and right while a client is
    connected: the server's GPU class reads realtime from outside, the
    game's normal.
  - Done: the steady `wait` (7-10 ms) was pacing. Frames were made for 60
    a second and came at up to 120, each twice the size its share of the
    link has room for; the encoder's cap is now for the client's frame
    rate. This also halves what the client decodes per frame.
  - Left: the GPU. Next to the game every wait for the GPU costs 3-5 ms
    (median) even in the realtime class, and encode takes 6-9 ms instead
    of 3. Not tried: fewer waits per frame (the encoder is handed BGRA and
    converts it in a step of its own), HAGS on, the game's frame rate
    capped so the GPU keeps some room.
  - Left: the stalls of 150-600 ms (`wait` or `encode` of one frame) are
    still in the event log, a few a minute. Not explained. Video memory
    was at 14.5 of 16.4 GB during the match; whether that is it was not
    looked into.
  - Not built: a limit on frame size for a client that needs longer per
    frame than frames are apart. Measured with a script as client (10 ms
    per frame at 120 Hz): capture to draw 25 ms instead of 4, since every
    frame waits at the server for the one before to be drawn. The MacBook
    was not in that state in the match (frames came 15-25 ms apart).

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
- **One UDP transport: WebTransport datagrams**, because the video stays
  in our own pipeline and under our control, which is closest to what
  Sunshine does. If it cannot connect, the transports of today. No WebRTC
  video track: it needs the same of the network as WebTransport (a UDP
  port of the server the client can reach), so it would fail in the same
  places.
- **WebTransport never asks the user for a certificate of its own.** Its
  certificate is self-signed, made by the server, and trusted only
  because the page hands its hash to the browser. So it depends on the
  page being trusted, however that came about: a valid certificate (Let's
  Encrypt through Caddy) or the browser's warning accepted once for our
  self-signed HTTPS. No second ACME challenge, no certificate files to
  configure.
- **Safari is assumed to follow the standard**, pinned certificates
  included: the design does not plan around a browser that refuses them,
  and nothing special is built for Safari. If testing shows a browser the
  user cares about refusing them, that is to be discussed with the user
  before anything requires a CA certificate.
- **Behind a reverse proxy with a valid certificate** (Caddy, for example)
  WebTransport must work too, as far as that is possible. What is
  possible is under "With a reverse proxy" below.
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

The Sunshine study (task 003, finished and deleted; what came of it is in
`docs/ARCHITECTURE.md` and `docs/RESULTS.md`, native NVENC in task 006)
numbered the candidates below. The parts that match what was found here:

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

Decided: UDP first, direct, as WebTransport datagrams; then what exists.

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
| browsers | all, phones included | Chrome, Firefox, and Safari since March 2026 (MDN lists it as newly available in all of them); older Safari and iOS do not have it |
| certificate | nothing new | a CA certificate, or a self-signed one pinned by hash and renewed every two weeks |
| our pipeline | the browser shows the video (a `<video>` element), outside our decoder and compositor: fits motion mode, where the whole picture is video | stays ours: decoder, compositor, link model |
| cost | medium: media in str0m, keyframes on request, bitrate from its feedback | very high: a third transport, plus error correction in the client |
| main risk | the browser's own buffering adds delay; to be measured (cloud gaming in browsers uses this path) | each loss the redundancy cannot repair costs a keyframe until the encoder can invalidate references (task 006, native NVENC) |

### WebTransport datagrams

What it gives that a WebRTC video track would not:

- **Possibly less delay.** The video track goes through the browser's own
  buffer, which holds frames a little to smooth the network; datagrams
  end in our decoder, which shows each frame as soon as it is complete.
  Neither delay is measured yet.
- **Video in tile mode too.** Frames stay in our compositor, in step with
  the lossless tiles, so the video areas of tile mode also travel without
  retransmission stalls, not only motion mode.
- **Our own link model** (`pacer.rs`) sets the sending rate from the
  client's ACKs, as today.

What has to be built:

- An HTTP/3 (QUIC) listener on a UDP port. The client must reach that
  port directly (a forwarded port, as for today's UDP); unlike WebRTC it
  cannot open a path through a NAT in front of the server.
- A certificate, see "Certificate" below.
- Loss handling, ours on both sides: a frame is cut into datagrams; a
  little redundancy rebuilds a single lost one; the client asks again for
  what is still missing (one round trip); a keyframe is the last resort.
  Dropping a frame without a keyframe needs an encoder that can
  invalidate references (task 006, native NVENC).
- In the client's worker: putting frames together again and the
  redundancy decoding, in front of today's decoder.
- Lossless data stays on a reliable channel (the WebSocket, or a reliable
  stream of the same WebTransport session).

### Certificate

Without options a browser checks a WebTransport server like any HTTPS
site against the public certificate authorities, and there is no warning
page to click through: the connection just fails. Accepting our
self-signed HTTPS certificate on the page is not expected to carry over
to it (not tried here; check it in the first test, it costs nothing).

The API has its own way for servers without a CA certificate, and it
needs no click: the page tells the browser the SHA-256 hash of the one
certificate to accept (`serverCertificateHashes`).

- The server makes a certificate only for WebTransport: ECDSA P-256,
  valid less than two weeks (the limit browsers set for pinned ones), and
  makes the next one before it expires.
- The client gets the hash over the WebSocket it already has, so the pin
  is as trustworthy as the page itself (reached through HTTPS the user
  accepted, or through the proxy).
- With `tls.cert` / `tls.key` set to a CA certificate that matches the
  name the client connects to, that one is used and no hash is sent.
- Pinned certificates are part of the WebTransport standard (MDN documents
  them) and every browser is assumed to honour them, Safari too (the
  user's decision). It is still checked on real browsers when this
  exists, because nobody tried it here. A browser that refuses stays on
  today's transports and the client says why; what to do then is the
  user's call (see "Decided").

### With a reverse proxy

The user's case: the page is served through a proxy such as Caddy, which
holds a valid certificate and talks plain HTTP to our server.

How it differs from HTTPS: WebTransport runs over QUIC, and QUIC is TLS
built into UDP. The certificate works as with HTTPS (it belongs to a
name, not to TCP), but QUIC has no unencrypted form. So "the proxy holds
the certificate and the server behind it speaks plain" does not exist
for WebTransport: whoever ends the QUIC connection needs a certificate,
and a proxy that ends it has to open a second QUIC connection to us.

Three arrangements, in the order we would recommend them:

1. **The page through the proxy, the video straight to the server.** The
   client loads the page and the WebSocket through the proxy, gets the
   hash there, and opens WebTransport to the server's own UDP port with
   the pinned certificate. The proxy's certificate is not involved and
   the proxy needs no UDP support. Needs the server's UDP port reachable
   by the client, as today's UDP does (`webrtc.advertise` has the same
   role).
2. **The proxy host forwards the UDP port as it is** (a plain UDP forward:
   the system's own, or Caddy's layer-4 plugin), for a server the client
   cannot reach directly. QUIC passes untouched, still encrypted end to
   end with the pinned certificate.
3. **The proxy ends WebTransport with its certificate and opens another
   session to us.** Caddy has this only as an experimental pull request
   (caddyserver/caddy #7669, passthrough of WebTransport in
   `reverse_proxy`); the connection to us is still QUIC, with our
   certificate left unchecked by the proxy. It puts the proxy's queues
   between our pacing and the client, which is the control WebTransport
   was chosen for.

What we build for it: a setting for where clients open WebTransport
(default: the page's host on our UDP port; else an address and port as in
1 and 2, or a full `https://` URL as in 3, in which case no hash is sent
and the browser checks the proxy's certificate as usual). 1 and 2 are
tested in the container; 3 is allowed by the setting and not tested until
a proxy that does it is at hand.

### Alternative left out: WebRTC video track

The browser's own real-time video path, over the UDP port the data channel
already uses (first column of the table). Left out because it comes up
and fails together with WebTransport, would be a second stack to keep
working, and only serves motion mode (the browser shows the video itself,
outside our compositor). It would only add: browsers without
WebTransport (Safari and iOS before March 2026), browsers that refuse the
pinned certificate, and servers reachable only through a path WebRTC
opens by itself through a NAT. Look at it again if the event log shows
those cases matter.

### Order of attempts

1. WebTransport, tried at every connection. When it is up, video goes over
   it in both modes: no comparing round trips first (today the data channel
   is only chosen when it is clearly shorter, because a reliable channel
   over the same path gains nothing).
2. Not up within a few seconds, or dead later: today's frames over the
   data channel or the WebSocket.

The client reports which one it ended on and why WebTransport failed
(event log), like every optional feature.

Already tried, do not repeat: an unreliable WebRTC data channel with our
own retransmission. The browser's SCTP counts what it abandons as loss and
shrinks its window; 0 fps at 1 % loss (`docs/RESULTS.md`). With QUIC the
sending rate is decided by our server, so the same idea is sound there.

## Proposed steps

1. **Motion mode** over the transports that exist: the mode decision next
   to the capture loop, a path in `crates/core/src/shared.rs` and
   `stream.rs` that bypasses the tile policy, the exact picture after 5 s
   still, and a client that draws video only while the mode is on.
2. **Video over WebTransport datagrams**, both modes, with the pinned
   certificate and the fallback.
3. **Per-client size and rate**: the client's report (debounced), the
   ladder, scalers in `crates/encoder`.
4. **Find the stage of what is left** (analysis, small, any time): put the
   server's own split into the late-frame report: waiting for the window,
   upload, encode (the client already has the three numbers per frame).
   The report is built in `client/src/worker.ts`. It tells whether the
   "almost all server" stalls are the GPU.
5. **If it is the GPU**: candidates 1, 2 and 4 of the Sunshine study (process and
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
  with the profiles in `docs/RESULTS.md`, which holds the baseline under
  load measured so far ("Host under load").
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
- How much delay our own path over datagrams has: measure it when it
  exists.
- That every browser accepts a certificate pinned by hash for
  WebTransport. Assumed, by the user's decision, for Safari as well; MDN
  lists WebTransport as available in Safari since March 2026 but nothing
  was run here. Also that an HTTPS exception the user accepted does not
  apply to WebTransport (general knowledge, not tried).
- WebTransport through a proxy that ends it (arrangement 3). The Caddy
  pull request was only found by search, not read or run.
- That the stalls in the log happened during a match and not in the
  launcher or on the desktop.
- Decoder limits on the clients, and how Moonlight was configured
  (resolution, rate, bitrate) in the sessions the user compares with.
