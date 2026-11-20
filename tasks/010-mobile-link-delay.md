# Link model on links whose delay wanders (phones on 5G)

- Status: agreed
- Created: 2026-11-20

## Goal
A phone on 5G gets the frame rate its link carries. Today it gets half of
the refreshes or less, because the link model cuts its estimate to the
floor on delay the link adds whatever we send.

## Decisions
- The server backs off whenever what reaches the client suffers, also for
  queues it did not cause (another device downloading on the same Wi-Fi),
  down to a rate at which the stream works; and it must not stay down: it
  tries again periodically (every 30 s to a minute) and returns to the
  higher bitrate once the connection is good again (user, 2026-11-20).

## Open questions

## What was measured
OnePlus CPH2653 on 5G (Wi-Fi off, through the router's forwarded ports),
bench container `--mode full --fps 120` at 1920x1080, AV1, 20 s per
transport, traces in `.tools/5g-session.json` (git-ignored):

| transport | frames/s | refreshes with a new picture | network p50 / p99 ms | estimate |
|---|---|---|---|---|
| WebTransport | 70 | 51 % | 17.6 / 94 | 1 Mbit/s |
| WebRTC | 78 | 56 % | 17.6 / 83 | 2 Mbit/s |
| WebSocket | 66 | 48 % | 19.0 / 157 | 1 Mbit/s |

The same phone on Wi-Fi: 114 frames/s, 92 % of refreshes, network p99
11-13 ms on all three transports. On 5G the events show "link estimate cut
to 0.9 Mbit/s (queue 37 ms)" every second or two. While we sent 0.5-0.9
Mbit/s, the one-way delay per second still ranged p10 9-20 ms, p90 30-110
ms: a delay floor that settles at a new level for hundreds of ms, not a
queue of ours. Measured from the 10 s minimum RTT it reads as a queue over
QUEUE_LIMIT_US; cuts take the estimate to MIN_RATE, and growth needs a
queue under TARGET_QUEUE_US, which never comes.

## Reproduction in the simulator
Give `tests::Link` a delay level drawn anew every period, added to every
ACK (amplitude, period), on 50 Mbit/s, 15 ms, with 6 KB frames at 120 fps
(not paced), 30 s:

| level up to / every / jitter | estimate | fps |
|---|---|---|
| 40 ms / 300 ms / 10 ms | 12.6 Mbit/s | 86 |
| 60 ms / 300 ms / 30 ms | 1.6-1.7 Mbit/s | 38-41 |
| 60 ms / 100 ms / 30 ms | 1.2-1.8 Mbit/s | 37-40 |
| 100 ms / 500 ms / 40 ms | 3.0-3.9 Mbit/s | 33-35 |

(the same from the default start and from a learned 30 Mbit/s estimate).

## Tried and dropped
Queue measured from the lowest window delay of the last second instead of
the 10 s minimum, the send window covering that floor: 40 ms of wander got
116 fps, 60 ms still collapsed, and two tests broke. A queue of our own held
steady by the send window (`light_content_still_detects_a_slow_link`) was
absorbed into the floor, and with ACKs seconds apart each window became its
own floor (`rate_drop_is_detected`). Delay alone cannot tell a steady queue
of ours from the radio's.

## Design (agreed)
- Cut only when the cut can be seen to work: if the delay does not fall in
  the round trips after a cut while we send less, it is the path's (a 5G
  radio): undo the cut and take that delay as the path's floor until it
  falls again. A queue that does fall after a cut, whoever else fills the
  link, keeps the estimate down.
- Every 30 s, while the estimate sits below where it was before its cuts,
  try the higher level again for a few round trips; keep it if the queue
  stays short, go back down otherwise. A link that recovers (the other
  download ended) gets its bitrate back within 30 s, without waiting for
  growth that a short-queue signal alone may never allow.
- Not pursued: bounding the queue by the bytes we have in flight. It
  ignores queues others build, which the user wants the server to yield to.
- Tests: the simulator cases above, a link that drops to a third for 20 s
  and comes back (recovered within 30 s), all current pacer tests passing,
  and the bad-link runs of RESULTS ("Bad links", "Stutters") repeated in
  the container before and after.
