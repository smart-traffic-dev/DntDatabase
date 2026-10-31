//! SplitMix64: a small, fast, seedable PRNG (no dependency). Every cell gets its own
//! generator from `(seed, row, column)`, so changing one column never changes another.

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// The splitmix64 finaliser.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(state: u64) -> SplitMix64 {
        SplitMix64(state)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GAMMA);
        mix(self.0)
    }

    /// Uniform in `0..n` (0 when `n` is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// Uniform in `lo..=hi` (`lo` ≤ `hi`, span at most 2^64).
    pub fn range_i128(&mut self, lo: i128, hi: i128) -> i128 {
        let span = (hi - lo) as u128 + 1;
        lo + ((u128::from(self.next_u64()) * span) >> 64) as i128
    }

    /// `true` with probability `p` / 100.
    pub fn percent(&mut self, p: u8) -> bool {
        self.below(100) < u64::from(p)
    }
}

/// The generator of one cell.
pub fn cell_rng(seed: u32, row: u64, column: u32) -> SplitMix64 {
    let state = mix(u64::from(seed).wrapping_add(GAMMA))
        ^ mix(row.wrapping_mul(GAMMA) ^ (u64::from(column) << 32 | 0xA5A5));
    SplitMix64::new(mix(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first4(mut r: SplitMix64) -> [u64; 4] {
        [r.next_u64(), r.next_u64(), r.next_u64(), r.next_u64()]
    }

    #[test]
    fn cell_rng_is_deterministic_and_independent() {
        assert_eq!(first4(cell_rng(7, 3, 2)), first4(cell_rng(7, 3, 2)));
        assert_ne!(first4(cell_rng(7, 3, 2)), first4(cell_rng(7, 3, 1)));
        assert_ne!(first4(cell_rng(7, 3, 2)), first4(cell_rng(7, 4, 2)));
        assert_ne!(first4(cell_rng(7, 3, 2)), first4(cell_rng(8, 3, 2)));
    }

    #[test]
    fn below_and_range_stay_in_bounds() {
        let mut r = SplitMix64::new(1);
        let mut seen = [false; 7];
        for _ in 0..10_000 {
            let v = r.range_i128(-3, 3);
            assert!((-3..=3).contains(&v));
            seen[(v + 3) as usize] = true;
            assert!(r.below(5) < 5);
        }
        assert!(seen.iter().all(|s| *s));
        assert_eq!(r.below(0), 0);
        // The widest span (u64) does not overflow.
        let v = r.range_i128(0, u64::MAX as i128);
        assert!((0..=u64::MAX as i128).contains(&v));
    }

    #[test]
    fn percent_edges() {
        let mut r = SplitMix64::new(9);
        assert!((0..1000).all(|_| !r.percent(0)));
        assert!((0..1000).all(|_| r.percent(100)));
    }
}
