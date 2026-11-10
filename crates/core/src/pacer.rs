//! Per-client link model: estimates bandwidth and round-trip time from frame
//! ACKs and decides when the next frame may go out.
//!
//! The client ACKs every frame once drawn, so an ACK covers the path plus
//! client processing. Each ACK's delay beyond the base (minimum) RTT and the
//! frame's own transmission time is queueing plus jitter. Over a judging
//! window of about one base RTT the least of those delays is the standing
//! queue; single late ACKs are path jitter (relays, Wi-Fi) or client hiccups.
//!
//! The estimate (bytes/s) moves on that signal, as delay-based congestion
//! control does (LEDBAT): each window it changes by up to ±GAIN in
//! proportion to (target - queue) / target. Growth needs content that waited
//! for the estimate (byte window full or pacing deferred something; not
//! frames waiting on the client): screen content paced from the estimate
//! never exceeds it, so measured rates alone could not raise it again after
//! a cut. Near the target the steps shrink, so the queue signal's round-trip
//! lag causes little overshoot. A queue past the limit (link got slower,
//! overshoot) cuts at once to below the delivery rate seen, so it drains;
//! ACKs of the next round trip still show the old queue and cause no
//! further cut.
//!
//! Content below the estimate (most desktop use) weakens the signal: the
//! queue is not caused by the estimate, and a window holds one or two ACKs
//! whose minimum delay is mostly jitter. Far below it the window waits for
//! more ACKs; decreases made while nothing waited are undone once the queue
//! is short again (up to the level reached with content filling the link);
//! and the cut needs the queue to persist. Without that the estimate
//! drifted down to whatever the content happened to use.
//!
//! Measured rates raise it directly:
//! - delivery rate, as in BBR: bytes ACKed over the longer of the send and
//!   ACK spans since the frame's predecessor was delivered;
//! - transmission time: a frame sent into an empty pipe takes base RTT plus
//!   size / bandwidth, so size / (RTT - base RTT) once that excess clearly
//!   exceeds jitter (large frames on slow links).
//!
//! It starts low and, until the link has queued once, doubles every window
//! in which content used it and the queue stayed empty (slow start): content
//! is planned against the estimate, so a slow link never gets a burst sized
//! for a fast one, and a fast one is found within a few round trips.
//!
//! The send window covers base RTT plus typical jitter at the estimated rate,
//! with 10 % margin: the pipe stays busy, so frame rate is not bound by RTT,
//! and when content outruns the link the queue it holds stays short.

use std::collections::VecDeque;

/// Until measured: little traffic (see slow start).
const INITIAL_RATE: f64 = 5e6 / 8.0;
/// Growth per window of slow start.
const SLOW_START: f64 = 2.0;
/// Content delivering at least this share of the estimate tests it as much
/// as content that waited for it.
const BUSY_SHARE: f64 = 0.5;
const MIN_RATE: f64 = 1e6 / 8.0;
/// One sample can at most double the estimate (guards against ACK bunching).
const MAX_GROWTH: f64 = 2.0;
/// Largest change per judging window of the proportional controller. Far
/// below the rate of the last cut (a deep cut, a path change), growth with
/// an empty queue accelerates by ACCEL_PER_WINDOW per window up to MAX_ACCEL
/// times, so it recovers in about a second; near it, steps stay small.
const GAIN: f64 = 0.04;
const ACCEL_PER_WINDOW: f64 = 0.25;
const MAX_ACCEL: f64 = 4.0;
/// Queue below this counts as empty for acceleration.
const EMPTY_QUEUE_US: u64 = 4_000;
/// Standing queue the controller steers to.
const TARGET_QUEUE_US: f64 = 12_000.0;
/// Shortest judging window.
const JUDGE_MIN_US: u64 = 40_000;
/// With content far below the estimate ACKs are sparse (one per frame,
/// frames as the screen changes): the window then waits for this many, or
/// this long, so its minimum delay is a queue and not one late ACK. Content
/// above SPARSE_SHARE of the estimate is judged at once: its ACKs are few
/// because its frames are large, and the queue they show is its own.
const JUDGE_SPARSE_ACKS: u64 = 6;
const JUDGE_SPARSE_US: u64 = 250_000;
const SPARSE_SHARE: f64 = 0.25;
/// Base RTT older than this is re-learned (routes change).
const BASE_RTT_WINDOW_US: u64 = 10_000_000;
/// Before the first ACK.
const DEFAULT_RTT_US: u64 = 20_000;
/// Standing queue beyond this (or half the base RTT) cuts the estimate.
const QUEUE_LIMIT_US: u64 = 30_000;
/// While content stays below the estimate, a queue beyond the limit cuts
/// only once it has lasted this long: a radio hiccup (Wi-Fi scan, roaming)
/// delays a few ACKs, congestion lasts as long as we keep sending.
const PERSIST_US: u64 = 200_000;
/// RTT excess below this is jitter, too small for a transmission-time sample.
const JITTER_US: u64 = 6_000;
/// Window beyond the bandwidth-delay product: margin and slack.
const WINDOW_MARGIN: f64 = 1.1;
const WINDOW_SLACK_US: f64 = 4_000.0;
/// Bookkeeping bound; tiny frames on long paths never need more in flight.
const MAX_FRAMES: usize = 64;

struct Sent {
    seq: u32,
    bytes: u64,
    sent_us: u64,
    /// The pipe was empty when it was sent.
    alone: bool,
    delivered: u64,
    delivered_us: u64,
    first_sent_us: u64,
}

pub struct Pacer {
    sent: VecDeque<Sent>,
    inflight: u64,
    delivered: u64,
    delivered_us: u64,
    /// Send time of the most recently delivered frame.
    first_sent_us: u64,
    /// Minimum RTT: (value, measured at); 0 = unknown.
    base_rtt: (u64, u64),
    rate: f64,
    /// Smoothed spread of ACK delays above their per-window minimum (µs).
    jitter_us: f64,
    /// Last emergency cut: time and the round trip until its effect shows.
    cut_us: u64,
    cut_hold_us: u64,
    /// An emergency cut not reported yet: (new estimate, queue seen in µs).
    cut_report: Option<(f64, u64)>,
    /// Consecutive judging windows of growth with an empty queue.
    up_streak: u32,
    /// Estimate just before the last emergency cut (0: never cut).
    ceiling: f64,
    /// Estimate reached while our own traffic filled the link (growth by
    /// probing, measured rates, cuts with content waiting). Decreases while
    /// content stays below the estimate are weaker evidence: with a short
    /// queue the estimate floats back up to this level without probing.
    backed: f64,
    /// Start of the current run of judging windows over the queue limit (0: none).
    over_since_us: u64,
    /// Current judging window: start, bytes delivered before it, least /
    /// summed / counted ACK delay beyond base RTT and own transmission, best
    /// delivery sample, and whether content waited for the estimate.
    judge_start_us: u64,
    judge_delivered: u64,
    judge_min_us: u64,
    judge_sum_us: u64,
    judge_n: u64,
    judge_best: f64,
    judge_limited: bool,
}

impl Default for Pacer {
    fn default() -> Self {
        Self {
            sent: VecDeque::new(),
            inflight: 0,
            delivered: 0,
            delivered_us: 0,
            first_sent_us: 0,
            base_rtt: (0, 0),
            rate: INITIAL_RATE,
            jitter_us: 0.0,
            cut_us: 0,
            cut_hold_us: 0,
            cut_report: None,
            up_streak: 0,
            ceiling: 0.0,
            backed: INITIAL_RATE,
            over_since_us: 0,
            judge_start_us: 0,
            judge_delivered: 0,
            judge_min_us: u64::MAX,
            judge_sum_us: 0,
            judge_n: 0,
            judge_best: 0.0,
            judge_limited: false,
        }
    }
}

impl Pacer {
    /// Estimated link bandwidth, bytes/s.
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// The link has queued at least once, so the estimate is near its real
    /// capacity. Before that it is only a lower bound: content that never
    /// filled the link says nothing about how much more it would carry.
    pub fn proven(&self) -> bool {
        self.ceiling > 0.0
    }

    /// The model for the same client over another path (WebSocket ↔ data
    /// channel): round trips are measured afresh, the estimate carries over
    /// as the starting point, since most of the route (the last mile) is
    /// shared. `proven` keeps content planned within it, as a fallback path
    /// must be; without it the new path may show it carries more.
    pub fn for_new_path(&self, proven: bool) -> Self {
        Self {
            rate: self.rate,
            backed: self.backed,
            ceiling: if proven { self.ceiling } else { 0.0 },
            ..Default::default()
        }
    }

    /// Minimum ACK round trip, µs (0 before the first ACK).
    pub fn base_rtt_us(&self) -> u64 {
        self.base_rtt.0
    }

    /// Round trip the window covers: base RTT plus typical jitter, µs.
    fn path_us(&self) -> f64 {
        if self.base_rtt.0 == 0 { DEFAULT_RTT_US as f64 } else { self.base_rtt.0 as f64 + self.jitter_us }
    }

    fn window(&self) -> f64 {
        self.rate * (self.path_us() * WINDOW_MARGIN + WINDOW_SLACK_US) / 1e6
    }

    pub fn can_send(&self, now: u64) -> bool {
        let Some(oldest) = self.sent.front() else { return true };
        // Two frames always (as many as one round trip of a fast path needs).
        // Beyond that, the oldest unacknowledged frame must not be overdue: a
        // client that stalls (busy decoding, GC, hidden tab) would otherwise
        // collect a backlog of stale frames, while frames held back here
        // coalesce into one current frame (pull model).
        let due = self.path_us() * WINDOW_MARGIN + WINDOW_SLACK_US + oldest.bytes as f64 * 1e6 / self.rate;
        let overdue = self.sent.len() >= 2 && now.saturating_sub(oldest.sent_us) as f64 > due;
        self.sent.len() < MAX_FRAMES && (self.inflight as f64) < self.window() && !overdue
    }

    /// The estimate is what holds content back: the bytes in flight fill the
    /// window. Frames waiting on a client that has not drawn the previous
    /// ones yet (overdue) say nothing about the link, and counting them made
    /// the estimate grow without bound on short paths.
    pub fn window_full(&self) -> bool {
        self.inflight as f64 >= self.window()
    }

    /// Content waited for the estimate: the window was full or pacing
    /// deferred something. Lets the estimate grow while queues stay short.
    pub fn note_limited(&mut self) {
        self.judge_limited = true;
    }

    /// The last emergency cut, once: (new estimate in bytes/s, queue in µs).
    pub fn take_cut(&mut self) -> Option<(f64, u64)> {
        self.cut_report.take()
    }

    pub fn on_send(&mut self, seq: u32, bytes: u64, now: u64) {
        let alone = self.sent.is_empty();
        if alone {
            self.delivered_us = now;
            self.first_sent_us = now;
            if self.judge_n == 0 {
                // Nothing in flight and no ACK yet in this judging window: it
                // starts here, not when the previous one ended (idle time
                // would count as the estimate going unused).
                self.judge_start_us = now;
                self.judge_delivered = self.delivered;
            }
        }
        self.inflight += bytes;
        self.sent.push_back(Sent {
            seq,
            bytes,
            sent_us: now,
            alone,
            delivered: self.delivered,
            delivered_us: self.delivered_us,
            first_sent_us: self.first_sent_us,
        });
    }

    /// The client drew frame `seq` (ACKs arrive in send order) after holding
    /// it `hold_us` (decode, decompression, upload). The window counts the
    /// frame until now, so a slow client still holds content back, but the
    /// link model sees only the network: a heavy frame's processing is not
    /// queueing on the path.
    pub fn on_ack(&mut self, seq: u32, now: u64, hold_us: u64) {
        // Unknown seq: a frame forgotten by `forget_inflight` (display switch).
        let Some(n) = self.sent.iter().position(|f| f.seq == seq) else { return };
        let mut acked = None;
        for f in self.sent.drain(..=n) {
            self.inflight -= f.bytes;
            self.delivered += f.bytes;
            acked = Some(f);
        }
        let f = acked.expect("drained at least one frame");
        let arrived = now.saturating_sub(hold_us).max(f.sent_us + 1);
        self.delivered_us = arrived;
        self.first_sent_us = f.sent_us;
        self.sample(&f, arrived);
    }

    /// Frames in flight will never be ACKed meaningfully (display switch).
    pub fn forget_inflight(&mut self) {
        self.sent.clear();
        self.inflight = 0;
    }

    /// Frame `seq` will never be acknowledged (lost with an abandoned data channel).
    pub fn on_lost(&mut self, seq: u32) {
        if let Some(n) = self.sent.iter().position(|f| f.seq == seq) {
            if let Some(f) = self.sent.remove(n) {
                self.inflight -= f.bytes;
            }
        }
    }

    fn sample(&mut self, f: &Sent, now: u64) {
        let rtt = now.saturating_sub(f.sent_us).max(1);
        // Raw minimum: it includes the smallest frame's transmission, but never
        // depends on the rate estimate (subtracting an estimated transmission
        // time would make any rate self-consistent).
        if self.base_rtt.0 == 0 || rtt <= self.base_rtt.0 || now - self.base_rtt.1 > BASE_RTT_WINDOW_US {
            self.base_rtt = (rtt, now);
        }
        let base = self.base_rtt.0;

        let interval = f.sent_us.saturating_sub(f.first_sent_us).max(now.saturating_sub(f.delivered_us));
        let mut sample = if interval > 0 { (self.delivered - f.delivered) as f64 * 1e6 / interval as f64 } else { 0.0 };
        let excess = rtt - base;
        if f.alone && excess >= JITTER_US.max(base / 4) {
            sample = sample.max(f.bytes as f64 * 1e6 / excess as f64);
        }
        if sample > self.rate {
            self.rate = sample.min(self.rate * MAX_GROWTH);
            self.backed = self.backed.max(self.rate);
        }

        let own = (f.bytes as f64 * 1e6 / self.rate) as u64;
        let delay = rtt.saturating_sub(base + own);
        self.judge_min_us = self.judge_min_us.min(delay);
        self.judge_sum_us += delay;
        self.judge_n += 1;
        self.judge_best = self.judge_best.max(sample);
        let elapsed = now.saturating_sub(self.judge_start_us);
        let window = base.max(JUDGE_MIN_US);
        // Share of the estimate the content used in this window.
        let used = (self.delivered - self.judge_delivered) as f64 / (self.rate * elapsed.max(1) as f64 / 1e6);
        let held = used < SPARSE_SHARE && !self.judge_limited;
        if elapsed >= window && !(held && self.judge_n < JUDGE_SPARSE_ACKS && elapsed < JUDGE_SPARSE_US) {
            // A window held open until it had enough ACKs stands for as
            // many windows as it lasted; one that timed out with a few
            // ACKs is no stronger than a single one.
            let windows = match held && self.judge_n >= JUDGE_SPARSE_ACKS {
                true => (elapsed / window).min(JUDGE_SPARSE_US / JUDGE_MIN_US) as i32,
                false => 1,
            };
            self.judge(base, now, windows, used);
            self.judge_start_us = now;
            self.judge_delivered = self.delivered;
            self.judge_min_us = u64::MAX;
            self.judge_sum_us = 0;
            self.judge_n = 0;
            self.judge_best = 0.0;
            self.judge_limited = false;
        }
        self.rate = self.rate.max(MIN_RATE);
    }

    /// `used`: share of the estimate delivered in the window.
    fn judge(&mut self, base: u64, now: u64, windows: i32, used: f64) {
        let queue = self.judge_min_us;
        let spread = (self.judge_sum_us / self.judge_n - queue) as f64;
        self.jitter_us += (spread - self.jitter_us) / 4.0;
        let over = queue > QUEUE_LIMIT_US.max(base / 2);
        if !over {
            self.over_since_us = 0;
        } else if self.over_since_us == 0 {
            self.over_since_us = self.judge_start_us.max(1);
        }
        if now.saturating_sub(self.cut_us) < self.cut_hold_us {
            // ACKs of frames sent before the last cut: no news yet.
            return;
        }
        // Content that waited for the estimate or used a good part of it
        // tests it: a queue is its own, an empty one means there is room.
        // Below that the queue is not (only) ours, and with few ACKs per
        // window its minimum is mostly jitter.
        let pushing = self.judge_limited || used >= BUSY_SHARE;
        if over && (pushing || now.saturating_sub(self.over_since_us) >= PERSIST_US) {
            // Below the best delivery rate seen, so the queue drains; at most
            // 30 %: an app-limited best can be far below the link.
            self.ceiling = self.rate;
            self.rate = (self.rate * 0.85).min(self.judge_best * 0.9).max(self.rate * 0.7);
            self.cut_report = Some((self.rate, queue));
            self.cut_us = now;
            self.cut_hold_us = base + queue + JUDGE_MIN_US;
            self.up_streak = 0;
        } else {
            let off = ((TARGET_QUEUE_US - queue as f64) / TARGET_QUEUE_US).clamp(-1.0, 1.0);
            let recovering = !pushing && self.rate < self.backed;
            if off < 0.0 {
                self.rate *= (1.0 + GAIN * off).powi(windows);
            } else if pushing && self.ceiling == 0.0 {
                // Slow start: up to twice what the link just carried, so the
                // estimate never runs ahead of traffic that does not follow
                // it (a capped encoder, light content); less as the queue
                // nears its target.
                let room = (SLOW_START * used - 1.0).clamp(GAIN, SLOW_START - 1.0);
                self.rate *= 1.0 + room * off;
            } else if pushing || recovering {
                let accel = (1.0 + self.up_streak as f64 * ACCEL_PER_WINDOW).min(MAX_ACCEL);
                self.rate *= (1.0 + GAIN * off * accel).powi(windows);
                if recovering {
                    self.rate = self.rate.min(self.backed);
                }
            }
            // Far below where the estimate has been: after a deep cut with
            // content pushing, or after decreases nothing backed.
            let far_below = self.rate < 0.8 * if pushing { self.ceiling } else { self.backed };
            if (pushing || recovering) && queue < EMPTY_QUEUE_US && far_below {
                self.up_streak += 1;
            } else {
                self.up_streak = 0;
            }
        }
        if pushing {
            self.backed = self.rate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIFO bottleneck of `rate` bytes/s with `rtt_us` base round trip, plus
    /// up to `jitter_us` of extra ACK delay (deterministic, order kept), and
    /// `hold_us` the client keeps each frame before drawing (ACKing) it.
    struct Link {
        rate: f64,
        rtt_us: u64,
        jitter_us: u64,
        hold_us: u64,
        busy_until: u64,
        last_ack: u64,
        lcg: u64,
        /// Longest wait of a frame behind earlier ones, over the whole run.
        worst_queue_us: u64,
    }

    impl Link {
        fn new(rate: f64, rtt_us: u64, jitter_us: u64) -> Self {
            Self { rate, rtt_us, jitter_us, hold_us: 0, busy_until: 0, last_ack: 0, lcg: 1, worst_queue_us: 0 }
        }

        /// ACK time of a frame handed to the link at `now`.
        fn deliver(&mut self, bytes: u64, now: u64) -> u64 {
            let start = self.busy_until.max(now);
            self.worst_queue_us = self.worst_queue_us.max(start - now);
            self.busy_until = start + (bytes as f64 * 1e6 / self.rate) as u64;
            self.lcg = self.lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let jitter = if self.jitter_us == 0 { 0 } else { (self.lcg >> 33) % self.jitter_us };
            self.last_ack = (self.busy_until + self.rtt_us + jitter + self.hold_us).max(self.last_ack);
            self.last_ack
        }
    }

    /// Runs `secs` of content: `next(pacer, now)` gives the next frame's size
    /// and the gap after it. Frames wait for the window (noted as limited);
    /// with `paced`, content always waits for the estimate (it is sized from
    /// it). Returns (frames sent, worst bottleneck queueing of the last half).
    fn run(
        p: &mut Pacer,
        link: &mut Link,
        secs: u64,
        paced: bool,
        mut next: impl FnMut(&Pacer) -> (u64, u64),
    ) -> (u64, u64) {
        let mut acks: VecDeque<(u64, u32)> = VecDeque::new();
        let (mut seq, mut sent, mut worst) = (0u32, 0u64, 0u64);
        let start = 1_000_000u64;
        let end = start + secs * 1_000_000;
        let (mut t, mut next_frame) = (start, start);
        while t < end {
            while acks.front().is_some_and(|&(at, _)| at <= t) {
                let (_, s) = acks.pop_front().unwrap();
                p.on_ack(s, t, link.hold_us);
            }
            if t >= next_frame {
                if p.can_send(t) {
                    let (bytes, gap) = next(p);
                    seq += 1;
                    p.on_send(seq, bytes, t);
                    if paced {
                        p.note_limited();
                    }
                    if t > start + secs * 500_000 {
                        worst = worst.max(link.busy_until.saturating_sub(t));
                    }
                    acks.push_back((link.deliver(bytes, t), seq));
                    sent += 1;
                    next_frame = t + gap;
                } else if p.window_full() {
                    p.note_limited();
                }
            }
            t += 250;
        }
        (sent, worst)
    }

    const FUNNEL: f64 = 18e6 / 8.0;

    #[test]
    fn small_frames_are_not_bound_by_round_trip() {
        // 2 KB at 165 fps over 100 ms RTT: a fixed 2-frame window would allow 20 fps.
        let mut p = Pacer::default();
        let (sent, _) = run(&mut p, &mut Link::new(2.5e6, 100_000, 0), 4, false, |_| (2_000, 6_060));
        assert!(sent as f64 >= 165.0 * 4.0 * 0.95, "sent {sent}");
    }

    /// Bulk content sized from the estimate, as refinement after a connect is:
    /// one frame of 80 % of the estimate every 16 ms.
    fn bulk(p: &Pacer) -> (u64, u64) {
        ((p.rate() * 0.8 * 0.016) as u64, 16_000)
    }

    #[test]
    fn slow_start_finds_a_fast_link_quickly() {
        // 100 Mbit/s, 20 ms: found within a second from a 5 Mbit/s start.
        let mut p = Pacer::default();
        let mut link = Link::new(100e6 / 8.0, 20_000, 0);
        run(&mut p, &mut link, 1, true, bulk);
        assert!(p.rate() > 50e6 / 8.0, "estimate {} Mbit/s", p.rate() * 8.0 / 1e6);
        assert!(link.worst_queue_us < 100_000, "worst queueing {} µs", link.worst_queue_us);
    }

    #[test]
    fn slow_start_does_not_flood_a_slow_link() {
        // 6 Mbit/s, 50 ms (mobile): the same content from the first frame on.
        let mut p = Pacer::default();
        let mut link = Link::new(6e6 / 8.0, 50_000, 0);
        run(&mut p, &mut link, 3, true, bulk);
        // Content takes 80 % of the estimate: it settles around 6 / 0.8 = 7.5.
        assert!(p.proven() && p.rate() > 3e6 / 8.0 && p.rate() < 11e6 / 8.0, "estimate {} Mbit/s", p.rate() * 8.0 / 1e6);
        assert!(link.worst_queue_us < 150_000, "worst queueing {} µs", link.worst_queue_us);
    }

    #[test]
    fn client_hold_does_not_inflate_the_estimate() {
        // Light content changing at 144 Hz (7.5 KB frames) on a fast, short
        // path (100 Mbit/s, 5 ms): the client draws each frame 12 ms after it
        // arrives, so frames wait for ACKs while the link idles. That is the
        // client, not the link: the estimate must not grow without bound.
        let mut p = Pacer::default();
        let mut link = Link::new(100e6 / 8.0, 5_000, 0);
        link.hold_us = 12_000;
        run(&mut p, &mut link, 10, false, |_| (7_500, 6_944));
        assert!(p.rate() < 2.0 * 100e6 / 8.0, "estimate {} Mbit/s", p.rate() * 8.0 / 1e6);
    }

    #[test]
    fn light_content_keeps_the_estimate_through_ack_jitter() {
        // 3 Mbit/s of content (7.5 KB at 50 fps) on a 100 Mbit/s, 6 ms path
        // whose ACKs jitter by up to 60 ms (poor Wi-Fi): one or two ACKs per
        // 40 ms, so their minimum delay is mostly jitter. Nothing here says
        // the link got slower.
        let mut p = Pacer { rate: 80e6 / 8.0, ceiling: 80e6 / 8.0, backed: 80e6 / 8.0, ..Default::default() };
        run(&mut p, &mut Link::new(100e6 / 8.0, 6_000, 60_000), 60, false, |_| (7_500, 20_000));
        assert!(p.rate() > 40e6 / 8.0, "estimate {} Mbit/s", p.rate() * 8.0 / 1e6);
    }

    #[test]
    fn light_content_still_detects_a_slow_link() {
        // The same content when the link really carries 2 Mbit/s: the queue
        // persists, so the estimate comes down although nothing waited for it.
        let mut p = Pacer { rate: 80e6 / 8.0, ceiling: 80e6 / 8.0, backed: 80e6 / 8.0, ..Default::default() };
        let (_, worst) = run(&mut p, &mut Link::new(2e6 / 8.0, 6_000, 0), 10, false, |_| (7_500, 20_000));
        assert!(p.rate() < 4e6 / 8.0, "estimate {} Mbit/s", p.rate() * 8.0 / 1e6);
        // Frames of 30 ms each on this link: a few of them, not seconds.
        assert!(worst < 150_000, "worst queueing {worst} µs");
    }

    #[test]
    fn saturating_content_converges_without_bufferbloat() {
        // 18 Mbit/s, 50 ms RTT, 100 KB frames offered at 165 fps (far above capacity).
        let mut p = Pacer::default();
        let (sent, worst) = run(&mut p, &mut Link::new(FUNNEL, 50_000, 0), 10, false, |_| (100_000, 6_060));
        let fps = sent as f64 / 10.0;
        assert!(p.rate() > FUNNEL * 0.75 && p.rate() < FUNNEL * 1.3, "estimate {} vs {FUNNEL}", p.rate());
        assert!(fps > FUNNEL / 100_000.0 * 0.75, "fps {fps}");
        // Queueing stays around one round trip, far from seconds of bufferbloat.
        assert!(worst < 120_000, "worst queueing {worst} µs");
    }

    #[test]
    fn path_jitter_does_not_collapse_the_estimate() {
        // 18 Mbit/s, 50 ms RTT plus up to 60 ms of ACK jitter (relayed paths),
        // 25 KB video frames at 165 fps: more than the link carries.
        let mut p = Pacer::default();
        let (sent, _) = run(&mut p, &mut Link::new(FUNNEL, 50_000, 60_000), 10, false, |_| (25_000, 6_060));
        let fps = sent as f64 / 10.0;
        assert!(p.rate() > FUNNEL * 0.7 && p.rate() < FUNNEL * 1.4, "estimate {} vs {FUNNEL}", p.rate());
        assert!(fps > FUNNEL / 25_000.0 * 0.7, "fps {fps}");
    }

    #[test]
    fn content_paced_by_the_estimate_still_finds_capacity() {
        // After a cut to 3 Mbit/s, content sizes itself from the estimate (as
        // paced video does: 70 % of it) and so never exceeds it by itself.
        let mut p = Pacer { rate: 3e6 / 8.0, backed: 3e6 / 8.0, ..Default::default() };
        let mut link = Link::new(FUNNEL, 50_000, 10_000);
        let (_, worst) = run(&mut p, &mut link, 10, true, |p| (20_000, (20_000.0 / (0.7 * p.rate()) * 1e6) as u64));
        // The estimate is the content's budget: it settles around where its
        // 70 % fills the link (a sawtooth: the queue bound below is what matters).
        assert!(p.rate() > FUNNEL * 0.9 && p.rate() < FUNNEL / 0.7 * 1.5, "estimate {} vs {FUNNEL}", p.rate());
        assert!(worst < 80_000, "worst queueing {worst} µs");
    }

    #[test]
    fn fast_links_are_learned_quickly() {
        // 1 Gbit/s LAN, 2 ms RTT, 1 MB frames at 60 fps (content-limited).
        let mut p = Pacer::default();
        run(&mut p, &mut Link::new(125e6, 2_000, 0), 2, false, |_| (1_000_000, 16_666));
        assert!(p.rate() > 60e6, "estimate {}", p.rate());
    }

    #[test]
    fn rate_drop_is_detected() {
        let mut p = Pacer { rate: 100e6 / 8.0, ..Default::default() };
        p.base_rtt = (40_000, 0);
        // A 200 KB frame alone takes 40 ms + 16 ms at 100 Mbit/s; at 5 Mbit/s 360 ms.
        for i in 0..10u32 {
            let t = 1_000_000 * (i as u64 + 1);
            p.on_send(i, 200_000, t);
            p.on_ack(i, t + 40_000 + 320_000, 0);
        }
        assert!(p.rate() < 10e6 / 8.0, "estimate {}", p.rate());
    }

    #[test]
    fn client_processing_is_not_link_queueing() {
        // LAN (2 ms) with a slow client: frames held 50-80 ms before the ACK.
        let mut p = Pacer::default();
        for i in 0..200u32 {
            let t = 1_000_000 + i as u64 * 10_000;
            let hold = 50_000 + (i as u64 % 4) * 10_000;
            p.on_send(i, 50_000, t);
            p.on_ack(i, t + 2_000 + hold, hold);
        }
        assert!(!p.proven() && p.rate() >= INITIAL_RATE, "estimate {} proven {}", p.rate(), p.proven());
    }

    #[test]
    fn stalled_client_gets_no_backlog() {
        // LAN: after a warm-up, the client stops acknowledging for 100 ms
        // (busy, GC); 10 KB frames are offered at 165 fps throughout.
        let mut p = Pacer::default();
        let (mut seq, mut t) = (0u32, 1_000_000u64);
        for _ in 0..200 {
            seq += 1;
            p.on_send(seq, 10_000, t);
            p.on_ack(seq, t + 1_000, 500);
            t += 6_060;
        }
        let mut during_stall = 0;
        for _ in 0..16 {
            if p.can_send(t) {
                seq += 1;
                p.on_send(seq, 10_000, t);
                during_stall += 1;
            }
            t += 6_060;
        }
        // Skipped updates coalesce on the server instead of queueing at the client.
        assert!(during_stall <= 3, "sent {during_stall} frames to a stalled client");
    }
}
