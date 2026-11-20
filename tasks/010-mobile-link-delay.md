# Link model on links whose delay wanders (phones on 5G)

- Status: under discussion
- Created: 2026-11-20

## Goal
A phone on 5G gets the frame rate its link carries. Today it gets half of
the refreshes or less, because the link model cuts its estimate to the
floor on delay the link adds whatever we send.

## Open questions
- Should the server keep backing off for queues it did not cause (another
  device downloading on the same Wi-Fi)? A queue-only signal yields to them;
  any fix that ignores delay we cannot have caused stops yielding.

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

## Directions
- Bound the queue a frame can have met behind our own traffic: the bytes
  ahead of it when it was sent, over the delivery rate measured (not the
  estimate, which a slow link would make too high). Delay beyond that is
  not ours.
- Cut only when the cut can be seen to work: if the delay does not fall in
  the round trips after a cut while we send less, take it as the path's,
  undo the cut and raise the floor.
- Either way: the simulator cases above as tests, all current pacer tests
  passing, and the bad-link runs of RESULTS ("Bad links", "Stutters")
  repeated in the container before and after.
