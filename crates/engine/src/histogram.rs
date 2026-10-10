//! Fixed-memory log-linear latency histogram.
//!
//! Storing every sample to compute exact percentiles costs memory proportional
//! to the sample count and an allocation-prone growing `Vec`. Instead, values
//! are counted in buckets whose width grows with magnitude (the same idea as
//! HdrHistogram):
//!
//! * values `0..32` get one exact bucket each;
//! * every power-of-two range `[2^k, 2^(k+1))` for `k >= 5` is split into 32
//!   equal sub-buckets.
//!
//! Each bucket is therefore at most 1/32 (~3.1%) of its value wide. Recording
//! is a `leading_zeros` instruction, a shift, a mask, and an increment, with no
//! allocation. Reported percentiles are the *upper* bound of the bucket, so
//! the histogram may over-report latency by up to 3.1% but never
//! under-reports it.

use std::fmt;

const SUB_BITS: u32 = 5;
const SUB_COUNT: usize = 1 << SUB_BITS;
/// `SUB_COUNT` exact buckets plus `SUB_COUNT` per power of two from 2^5..2^63.
const BUCKETS: usize = SUB_COUNT + (64 - SUB_BITS as usize) * SUB_COUNT;

/// Fixed-memory histogram of `u64` values (typically nanoseconds).
#[derive(Clone)]
pub struct LatencyHistogram {
    // 1920 * 8 bytes = 15 KiB. Boxed so the struct itself is small and cheap
    // to move, and so it never risks overflowing a thread's stack.
    counts: Box<[u64]>,
    total: u64,
    sum: u128,
    min: u64,
    max: u64,
}

/// Summary statistics extracted from a [`LatencyHistogram`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LatencySummary {
    /// Number of samples.
    pub count: u64,
    /// Exact minimum.
    pub min: u64,
    /// Median (bucket upper bound).
    pub p50: u64,
    /// 95th percentile.
    pub p95: u64,
    /// 99th percentile.
    pub p99: u64,
    /// 99.9th percentile.
    pub p999: u64,
    /// Exact maximum.
    pub max: u64,
    /// Exact arithmetic mean, rounded down.
    pub mean: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for LatencyHistogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LatencyHistogram")
            .field("summary", &self.summary())
            .finish()
    }
}

#[inline]
fn bucket_index(value: u64) -> usize {
    if value < SUB_COUNT as u64 {
        return value as usize;
    }
    let msb = 63 - value.leading_zeros(); // >= SUB_BITS
    let shift = msb - SUB_BITS;
    let mantissa = ((value >> shift) as usize) & (SUB_COUNT - 1);
    SUB_COUNT + (shift as usize) * SUB_COUNT + mantissa
}

/// Largest value that maps to bucket `index`.
fn bucket_upper(index: usize) -> u64 {
    if index < SUB_COUNT {
        return index as u64;
    }
    let shift = ((index - SUB_COUNT) / SUB_COUNT) as u32;
    let mantissa = ((index - SUB_COUNT) % SUB_COUNT) as u64;
    let msb = shift + SUB_BITS;
    let lower = (1u64 << msb) | (mantissa << shift);
    lower + ((1u64 << shift) - 1)
}

impl LatencyHistogram {
    /// Creates an empty histogram. This is the only allocation it performs.
    pub fn new() -> Self {
        Self {
            counts: vec![0; BUCKETS].into_boxed_slice(),
            total: 0,
            sum: 0,
            min: u64::MAX,
            max: 0,
        }
    }

    /// Records one sample.
    #[inline]
    pub fn record(&mut self, value: u64) {
        self.counts[bucket_index(value)] += 1;
        self.total += 1;
        self.sum += u128::from(value);
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    /// Number of recorded samples.
    pub fn count(&self) -> u64 {
        self.total
    }

    /// Value at percentile `p` (0.0..=100.0), or 0 if empty.
    ///
    /// Returns the upper bound of the bucket containing the sample of rank
    /// `ceil(p/100 * count)`, clamped to the exact observed min/max.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let p = p.clamp(0.0, 100.0);
        let rank = ((p / 100.0) * self.total as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return bucket_upper(i).clamp(self.min, self.max);
            }
        }
        self.max
    }

    /// Computes the standard summary.
    pub fn summary(&self) -> LatencySummary {
        if self.total == 0 {
            return LatencySummary::default();
        }
        LatencySummary {
            count: self.total,
            min: self.min,
            p50: self.percentile(50.0),
            p95: self.percentile(95.0),
            p99: self.percentile(99.0),
            p999: self.percentile(99.9),
            max: self.max,
            mean: (self.sum / u128::from(self.total)) as u64,
        }
    }

    /// Clears all samples without freeing memory.
    pub fn reset(&mut self) {
        self.counts.fill(0);
        self.total = 0;
        self.sum = 0;
        self.min = u64::MAX;
        self.max = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_bounds_are_contiguous_and_cover_u64() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(u64::MAX), BUCKETS - 1);
        assert_eq!(bucket_upper(BUCKETS - 1), u64::MAX);
        for i in 1..BUCKETS {
            let lower_i = bucket_upper(i - 1) + 1;
            assert_eq!(bucket_index(lower_i), i, "lower edge of bucket {i}");
            assert_eq!(bucket_index(bucket_upper(i)), i, "upper edge of bucket {i}");
        }
    }

    #[test]
    fn relative_error_is_bounded() {
        let mut v = 1u64;
        while v < u64::MAX / 3 {
            let upper = bucket_upper(bucket_index(v));
            assert!(upper >= v);
            assert!((upper - v) as f64 <= v as f64 / SUB_COUNT as f64, "v={v}");
            v = v * 3 + 1;
        }
    }

    #[test]
    fn small_values_are_exact() {
        let mut h = LatencyHistogram::new();
        for v in 1..=10 {
            h.record(v);
        }
        assert_eq!(h.percentile(50.0), 5);
        assert_eq!(h.percentile(100.0), 10);
        assert_eq!(h.percentile(0.0), 1);
        let s = h.summary();
        assert_eq!((s.count, s.min, s.max, s.mean), (10, 1, 10, 5));
    }

    #[test]
    fn percentiles_of_uniform_distribution() {
        let mut h = LatencyHistogram::new();
        for v in 1..=100_000u64 {
            h.record(v);
        }
        for (p, exact) in [(50.0, 50_000u64), (99.0, 99_000), (99.9, 99_900)] {
            let got = h.percentile(p);
            assert!(got >= exact, "p{p}: {got} < {exact}");
            assert!(
                got as f64 <= exact as f64 * (1.0 + 1.0 / 32.0),
                "p{p}: {got}"
            );
        }
        assert_eq!(h.summary().max, 100_000);
    }

    #[test]
    fn empty_and_reset() {
        let mut h = LatencyHistogram::new();
        assert_eq!(h.summary(), LatencySummary::default());
        h.record(1_000_000);
        h.reset();
        assert_eq!(h.count(), 0);
        assert_eq!(h.percentile(99.0), 0);
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let mut h = LatencyHistogram::new();
        h.record(u64::MAX);
        h.record(u64::MAX);
        assert_eq!(h.summary().mean, u64::MAX);
        assert_eq!(h.percentile(50.0), u64::MAX);
    }
}
