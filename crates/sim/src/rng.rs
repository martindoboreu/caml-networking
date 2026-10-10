/// SplitMix64 pseudo-random generator.
///
/// Tiny, fast, and statistically adequate for synthetic data. Not suitable
/// for cryptography. Implemented locally to keep the workspace free of an RNG
/// dependency and to guarantee the sequence never changes across versions.
#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Creates a generator from a seed.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next 64 random bits.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..bound`. Returns 0 if `bound` is 0.
    ///
    /// Uses the multiply-shift method, which has negligible bias for the small
    /// bounds used here.
    #[inline]
    pub fn below(&mut self, bound: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(bound)) >> 64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_values() {
        // First outputs for seed 0 from the reference SplitMix64 algorithm.
        let mut r = SplitMix64::new(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = SplitMix64::new(7);
        for bound in [1u64, 2, 3, 10, 1000] {
            for _ in 0..1000 {
                assert!(r.below(bound) < bound);
            }
        }
        assert_eq!(r.below(0), 0);
    }
}
