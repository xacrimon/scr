//! A small 32-bit PCG random number generator.
//!
//! This is a direct port of the reference `pcg32_random_r` implementation
//! using the XSH RR output function.

/// The LCG multiplier used by PCG for a 64-bit state.
const MULTIPLIER: u64 = 6364136223846793005;

/// A PCG random number generator producing 32 bits per step.
///
/// The generator is not cryptographically secure and must not be used where
/// unpredictability matters.
pub(crate) struct Rand32 {
    /// The LCG state, advanced on every step.
    state: u64,
    /// The stream selector. Only the upper 63 bits matter, the low bit is
    /// always forced to one to keep the LCG full period.
    inc: u64,
}

impl Rand32 {
    /// Create a generator directly from a raw state and stream selector.
    pub(crate) const fn new(state: u64, inc: u64) -> Self {
        Self { state, inc }
    }

    pub(crate) fn with_random_seed() -> Self {
        let mut rng_seed = [0u8; 16];
        getrandom::fill(&mut rng_seed).unwrap();
        let rng_seed_lo = u64::from_ne_bytes(rng_seed[0..8].try_into().unwrap());
        let rng_seed_hi = u64::from_ne_bytes(rng_seed[8..16].try_into().unwrap());
        Self::new(rng_seed_lo, rng_seed_hi)
    }

    /// Advance the state and return the next 32 bits of output.
    pub(crate) fn next_u32(&mut self) -> u32 {
        let oldstate: u64 = self.state;

        // Advance internal state.
        self.state = oldstate.wrapping_mul(MULTIPLIER).wrapping_add(self.inc | 1);

        // Calculate output function (XSH RR), uses old state for max ILP.
        let xorshifted = (((oldstate >> 18) ^ oldstate) >> 27) as u32;
        let rot = (oldstate >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    pub(crate) fn next_u32_below(&mut self, max: u32) -> u32 {
        // https://lemire.me/blog/2016/06/27/a-fast-alternative-to-the-modulo-reduction/
        let mul = (self.next_u32() as u64).wrapping_mul(max as u64);
        (mul >> 32) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::Rand32;

    // Expected values are taken from the reference C implementation.
    #[test]
    fn matches_reference_output() {
        let mut rng = Rand32::new(42, 54);
        let output: [u32; 6] = std::array::from_fn(|_| rng.next_u32());

        assert_eq!(
            output,
            [0x00000000, 0x0c855c84, 0x452a1874, 0x126f419d, 0xb0eb774d, 0xd986ea86]
        );
    }

    // The rotation amount is zero for a zero state, which is where a naive
    // translation of the C shifts would overflow.
    #[test]
    fn zero_state_does_not_overflow() {
        let mut rng = Rand32::new(0, 0);
        let output: [u32; 4] = std::array::from_fn(|_| rng.next_u32());

        assert_eq!(output, [0x00000000, 0x00000000, 0xe4c14788, 0x379c6516]);
    }
}
