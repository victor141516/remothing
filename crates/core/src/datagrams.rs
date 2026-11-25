//! Frames over WebTransport datagrams (`proto::S_PIECE`): how many parity
//! pieces each frame carries (`Fec`), the frames kept for repairs (`Kept`)
//! and the paced sending of the datagrams (`pace`).

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::proto;

/// Data pieces per parity piece, from the lightest protection to the heaviest.
const PER_STEPS: [usize; 4] = [16, 8, 4, 2];
/// Where a session starts: one in 8 (the fixed ratio before it adapted).
const STEP_START: usize = 1;
/// Loss is judged over this long (µs).
const FEC_WINDOW_US: u64 = 1_000_000;
/// Pieces asked for again above this share of those sent: more parity.
const FEC_LOSS_UP: f64 = 0.005;
/// This long without a piece asked for again: less parity (µs).
const FEC_CLEAN_US: u64 = 5_000_000;
/// After the link model cut its estimate, losses may be the queue's own,
/// which parity would only add to: no more parity for this long (µs). Not
/// less either: random loss makes the model cut now and then too, and
/// dropping to the least parity then had 8-20 % of the pieces asked for
/// again at 3 % loss.
const FEC_QUEUE_US: u64 = 2_000_000;

/// Parity follows the loss parity could not repair: what the client asks
/// for again (pieces lost beyond what the parity rebuilt). Random loss
/// (Wi-Fi, radio) then costs no round trip of asking; a clean path carries
/// half the parity it used to.
pub struct Fec {
    step: usize,
    sent: u64,
    asked: u64,
    window_from: u64,
    clean_from: u64,
    queue_until: u64,
}

impl Default for Fec {
    fn default() -> Self {
        Self { step: STEP_START, sent: 0, asked: 0, window_from: 0, clean_from: 0, queue_until: 0 }
    }
}

impl Fec {
    /// Data pieces per parity piece for the next frame.
    pub fn per(&self) -> usize {
        PER_STEPS[self.step]
    }

    /// A frame of `pieces` datagrams (data and parity) went out at `now`
    /// (µs). When that changed the parity: the share of pieces asked for
    /// again over the window that decided it.
    pub fn on_sent(&mut self, pieces: usize, now: u64) -> Option<f64> {
        self.sent += pieces as u64;
        if self.window_from == 0 {
            (self.window_from, self.clean_from) = (now, now);
        }
        if now.saturating_sub(self.window_from) < FEC_WINDOW_US {
            return None;
        }
        let (loss, was) = (self.asked as f64 / self.sent.max(1) as f64, self.step);
        if loss > FEC_LOSS_UP && now >= self.queue_until {
            (self.step, self.clean_from) = ((self.step + 1).min(PER_STEPS.len() - 1), now);
        } else if self.asked > 0 {
            self.clean_from = now;
        } else if now.saturating_sub(self.clean_from) >= FEC_CLEAN_US {
            (self.step, self.clean_from) = (self.step.saturating_sub(1), now);
        }
        (self.sent, self.asked, self.window_from) = (0, 0, now);
        (self.step != was).then_some(loss)
    }

    /// The client asked for `pieces` data pieces again.
    pub fn on_asked(&mut self, pieces: usize) {
        self.asked += pieces as u64;
    }

    /// The link model cut its estimate at `now`: a queue is building.
    pub fn on_queue(&mut self, now: u64) {
        self.queue_until = now + FEC_QUEUE_US;
    }
}

/// A frame sent as datagrams and not acknowledged yet.
pub struct Kept {
    pub seq: u32,
    frame: Vec<u8>,
    per: usize,
    /// Data pieces sent again already.
    resent: Vec<bool>,
}

impl Kept {
    /// Frame `frame` (`proto::FRAME`), sent with one parity piece every `per`
    /// data pieces: appends all its datagrams to `out`.
    pub fn send(frame: Vec<u8>, per: usize, out: &mut Vec<Datagram>) -> Self {
        let (data, parity) = proto::piece_counts(frame.len(), per);
        let kept = Self { seq: u32::from_le_bytes(frame[4..8].try_into().unwrap()), frame, per, resent: vec![false; data] };
        out.extend((0..data + parity).map(|i| kept.datagram(i, false)));
        kept
    }

    fn datagram(&self, index: usize, repair: bool) -> Datagram {
        let mut bytes = Vec::with_capacity(proto::PIECE_HEADER_LEN + proto::PIECE_PAYLOAD);
        proto::piece(&self.frame, index, self.per, &mut bytes);
        Datagram { bytes, repair }
    }

    /// Data pieces `asked` again (all of them when `None`: not one piece
    /// arrived, the client knows nothing of the frame): appends them to
    /// `out` and returns how many were asked for. A piece asked for the
    /// first time goes twice, the copy after all the others: a loss burst
    /// that took the original seldom takes both, and a repair that fails
    /// costs another round trip of asking.
    pub fn repair(&mut self, asked: Option<&mut dyn Iterator<Item = usize>>, out: &mut Vec<Datagram>) -> usize {
        let data = self.resent.len();
        let asked: Vec<usize> = match asked {
            Some(asked) => asked.filter(|&i| i < data).collect(),
            None => (0..data).collect(),
        };
        out.extend(asked.iter().map(|&i| self.datagram(i, true)));
        for &i in &asked {
            if !std::mem::replace(&mut self.resent[i], true) {
                out.push(self.datagram(i, true));
            }
        }
        asked.len()
    }
}

/// One datagram for `pace`; repairs go ahead of the frames waiting.
pub struct Datagram {
    bytes: Vec<u8>,
    repair: bool,
}

/// Pacing rate over the link model's estimate: a frame's pieces leave over
/// 40 % of the time the path needs for them, so they never wait long here,
/// but reach a slower hop (a Wi-Fi access point, a radio cell) spread out
/// instead of as one line-rate burst that overflows its buffer.
pub const PACE_GAIN: f64 = 2.5;
/// A burst may be this long at the pacing rate (s), after a pause; then the
/// timer, which wakes about once a millisecond, sends what each wake-up
/// allows. A larger minimum burst meant pauses beyond 4 ms between bursts
/// at low rates, which the client takes for the end of a frame's pieces
/// and asks for the rest (13 % of the pieces asked for again at 2 % loss,
/// the repairs then crowding out frames).
const BURST_S: f64 = 0.002;
/// Shortest wait between bursts.
const TICK: Duration = Duration::from_millis(1);

/// Sends the datagrams that come through `queue` over `conn`, at most at
/// `rate()` bytes a second (`None`: unpaced, the estimate is not proven
/// yet and a keyframe would crawl at its guess). Returns when the queue
/// closes.
pub async fn pace(conn: &wtransport::Connection, mut queue: mpsc::UnboundedReceiver<Datagram>, rate: impl Fn() -> Option<f64>) -> String {
    let (mut repairs, mut frames) = (std::collections::VecDeque::new(), std::collections::VecDeque::new());
    let (mut tokens, mut last) = (0.0f64, Instant::now());
    loop {
        if repairs.is_empty() && frames.is_empty() {
            match queue.recv().await {
                Some(d) => if d.repair { repairs.push_back(d) } else { frames.push_back(d) },
                None => return "send queue closed".into(),
            }
        }
        while let Ok(d) = queue.try_recv() {
            if d.repair { repairs.push_back(d) } else { frames.push_back(d) }
        }
        let now = Instant::now();
        let rate = rate();
        let burst = rate.map_or(f64::INFINITY, |r| r * BURST_S);
        tokens = (tokens + rate.map_or(f64::INFINITY, |r| r * now.duration_since(last).as_secs_f64())).min(burst);
        last = now;
        // A datagram may overdraw the bucket: the next burst waits for it.
        while tokens > 0.0 {
            let Some(d) = repairs.pop_front().or_else(|| frames.pop_front()) else { break };
            tokens -= d.bytes.len() as f64;
            // A closed connection drops it; its closing is handled elsewhere.
            let _ = conn.send_datagram(&d.bytes);
        }
        if let Some(r) = rate.filter(|_| !(repairs.is_empty() && frames.is_empty())) {
            tokio::time::sleep(Duration::from_secs_f64(-tokens.min(0.0) / r).max(TICK)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(fec: &mut Fec, secs: u64, asked_per_s: usize, t: &mut u64) {
        for _ in 0..secs * 100 {
            *t += 10_000;
            fec.on_sent(10, *t);
            if t.is_multiple_of(1_000_000) {
                fec.on_asked(asked_per_s);
            }
        }
    }

    #[test]
    fn parity_follows_unrepaired_loss() {
        let (mut fec, mut t) = (Fec::default(), 1_000_000);
        assert_eq!(fec.per(), 8);
        // 1000 pieces a second, 10 asked for again: 1 %.
        run(&mut fec, 2, 10, &mut t);
        assert_eq!(fec.per(), 4, "protection grows while loss stays");
        // A cut of the estimate: losses may be the queue's, no more parity.
        fec.on_queue(t);
        run(&mut fec, 1, 10, &mut t);
        assert_eq!(fec.per(), 4);
        run(&mut fec, 2, 10, &mut t);
        assert_eq!(fec.per(), 2, "loss after the queue went is answered again");
        // Clean for good: back to the lightest, in steps of 5 s.
        run(&mut fec, 4, 0, &mut t);
        let before = fec.per();
        run(&mut fec, 30, 0, &mut t);
        assert!(before < 16 && fec.per() == 16, "{before} then {}", fec.per());
        // Occasional loss under the threshold keeps the protection there
        // (the second run takes in what the first left in its last window).
        run(&mut fec, 2, 10, &mut t);
        run(&mut fec, 1, 2, &mut t);
        let raised = fec.per();
        run(&mut fec, 20, 2, &mut t);
        assert_eq!(fec.per(), raised);
    }

    #[test]
    fn first_repair_of_a_piece_goes_twice() {
        let frame: Vec<u8> = (0..20 * proto::PIECE_PAYLOAD).map(|i| i as u8).collect();
        let mut sent = Vec::new();
        let mut kept = Kept::send(frame, 8, &mut sent);
        assert_eq!(sent.len(), 20 + 3);
        let mut out = Vec::new();
        assert_eq!(kept.repair(Some(&mut [3, 4, 25].into_iter()), &mut out), 2);
        // 3 and 4, then their copies after both.
        let index = |d: &Datagram| u16::from_le_bytes([d.bytes[2], d.bytes[3]]);
        assert_eq!(out.iter().map(index).collect::<Vec<_>>(), [3, 4, 3, 4]);
        assert!(out.iter().all(|d| d.repair));
        out.clear();
        kept.repair(Some(&mut [4, 5].into_iter()), &mut out);
        assert_eq!(out.iter().map(index).collect::<Vec<_>>(), [4, 5, 5]);
        out.clear();
        kept.repair(None, &mut out);
        assert_eq!(out.len(), 20 + 17);
    }
}
