//! Sequence-number validation.
//!
//! A [`SequenceTracker`] classifies each observed sequence number as in-order,
//! a forward gap, a late arrival that fills a previously detected gap, or a
//! duplicate. Gaps are remembered in a bounded table so that late (reordered)
//! data can be told apart from duplicates without unbounded memory.
//!
//! Accounting invariant, checked by tests:
//!
//! ```text
//! missing_total == outstanding + recovered + evicted
//! ```

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
        self.end - self.start
    }

    /// Whether the range is empty.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    fn contains(&self, seq: u64) -> bool {
        self.start <= seq && seq < self.end
    }
}

/// Classification of one observed sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqOutcome {
    /// First number seen since creation or reset; establishes the baseline.
    First,
    /// Exactly the expected next number.
    InOrder,
    /// Ahead of the expected number; `missing` numbers were skipped.
    Gap {
        /// How many sequence numbers were skipped.
        missing: u64,
    },
    /// Behind the expected number but inside a tracked gap: reordered data.
    Late,
    /// Behind the expected number and not inside any tracked gap.
    Duplicate,
}

/// Cumulative counters maintained by a [`SequenceTracker`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeqStats {
    /// Numbers accepted as first or in-order.
    pub in_order: u64,
    /// Number of forward gaps detected.
    pub gaps: u64,
    /// Total sequence numbers ever declared missing.
    pub missing_total: u64,
    /// Missing numbers that later arrived ([`SeqOutcome::Late`]).
    pub recovered: u64,
    /// Missing numbers dropped from the gap table because it was full. These
    /// are counted as lost; if they arrive later they are reported as
    /// duplicates.
    pub evicted: u64,
    /// Missing numbers still tracked in the gap table.
    pub outstanding: u64,
    /// Numbers classified as duplicates.
    pub duplicates: u64,
}

/// Tracks one monotonically increasing sequence stream.
#[derive(Debug)]
pub struct SequenceTracker {
    next: Option<u64>,
    /// Sorted, non-overlapping, non-empty gap ranges, oldest first.
    gaps: Vec<GapRange>,
    max_gaps: usize,
    stats: SeqStats,
}

impl SequenceTracker {
    /// Creates a tracker that remembers at most `max_gaps` gap ranges.
    ///
    /// Storage is allocated up front; [`observe`](Self::observe) never
    /// allocates. A `max_gaps` of 0 disables late-arrival detection.
    pub fn new(max_gaps: usize) -> Self {
        Self {
            next: None,
            gaps: Vec::with_capacity(max_gaps + 1),
            max_gaps,
            stats: SeqStats::default(),
        }
    }

    /// Next expected sequence number, if a baseline has been established.
    pub fn next_expected(&self) -> Option<u64> {
        self.next
    }

    /// Currently tracked gaps, oldest first.
    pub fn gaps(&self) -> &[GapRange] {
        &self.gaps
    }

    /// Cumulative counters.
    pub fn stats(&self) -> SeqStats {
        self.stats
    }

    /// Forgets the baseline and tracked gaps (e.g. after a session change).
    /// Cumulative counters are preserved; outstanding gaps become evicted.
    pub fn reset(&mut self) {
        self.stats.evicted += self.stats.outstanding;
        self.stats.outstanding = 0;
        self.gaps.clear();
        self.next = None;
    }

    /// Classifies `seq` and updates state.
    ///
    /// Sequence number `u64::MAX` saturates the expected-next counter, so a
    /// stream that reaches it will report subsequent values as duplicates.
    #[inline]
    pub fn observe(&mut self, seq: u64) -> SeqOutcome {
        let Some(next) = self.next else {
            self.next = Some(seq.saturating_add(1));
            self.stats.in_order += 1;
            return SeqOutcome::First;
        };

        if seq == next {
            self.next = Some(next.saturating_add(1));
            self.stats.in_order += 1;
            SeqOutcome::InOrder
        } else if seq > next {
            let missing = seq - next;
            self.next = Some(seq.saturating_add(1));
            self.stats.gaps += 1;
            self.stats.missing_total += missing;
            self.stats.outstanding += missing;
            self.gaps.push(GapRange {
                start: next,
                end: seq,
            });
            self.enforce_capacity();
            SeqOutcome::Gap { missing }
        } else {
            self.observe_behind(seq)
        }
    }

    fn observe_behind(&mut self, seq: u64) -> SeqOutcome {
        // Gaps are sorted and few, so a linear scan is cheaper than a tree and
        // touches a single contiguous allocation.
        let Some(i) = self.gaps.iter().position(|g| g.contains(seq)) else {
            self.stats.duplicates += 1;
            return SeqOutcome::Duplicate;
        };

        let gap = self.gaps[i];
        if gap.len() == 1 {
            self.gaps.remove(i);
        } else if seq == gap.start {
            self.gaps[i].start += 1;
        } else if seq == gap.end - 1 {
            self.gaps[i].end -= 1;
        } else {
            self.gaps[i].end = seq;
            self.gaps.insert(
                i + 1,
                GapRange {
                    start: seq + 1,
                    end: gap.end,
                },
            );
            self.enforce_capacity();
        }
        self.stats.recovered += 1;
        self.stats.outstanding -= 1;
        SeqOutcome::Late
    }

    fn enforce_capacity(&mut self) {
        while self.gaps.len() > self.max_gaps {
            let oldest = self.gaps.remove(0);
            self.stats.evicted += oldest.len();
            self.stats.outstanding -= oldest.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_invariant(t: &SequenceTracker) {
        let s = t.stats();
        assert_eq!(s.missing_total, s.outstanding + s.recovered + s.evicted);
        let tracked: u64 = t.gaps().iter().map(GapRange::len).sum();
        assert_eq!(tracked, s.outstanding);
        assert!(t.gaps().iter().all(|g| !g.is_empty()));
        assert!(t.gaps().windows(2).all(|w| w[0].end <= w[1].start));
    }

    fn feed(t: &mut SequenceTracker, seqs: &[u64]) -> Vec<SeqOutcome> {
        seqs.iter()
            .map(|&s| {
                let o = t.observe(s);
                check_invariant(t);
                o
            })
            .collect()
    }

    use SeqOutcome::*;

    #[test]
    fn in_order_stream() {
        let mut t = SequenceTracker::new(8);
        assert_eq!(feed(&mut t, &[10, 11, 12]), [First, InOrder, InOrder]);
        assert_eq!(t.next_expected(), Some(13));
        assert_eq!(t.stats().in_order, 3);
    }

    #[test]
    fn gap_then_late_fill() {
        let mut t = SequenceTracker::new(8);
        let out = feed(&mut t, &[1, 2, 5, 3, 4, 6]);
        assert_eq!(
            out,
            [First, InOrder, Gap { missing: 2 }, Late, Late, InOrder]
        );
        let s = t.stats();
        assert_eq!(
            (s.gaps, s.missing_total, s.recovered, s.outstanding),
            (1, 2, 2, 0)
        );
        assert!(t.gaps().is_empty());
    }

    #[test]
    fn late_fill_in_middle_splits_gap() {
        let mut t = SequenceTracker::new(8);
        feed(&mut t, &[1, 10]);
        assert_eq!(t.observe(5), Late);
        assert_eq!(
            t.gaps(),
            [
                GapRange { start: 2, end: 5 },
                GapRange { start: 6, end: 10 }
            ]
        );
        check_invariant(&t);
    }

    #[test]
    fn duplicates_are_distinguished_from_late_data() {
        let mut t = SequenceTracker::new(8);
        let out = feed(&mut t, &[1, 2, 2, 4, 3, 3, 1]);
        assert_eq!(
            out,
            [
                First,
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
        let mut t = SequenceTracker::new(2);
        // Three gaps: {2}, {4}, {6}. Only the newest two can be tracked.
        feed(&mut t, &[1, 3, 5, 7]);
        let s = t.stats();
        assert_eq!(
            (s.gaps, s.missing_total, s.evicted, s.outstanding),
            (3, 3, 1, 2)
        );
        // The evicted number now looks like a duplicate; that is the
        // documented cost of bounded memory.
        assert_eq!(t.observe(2), Duplicate);
        assert_eq!(t.observe(4), Late);
        check_invariant(&t);
    }

    #[test]
    fn split_that_overflows_table_evicts() {
        let mut t = SequenceTracker::new(1);
        feed(&mut t, &[1, 10]);
        assert_eq!(t.observe(5), Late);
        check_invariant(&t);
        assert_eq!(t.gaps(), [GapRange { start: 6, end: 10 }]);
        assert_eq!(t.stats().evicted, 3);
    }

    #[test]
    fn reset_moves_outstanding_to_evicted() {
        let mut t = SequenceTracker::new(8);
        feed(&mut t, &[1, 5]);
        t.reset();
        check_invariant(&t);
        assert_eq!(t.stats().evicted, 3);
        assert_eq!(t.observe(1), First);
    }

    #[test]
    fn saturates_at_u64_max_without_panicking() {
        let mut t = SequenceTracker::new(8);
        assert_eq!(t.observe(u64::MAX - 1), First);
        assert_eq!(t.observe(u64::MAX), InOrder);
        assert_eq!(t.observe(u64::MAX), InOrder);
        check_invariant(&t);
    }

    #[test]
    fn deterministic_shuffle_conserves_accounting() {
        // Deliver 0..1000 with local reordering, some drops, and duplicates;
        // verify every number is accounted for exactly once.
        let mut rng = 12345u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut seqs: Vec<u64> = (0..1000).collect();
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
        let mut t = SequenceTracker::new(256);
        feed(&mut t, &stream);
        let s = t.stats();
        assert_eq!(s.evicted, 0);
        // Numbers that never arrived are outstanding, except a dropped
        // first/last number, which is undetectable by a sequence tracker.
        assert!(s.outstanding <= dropped);
        assert!(s.outstanding + 2 >= dropped);
    }
}
