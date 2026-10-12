//! Message-sequence validation.
//!
//! A [`SequenceTracker`] follows one session's message sequence space
//! (protocol v2: sequences start at 1, packets carry contiguous ranges, and
//! heartbeats announce the next sequence number). For every packet it first
//! decides whether the packet is *plausible* ([`admit`](SequenceTracker::admit)),
//! then classifies each message ([`observe`](SequenceTracker::observe)) as
//! in-order, after a gap, a late fill of a tracked gap, a duplicate, or below
//! the receiver's baseline.
//!
//! Design rules, each enforced by tests:
//!
//! * **Wire input can't panic or poison state.** All arithmetic is checked or
//!   saturating (this module denies `clippy::arithmetic_side_effects`). A
//!   packet further ahead than `max_forward_jump` is held back as
//!   [`Admission::Implausible`] and changes nothing, unless `resync_after`
//!   consecutive packets confirm the new position.
//! * **Bounded memory, explicit loss.** At most `max_tracked_gaps` gaps are
//!   remembered; the oldest is evicted and counted as lost.
//! * **Accounting invariant** (as long as no counter saturates):
//!
//! ```text
//! missing_total == outstanding + recovered + evicted + abandoned
//! ```

#![deny(clippy::arithmetic_side_effects)]

use std::collections::VecDeque;

/// Half-open range `[start, end)` of sequence numbers not yet received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GapRange {
    /// First missing sequence number.
    pub start: u64,
    /// One past the last missing sequence number.
    pub end: u64,
}

impl GapRange {
    /// Number of sequence numbers in the range.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the range is empty.
    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    fn contains(&self, seq: u64) -> bool {
        self.start <= seq && seq < self.end
    }
}

/// Tunables for a [`SequenceTracker`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqConfig {
    /// Maximum number of gap ranges remembered. Storage for them is
    /// allocated once, up front. 0 disables late-arrival detection.
    pub max_tracked_gaps: usize,
    /// Largest forward jump (in messages) accepted without confirmation.
    pub max_forward_jump: u64,
    /// Number of consecutive, mutually consistent packets required to accept
    /// a jump larger than `max_forward_jump`. Values below 1 act as 1.
    pub resync_after: u32,
    /// At session start, a first packet whose `first_seq - 1` is at most
    /// this is treated as "the receiver saw the session from its start":
    /// anything before it is tracked as missing. A larger start is a late
    /// join, counted in [`SeqStats::missed_before_join`] instead.
    pub join_window: u64,
}

impl Default for SeqConfig {
    fn default() -> Self {
        Self {
            max_tracked_gaps: 1024,
            max_forward_jump: 1 << 20,
            resync_after: 3,
            join_window: 1024,
        }
    }
}

/// Whether a packet's sequence range is plausible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Process the packet.
    Accepted,
    /// Too far ahead; the packet must be dropped. State is unchanged except
    /// for the confirmation candidate and the counter.
    Implausible,
}

/// Classification of one observed message sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqOutcome {
    /// Exactly the expected next number.
    InOrder,
    /// Ahead of the expected number; `missing` numbers were skipped.
    Gap {
        /// How many sequence numbers were skipped.
        missing: u64,
    },
    /// Behind the expected number but inside a tracked gap: reordered data.
    Late,
    /// Behind the expected number, at or above the baseline, and not inside
    /// any tracked gap: already delivered (or evicted from the gap table).
    Duplicate,
    /// Below the first sequence number this receiver accounts for (after a
    /// late join). Never delivered.
    BeforeBaseline,
}

/// Cumulative counters maintained by a [`SequenceTracker`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeqStats {
    /// Messages accepted exactly in order.
    pub in_order: u64,
    /// Number of forward gaps detected (by a message or a heartbeat).
    pub gaps: u64,
    /// Total sequence numbers ever declared missing.
    pub missing_total: u64,
    /// Missing numbers that later arrived ([`SeqOutcome::Late`]).
    pub recovered: u64,
    /// Missing numbers dropped from a full gap table; counted as lost.
    pub evicted: u64,
    /// Missing numbers still outstanding when the session ended or changed;
    /// counted as lost.
    pub abandoned: u64,
    /// Missing numbers still tracked in the gap table.
    pub outstanding: u64,
    /// Messages classified as duplicates.
    pub duplicates: u64,
    /// Messages below the baseline after a late join.
    pub before_baseline: u64,
    /// Sequence numbers before a late join's first packet. Not loss: this
    /// receiver was not there.
    pub missed_before_join: u64,
    /// Packets rejected as implausible jumps.
    pub implausible_packets: u64,
    /// Confirmed jumps beyond `max_forward_jump`.
    pub resyncs: u64,
}

impl SeqStats {
    /// Messages known to be lost: still missing, evicted, or abandoned.
    pub fn lost(&self) -> u64 {
        self.outstanding
            .saturating_add(self.evicted)
            .saturating_add(self.abandoned)
    }
}

#[derive(Clone, Copy, Debug)]
struct Candidate {
    /// `first_seq` the next packet must carry to confirm the candidate.
    expect: u64,
    packets: u32,
}

#[inline]
fn bump(counter: &mut u64, by: u64) {
    *counter = counter.saturating_add(by);
}

#[inline]
fn drop_by(counter: &mut u64, by: u64) {
    *counter = counter.saturating_sub(by);
}

/// Tracks one session's message sequence space.
#[derive(Debug)]
pub struct SequenceTracker {
    config: SeqConfig,
    /// Next expected sequence number.
    next: u64,
    /// Set once `u64::MAX` itself has been consumed; nothing can follow it.
    exhausted: bool,
    /// Lowest sequence number this receiver accounts for.
    baseline: u64,
    /// Whether any packet has been admitted in this session.
    started: bool,
    /// Sorted, non-overlapping, non-empty gap ranges, oldest first.
    gaps: VecDeque<GapRange>,
    candidate: Option<Candidate>,
    stats: SeqStats,
}

impl SequenceTracker {
    /// Creates a tracker. Gap storage is allocated here, once; no later
    /// method allocates.
    pub fn new(config: SeqConfig) -> Self {
        Self {
            config,
            next: 1,
            exhausted: false,
            baseline: 1,
            started: false,
            gaps: VecDeque::with_capacity(config.max_tracked_gaps.saturating_add(1)),
            candidate: None,
            stats: SeqStats::default(),
        }
    }

    /// Configuration in use.
    pub fn config(&self) -> &SeqConfig {
        &self.config
    }

    /// Next expected sequence number.
    pub fn next_expected(&self) -> u64 {
        self.next
    }

    /// Lowest sequence number this receiver accounts for in the session.
    pub fn baseline(&self) -> u64 {
        self.baseline
    }

    /// Currently tracked gaps, oldest first.
    pub fn gaps(&self) -> impl ExactSizeIterator<Item = &GapRange> {
        self.gaps.iter()
    }

    /// Cumulative counters.
    pub fn stats(&self) -> SeqStats {
        self.stats
    }

    /// Starts a new session: outstanding gaps become `abandoned` (lost) and
    /// the expected sequence returns to 1. Cumulative counters are kept.
    pub fn reset(&mut self) {
        bump(&mut self.stats.abandoned, self.stats.outstanding);
        self.stats.outstanding = 0;
        self.gaps.clear();
        self.next = 1;
        self.exhausted = false;
        self.baseline = 1;
        self.started = false;
        self.candidate = None;
    }

    /// Decides whether a packet covering `[first, end)` is plausible. `end`
    /// is one past its last message (equal to `first` for a heartbeat).
    pub fn admit(&mut self, first: u64, end: u64) -> Admission {
        let end = end.max(first);
        let ahead = first.saturating_sub(self.next);

        if !self.started {
            let before_first = first.saturating_sub(1);
            if before_first <= self.config.join_window {
                self.accept();
                return Admission::Accepted;
            }
            if before_first <= self.config.max_forward_jump {
                self.late_join(first);
                self.accept();
                return Admission::Accepted;
            }
        } else if ahead <= self.config.max_forward_jump {
            self.accept();
            return Admission::Accepted;
        }

        // Too far ahead: require `resync_after` consecutive packets that
        // continue each other before believing it.
        let packets = match self.candidate {
            Some(c) if c.expect == first => c.packets.saturating_add(1),
            _ => 1,
        };
        if packets >= self.config.resync_after.max(1) {
            bump(&mut self.stats.resyncs, 1);
            if !self.started {
                self.late_join(first);
            }
            self.accept();
            Admission::Accepted
        } else {
            self.candidate = Some(Candidate {
                expect: end,
                packets,
            });
            bump(&mut self.stats.implausible_packets, 1);
            Admission::Implausible
        }
    }

    fn accept(&mut self) {
        self.started = true;
        self.candidate = None;
    }

    fn late_join(&mut self, first: u64) {
        bump(&mut self.stats.missed_before_join, first.saturating_sub(1));
        self.baseline = first;
        self.next = first;
    }

    /// Classifies message `seq` and updates state.
    #[inline]
    pub fn observe(&mut self, seq: u64) -> SeqOutcome {
        self.started = true;
        if self.exhausted || seq < self.next {
            return self.observe_behind(seq);
        }
        let outcome = if seq == self.next {
            bump(&mut self.stats.in_order, 1);
            SeqOutcome::InOrder
        } else {
            let missing = seq.saturating_sub(self.next);
            self.record_gap(self.next, seq);
            SeqOutcome::Gap { missing }
        };
        match seq.checked_add(1) {
            Some(n) => self.next = n,
            None => {
                self.next = u64::MAX;
                self.exhausted = true;
            }
        }
        outcome
    }

    /// Applies a heartbeat announcing that the publisher's next message will
    /// be `next_seq`. Anything between the expected sequence and `next_seq` is
    /// recorded as a gap; this is how loss of the final messages before an
    /// idle period is detected. Returns the number of newly missing messages.
    pub fn observe_next_seq(&mut self, next_seq: u64) -> u64 {
        self.started = true;
        if self.exhausted || next_seq <= self.next {
            return 0;
        }
        let missing = next_seq.saturating_sub(self.next);
        self.record_gap(self.next, next_seq);
        self.next = next_seq;
        missing
    }

    fn observe_behind(&mut self, seq: u64) -> SeqOutcome {
        if seq < self.baseline {
            bump(&mut self.stats.before_baseline, 1);
            return SeqOutcome::BeforeBaseline;
        }
        // Gaps are sorted and disjoint, so `end <= seq` holds for a prefix.
        let i = self.gaps.partition_point(|g| g.end <= seq);
        let gap = match self.gaps.get(i) {
            Some(g) if g.contains(seq) => *g,
            _ => {
                bump(&mut self.stats.duplicates, 1);
                return SeqOutcome::Duplicate;
            }
        };

        if gap.len() == 1 {
            self.gaps.remove(i);
        } else if seq == gap.start {
            if let Some(g) = self.gaps.get_mut(i) {
                g.start = seq.saturating_add(1);
            }
        } else if seq == gap.end.saturating_sub(1) {
            if let Some(g) = self.gaps.get_mut(i) {
                g.end = seq;
            }
        } else {
            if let Some(g) = self.gaps.get_mut(i) {
                g.end = seq;
            }
            self.gaps.insert(
                i.saturating_add(1),
                GapRange {
                    start: seq.saturating_add(1),
                    end: gap.end,
                },
            );
            self.enforce_capacity();
        }
        bump(&mut self.stats.recovered, 1);
        drop_by(&mut self.stats.outstanding, 1);
        SeqOutcome::Late
    }

    fn record_gap(&mut self, start: u64, end: u64) {
        let gap = GapRange { start, end };
        let len = gap.len();
        bump(&mut self.stats.gaps, 1);
        bump(&mut self.stats.missing_total, len);
        bump(&mut self.stats.outstanding, len);
        self.gaps.push_back(gap);
        self.enforce_capacity();
    }

    fn enforce_capacity(&mut self) {
        while self.gaps.len() > self.config.max_tracked_gaps {
            let Some(oldest) = self.gaps.pop_front() else {
                break;
            };
            bump(&mut self.stats.evicted, oldest.len());
            drop_by(&mut self.stats.outstanding, oldest.len());
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use SeqOutcome::*;

    fn cfg(max_tracked_gaps: usize) -> SeqConfig {
        SeqConfig {
            max_tracked_gaps,
            ..SeqConfig::default()
        }
    }

    fn check_invariant(t: &SequenceTracker) {
        let s = t.stats();
        assert_eq!(
            s.missing_total,
            s.outstanding + s.recovered + s.evicted + s.abandoned
        );
        let tracked: u64 = t.gaps().map(GapRange::len).sum();
        assert_eq!(tracked, s.outstanding);
        let gaps: Vec<_> = t.gaps().copied().collect();
        assert!(gaps.iter().all(|g| !g.is_empty()));
        assert!(gaps.windows(2).all(|w| w[0].end <= w[1].start));
        assert!(gaps.len() <= t.config().max_tracked_gaps);
    }

    fn feed(t: &mut SequenceTracker, seqs: &[u64]) -> Vec<SeqOutcome> {
        seqs.iter()
            .map(|&s| {
                assert_eq!(t.admit(s, s + 1), Admission::Accepted);
                let o = t.observe(s);
                check_invariant(t);
                o
            })
            .collect()
    }

    fn gaps(t: &SequenceTracker) -> Vec<GapRange> {
        t.gaps().copied().collect()
    }

    #[test]
    fn in_order_stream() {
        let mut t = SequenceTracker::new(cfg(8));
        assert_eq!(feed(&mut t, &[1, 2, 3]), [InOrder, InOrder, InOrder]);
        assert_eq!(t.next_expected(), 4);
        assert_eq!(t.stats().in_order, 3);
    }

    #[test]
    fn loss_at_session_start_is_a_gap() {
        // Sequences start at 1, so a first packet at 3 means 1 and 2 are
        // missing, and a reordered 1 is late, not a duplicate.
        let mut t = SequenceTracker::new(cfg(8));
        assert_eq!(feed(&mut t, &[3, 1]), [Gap { missing: 2 }, Late]);
        assert_eq!(gaps(&t), [GapRange { start: 2, end: 3 }]);
    }

    #[test]
    fn gap_then_late_fill() {
        let mut t = SequenceTracker::new(cfg(8));
        let out = feed(&mut t, &[1, 2, 5, 3, 4, 6]);
        assert_eq!(
            out,
            [InOrder, InOrder, Gap { missing: 2 }, Late, Late, InOrder]
        );
        let s = t.stats();
        assert_eq!(
            (s.gaps, s.missing_total, s.recovered, s.outstanding),
            (1, 2, 2, 0)
        );
        assert_eq!(t.gaps().len(), 0);
    }

    #[test]
    fn late_fill_in_middle_splits_gap() {
        let mut t = SequenceTracker::new(cfg(8));
        feed(&mut t, &[1, 10]);
        assert_eq!(t.observe(5), Late);
        assert_eq!(
            gaps(&t),
            [
                GapRange { start: 2, end: 5 },
                GapRange { start: 6, end: 10 }
            ]
        );
        check_invariant(&t);
    }

    #[test]
    fn duplicates_are_distinguished_from_late_data() {
        let mut t = SequenceTracker::new(cfg(8));
        let out = feed(&mut t, &[1, 2, 2, 4, 3, 3, 1]);
        assert_eq!(
            out,
            [
                InOrder,
                InOrder,
                Duplicate,
                Gap { missing: 1 },
                Late,
                Duplicate,
                Duplicate
            ]
        );
        assert_eq!(t.stats().duplicates, 3);
    }

    #[test]
    fn full_gap_table_evicts_oldest_as_lost() {
        let mut t = SequenceTracker::new(cfg(2));
        // Three gaps: {2}, {4}, {6}. Only the newest two can be tracked.
        feed(&mut t, &[1, 3, 5, 7]);
        let s = t.stats();
        assert_eq!(
            (s.gaps, s.missing_total, s.evicted, s.outstanding),
            (3, 3, 1, 2)
        );
        assert_eq!(s.lost(), 3);
        // The evicted number now looks like a duplicate; that is the
        // documented cost of bounded memory, and it is already counted lost.
        assert_eq!(t.observe(2), Duplicate);
        assert_eq!(t.observe(4), Late);
        check_invariant(&t);
    }

    #[test]
    fn split_that_overflows_table_evicts() {
        let mut t = SequenceTracker::new(cfg(1));
        feed(&mut t, &[1, 10]);
        assert_eq!(t.observe(5), Late);
        check_invariant(&t);
        assert_eq!(gaps(&t), [GapRange { start: 6, end: 10 }]);
        assert_eq!(t.stats().evicted, 3);
    }

    #[test]
    fn zero_capacity_counts_every_gap_as_lost() {
        let mut t = SequenceTracker::new(cfg(0));
        feed(&mut t, &[1, 5]);
        assert_eq!(t.stats().evicted, 3);
        assert_eq!(t.observe(3), Duplicate);
        check_invariant(&t);
    }

    #[test]
    fn reset_abandons_outstanding_and_restarts_at_one() {
        let mut t = SequenceTracker::new(cfg(8));
        feed(&mut t, &[1, 5]);
        t.reset();
        check_invariant(&t);
        assert_eq!(t.stats().abandoned, 3);
        assert_eq!(t.stats().lost(), 3);
        assert_eq!(t.next_expected(), 1);
        assert_eq!(feed(&mut t, &[1]), [InOrder]);
    }

    #[test]
    fn heartbeat_reveals_tail_loss() {
        let mut t = SequenceTracker::new(cfg(8));
        feed(&mut t, &[1, 2]);
        // Publisher says its next message is 5: 3 and 4 were lost.
        assert_eq!(t.admit(5, 5), Admission::Accepted);
        assert_eq!(t.observe_next_seq(5), 2);
        assert_eq!(gaps(&t), [GapRange { start: 3, end: 5 }]);
        // A stale heartbeat changes nothing.
        assert_eq!(t.observe_next_seq(4), 0);
        assert_eq!(feed(&mut t, &[5, 3]), [InOrder, Late]);
        check_invariant(&t);
    }

    #[test]
    fn far_future_packet_is_implausible_and_changes_nothing() {
        let mut t = SequenceTracker::new(cfg(8));
        feed(&mut t, &[1]);
        let far = 1 + (1 << 20) + 2;
        assert_eq!(t.admit(far, far + 1), Admission::Implausible);
        assert_eq!(t.next_expected(), 2);
        assert_eq!(t.stats().implausible_packets, 1);
        // The real stream continues unharmed, and in order.
        assert_eq!(feed(&mut t, &[2, 3]), [InOrder, InOrder]);
        assert_eq!(t.stats().missing_total, 0);
    }

    #[test]
    fn consistent_far_jump_resyncs_and_counts_the_gap() {
        let mut t = SequenceTracker::new(cfg(8));
        feed(&mut t, &[1]);
        let far = 10_000_000u64;
        // Packets of 4 messages: [far, far+4), [far+4, far+8), [far+8, ...)
        assert_eq!(t.admit(far, far + 4), Admission::Implausible);
        assert_eq!(t.admit(far + 4, far + 8), Admission::Implausible);
        assert_eq!(t.admit(far + 8, far + 12), Admission::Accepted);
        assert_eq!(t.observe(far + 8), Gap { missing: far + 6 });
        let s = t.stats();
        assert_eq!((s.resyncs, s.implausible_packets), (1, 2));
        assert_eq!(s.missing_total, far + 6);
        check_invariant(&t);
    }

    #[test]
    fn inconsistent_far_packets_never_resync() {
        let mut t = SequenceTracker::new(cfg(8));
        feed(&mut t, &[1]);
        for k in 0..10u64 {
            let far = 50_000_000 + k * 1_000;
            assert_eq!(t.admit(far, far + 1), Admission::Implausible);
        }
        assert_eq!(t.stats().resyncs, 0);
        assert_eq!(feed(&mut t, &[2]), [InOrder]);
    }

    #[test]
    fn late_join_sets_baseline() {
        let mut t = SequenceTracker::new(cfg(8));
        // Beyond the join window but within the jump bound: late join.
        assert_eq!(t.admit(5_000, 5_002), Admission::Accepted);
        assert_eq!(t.observe(5_000), InOrder);
        assert_eq!(t.baseline(), 5_000);
        assert_eq!(t.stats().missed_before_join, 4_999);
        assert_eq!(t.stats().missing_total, 0);
        assert_eq!(t.observe(4_999), BeforeBaseline);
        assert_eq!(t.stats().before_baseline, 1);
        check_invariant(&t);
    }

    #[test]
    fn far_late_join_needs_confirmation() {
        let mut t = SequenceTracker::new(cfg(8));
        let far = 3_000_000_000u64;
        assert_eq!(t.admit(far, far + 1), Admission::Implausible);
        assert_eq!(t.admit(far + 1, far + 2), Admission::Implausible);
        assert_eq!(t.admit(far + 2, far + 3), Admission::Accepted);
        assert_eq!(t.observe(far + 2), InOrder);
        assert_eq!(t.baseline(), far + 2);
        assert_eq!(t.stats().missed_before_join, far + 1);
    }

    #[test]
    fn u64_max_is_consumed_exactly_once() {
        let mut t = SequenceTracker::new(cfg(8));
        assert_eq!(
            t.observe(u64::MAX - 1),
            Gap {
                missing: u64::MAX - 2
            }
        );
        assert_eq!(t.observe(u64::MAX), InOrder);
        assert_eq!(t.observe(u64::MAX), Duplicate);
        assert_eq!(t.observe_next_seq(u64::MAX), 0);
        check_invariant(&t);
    }

    #[test]
    fn deterministic_shuffle_conserves_accounting() {
        // Deliver 1..=1000 with local reordering, some drops, and duplicates;
        // verify every number is accounted for exactly once.
        let mut rng = 12345u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut seqs: Vec<u64> = (1..=1000).collect();
        for i in (1..seqs.len()).step_by(3) {
            if next() % 2 == 0 {
                seqs.swap(i - 1, i);
            }
        }
        let mut stream = Vec::new();
        let mut dropped = 0u64;
        for s in seqs {
            match next() % 20 {
                0 => dropped += 1,
                1 => {
                    stream.push(s);
                    stream.push(s);
                }
                _ => stream.push(s),
            }
        }
        let mut t = SequenceTracker::new(cfg(256));
        feed(&mut t, &stream);
        // A heartbeat announcing 1001 makes even a dropped tail detectable.
        t.observe_next_seq(1001);
        check_invariant(&t);
        let s = t.stats();
        assert_eq!(s.evicted, 0);
        assert_eq!(s.outstanding, dropped);
    }
}
