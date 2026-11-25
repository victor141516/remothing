//! Which frames the next frame may predict from once the client lost some: the bookkeeping
//! behind `VideoEncoder::invalidate_from` and `acknowledge` for NVENC, kept apart from the
//! driver so it can be judged without one.
//!
//! A loss newer than the oldest short-term reference is predicted past from a short-term
//! reference before it, as Sunshine does. An older one needs a long-term reference (an anchor)
//! from before the loss: frames are marked into the long-term slots on a schedule, and the
//! newest one the client acknowledged stays pinned while the schedule marks the other slot, so
//! a loss of any depth finds a frame the client holds. What the codec's syntax cannot carry
//! past a gap (H.264's `frame_num` wrap, a picture order count's most significant bits) is
//! answered with a keyframe instead.

/// Frames between anchors.
const ANCHOR_EVERY: u64 = 16;

/// An anchor the client has not acknowledged yet keeps its slot for this many anchor periods
/// (one on a slow link reports later than a period), then the schedule marks over it.
const PENDING_PERIODS: u64 = 4;

/// The most frames any codec's decoded picture buffer holds.
const MAX_DPB: u64 = 16;

/// What the decoded picture buffer and the codec's syntax allow.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    /// Short-term references the buffer keeps besides the anchors.
    pub short: u64,
    /// Long-term slots (0: no anchors).
    pub anchors: usize,
}

/// What the parameter sets say about gaps a decoder can bridge, once the first keyframe is out.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Syntax {
    /// H.264: values `frame_num` takes. FFmpeg's decoder, which browsers use on Linux, drops
    /// pictures after a gap across the frame where it wraps (selkies measured 13 of a range of
    /// 16 after two lost frames; with NVENC's 256 here, `d3d11_bench --lose 30`, FFmpeg 9 showed
    /// 135 fewer pictures over three such losses), so a loss covering that frame costs a keyframe.
    pub frame_num_range: Option<u64>,
    /// Picture order count LSB range (HEVC, H.264 with POC type 0): a decoder derives the
    /// order of the next picture right only if it lies less than half of it after the last
    /// picture it decoded.
    pub poc_lsb_range: Option<u64>,
    /// The parameter sets were read, so the limits above are known (absent ones do not apply).
    pub known: bool,
}

/// The plan for the frame about to be coded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    /// Mark the frame into this long-term slot.
    pub mark: Option<usize>,
    /// Predict from the anchor in this slot (recovery past a deep loss).
    pub use_slot: Option<usize>,
    /// The frame is the first after `invalidate(first)`.
    pub recovers: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct Recovery {
    first: u64,
    use_slot: Option<usize>,
}

pub(super) struct References {
    /// None: the device cannot invalidate references.
    limits: Option<Limits>,
    syntax: Syntax,
    /// Index of the next frame.
    next: u64,
    /// Index of the last keyframe.
    key: u64,
    /// Frame index held by each long-term slot.
    slots: Vec<Option<u64>>,
    /// Index of the last frame marked into a slot (or of the keyframe).
    marked: u64,
    /// Newest frame the client acknowledged.
    acked: Option<u64>,
    pending: Option<Recovery>,
}

impl References {
    pub fn new(limits: Option<Limits>) -> Self {
        let slots = vec![None; limits.map_or(0, |l| l.anchors)];
        Self { limits, syntax: Syntax::default(), next: 0, key: 0, slots, marked: 0, acked: None, pending: None }
    }

    pub fn set_syntax(&mut self, syntax: Syntax) {
        self.syntax = syntax;
    }

    /// Index of the frame about to be coded.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Frames from `first` on are lost. Returns the frames the encoder has to forget (the ones
    /// still in its buffer), or None when only a keyframe gets the client back.
    pub fn invalidate(&mut self, first: u64) -> Option<Vec<u64>> {
        let limits = self.limits?;
        let first = self.pending.map_or(first, |p| p.first.min(first));
        if first >= self.next {
            self.pending = Some(Recovery { first, use_slot: None });
            return Some(Vec::new());
        }
        if first <= self.key || !self.syntax.known {
            return None;
        }
        // Frames the client misses before the next one, which it decodes after `first - 1`.
        let gap = self.next - first;
        if let Some(range) = self.syntax.frame_num_range {
            // The first frame at or after `first` whose frame_num is 0.
            let wrap = self.key + (first - self.key).div_ceil(range) * range;
            if wrap < self.next {
                return None;
            }
        }
        if self.syntax.poc_lsb_range.is_some_and(|range| 2 * (gap + 1) >= range) {
            return None;
        }
        let use_slot = if gap < limits.short {
            None
        } else {
            let newest = self.slots.iter().enumerate().filter_map(|(s, i)| i.filter(|&i| i < first).map(|i| (s, i)));
            Some(newest.max_by_key(|&(_, i)| i)?.0)
        };
        // Every lost frame a decoded picture buffer can still hold (the driver may keep more
        // short-term frames than `short` while anchor slots are free), anchors included.
        let oldest = self.next.saturating_sub(MAX_DPB).max(first);
        let mut forget: Vec<u64> = (oldest..self.next).collect();
        for slot in &mut self.slots {
            if let Some(i) = slot.filter(|&i| i >= first) {
                if i < oldest {
                    forget.push(i);
                }
                *slot = None;
            }
        }
        self.pending = Some(Recovery { first, use_slot });
        Some(forget)
    }

    /// The encoder refused to forget: the caller codes a keyframe.
    pub fn abandon(&mut self) {
        self.pending = None;
    }

    pub fn acknowledge(&mut self, index: u64) {
        if index < self.next {
            self.acked = Some(self.acked.map_or(index, |a| a.max(index)));
        }
    }

    /// What the next frame does; `key`: it is forced to be a keyframe.
    pub fn plan(&self, key: bool) -> Plan {
        if key {
            return Plan::default();
        }
        let recovery = self.pending;
        Plan {
            mark: if recovery.is_some() { None } else { self.mark_slot() },
            use_slot: recovery.and_then(|r| r.use_slot),
            recovers: recovery.map(|r| r.first),
        }
    }

    /// The slot the schedule marks the next frame into, if it is due.
    fn mark_slot(&self) -> Option<usize> {
        if self.slots.is_empty() || self.next - self.marked < ANCHOR_EVERY {
            return None;
        }
        let acked = |i: u64| self.acked.is_some_and(|a| i <= a);
        let stale = |i: u64| self.next - i >= PENDING_PERIODS * ANCHOR_EVERY;
        if let Some(empty) = self.slots.iter().position(Option::is_none) {
            return Some(empty);
        }
        if self.slots.len() == 1 {
            // Moved on once the client holds it: until the next anchor reaches the client, a
            // loss deeper than the short-term references costs a keyframe.
            let i = self.slots[0]?;
            return (acked(i) || stale(i)).then_some(0);
        }
        let pinned = (0..self.slots.len()).filter(|&s| acked(self.slots[s].unwrap_or(0))).max_by_key(|&s| self.slots[s]);
        let (slot, index) = (0..self.slots.len())
            .filter(|&s| Some(s) != pinned)
            .map(|s| (s, self.slots[s].unwrap_or(0)))
            .min_by_key(|&(_, i)| i)?;
        (acked(index) || stale(index)).then_some(slot)
    }

    /// The frame planned with `plan` was coded; `key`: it came out a keyframe.
    pub fn coded(&mut self, plan: Plan, key: bool) {
        if key {
            self.key = self.next;
            self.marked = self.next;
            self.slots.iter_mut().for_each(|s| *s = None);
        } else if let Some(slot) = plan.mark {
            self.slots[slot] = Some(self.next);
            self.marked = self.next;
        }
        self.pending = None;
        self.next += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTAX: Syntax = Syntax { frame_num_range: None, poc_lsb_range: None, known: true };

    fn refs(short: u64, anchors: usize) -> References {
        let mut r = References::new(Some(Limits { short, anchors }));
        r.set_syntax(SYNTAX);
        r
    }

    /// Codes `n` frames, the first a keyframe when nothing was coded yet.
    fn code(r: &mut References, n: u64) {
        for _ in 0..n {
            let key = r.next == 0;
            let plan = r.plan(key);
            r.coded(plan, key);
        }
    }

    #[test]
    fn shallow_loss_predicts_from_a_short_term_reference() {
        let mut r = refs(3, 2);
        code(&mut r, 10);
        assert_eq!(r.invalidate(8), Some(vec![8, 9]));
        let plan = r.plan(false);
        assert_eq!(plan, Plan { mark: None, use_slot: None, recovers: Some(8) });
        r.coded(plan, false);
        assert_eq!(r.plan(false).recovers, None);
    }

    #[test]
    fn deep_loss_needs_an_anchor_before_it() {
        let mut r = refs(3, 2);
        code(&mut r, 40);
        // Anchors at 16 and 32 (empty slots fill without waiting for acknowledgements).
        assert_eq!(r.slots, vec![Some(16), Some(32)]);
        let forget = r.invalidate(30).expect("anchor 16 predates the loss");
        assert_eq!(forget, (30..40).collect::<Vec<_>>());
        assert_eq!(r.slots, vec![Some(16), None]);
        assert_eq!(r.plan(false).use_slot, Some(0));
        // Nothing before 10 but the keyframe, which the loss covers.
        let mut r = refs(3, 2);
        code(&mut r, 40);
        assert_eq!(r.invalidate(15), None);
        assert_eq!(r.invalidate(0), None);
    }

    #[test]
    fn acknowledged_anchor_stays_pinned() {
        let mut r = refs(3, 2);
        code(&mut r, 33);
        r.acknowledge(20);
        // Slot 0 (16) is acknowledged and pinned; slot 1 (32) waits for its acknowledgement.
        code(&mut r, 100);
        assert_eq!(r.slots[0], Some(16));
        assert_eq!(r.slots[1], Some(32 + PENDING_PERIODS * ANCHOR_EVERY));
        r.acknowledge(r.next - 1);
        code(&mut r, ANCHOR_EVERY);
        // The newer anchor is pinned now and the old slot is marked over.
        assert_eq!(r.slots[1], Some(32 + PENDING_PERIODS * ANCHOR_EVERY));
        assert_ne!(r.slots[0], Some(16));
    }

    #[test]
    fn frame_num_wrap_and_poc_range_cost_a_keyframe() {
        let mut r = refs(3, 2);
        r.set_syntax(Syntax { frame_num_range: Some(16), ..SYNTAX });
        code(&mut r, 20);
        assert_eq!(r.invalidate(17), Some(vec![17, 18, 19]));
        assert_eq!(r.invalidate(16), None, "frame 16 has frame_num 0");
        let mut r = refs(3, 2);
        r.set_syntax(Syntax { poc_lsb_range: Some(16), ..SYNTAX });
        code(&mut r, 40);
        assert!(r.invalidate(34).is_some());
        assert!(r.invalidate(33).is_none(), "8 frames from the last decoded one is half the range");
    }

    #[test]
    fn keyframe_resets_anchors() {
        let mut r = refs(3, 2);
        code(&mut r, 40);
        let plan = r.plan(true);
        r.coded(plan, true);
        assert_eq!(r.slots, vec![None, None]);
        code(&mut r, 5);
        assert_eq!(r.invalidate(39), None);
    }
}
