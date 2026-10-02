//! A tiny, fully specified pseudo-random generator.
//!
//! Fixture output must be byte-identical on every platform and must not
//! change when a dependency is upgraded, so the generator is implemented
//! here instead of taken from a crate: SplitMix64 (Steele, Lea & Flood,
//! 2014) uses only wrapping 64-bit arithmetic.

/// SplitMix64 generator. Not cryptographic; used only for synthetic data.
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Creates a generator from a seed. Every seed (including 0) is valid.
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Creates an independent stream for `label`, so that adding output to
    /// one stream never shifts the values of another.
    pub(crate) fn derive(seed: u64, label: &str) -> Self {
        Self::new(seed ^ fnv1a64(label.as_bytes()))
    }

    /// Next 64 random bits.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish value in `0..n` (multiply-shift; bias < 2^-32 for the
    /// small `n` used here). Returns 0 when `n == 0`.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let wide = u128::from(self.next_u64()) * (n as u128);
        (wide >> 64) as usize
    }

    /// Value in `lo..=hi` (returns `lo` when `hi < lo`).
    pub(crate) fn range(&mut self, lo: usize, hi: usize) -> usize {
        if hi < lo {
            return lo;
        }
        lo + self.below(hi - lo + 1)
    }

    /// True with probability `percent / 100`.
    pub(crate) fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// A random element, or `""` for an empty slice.
    pub(crate) fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        let index = self.below(items.len());
        items.get(index).copied().unwrap_or_default()
    }

    /// A random element of a non-string slice, or `None` when empty.
    pub(crate) fn pick_ref<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        let index = self.below(items.len());
        items.get(index)
    }
}

/// FNV-1a, 64-bit: a stable, dependency-free hash for deriving streams.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_splitmix64_outputs() {
        // Reference values of SplitMix64 for seed 0.
        let mut rng = SplitMix64::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn fnv_reference_values() {
        assert_eq!(fnv1a64(b""), 0xCBF2_9CE4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xAF63_DC4C_8601_EC8C);
    }

    #[test]
    fn helpers_stay_in_range() {
        let mut rng = SplitMix64::new(7);
        for _ in 0..1000 {
            assert!(rng.below(5) < 5);
            let v = rng.range(3, 6);
            assert!((3..=6).contains(&v));
        }
        assert_eq!(rng.below(0), 0);
        assert_eq!(rng.range(4, 2), 4);
        assert_eq!(rng.pick(&[]), "");
        assert!(rng.pick_ref::<u8>(&[]).is_none());
    }

    #[test]
    fn derived_streams_are_independent_and_stable() {
        let a1 = SplitMix64::derive(42, "a").next_u64();
        let a2 = SplitMix64::derive(42, "a").next_u64();
        let b = SplitMix64::derive(42, "b").next_u64();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }
}
