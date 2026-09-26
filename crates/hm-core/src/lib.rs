//! Shared primitives for every protocol crate: time, the sans-IO `Machine`
//! trait, the standard station interface and a deterministic RNG.
//!
//! Nothing in here touches the operating system. Time and randomness are
//! always passed in, so the simulator and the real daemon drive identical code.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use core::ops::{Add, AddAssign};

/// Monotonic time in milliseconds since an arbitrary epoch chosen by the driver.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub u64);

impl Millis {
    pub const ZERO: Millis = Millis(0);

    pub const fn from_secs(s: u64) -> Millis {
        Millis(s * 1000)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn saturating_sub(self, other: Millis) -> Millis {
        Millis(self.0.saturating_sub(other.0))
    }
}

impl Add for Millis {
    type Output = Millis;
    fn add(self, rhs: Millis) -> Millis {
        Millis(self.0 + rhs.0)
    }
}

impl AddAssign for Millis {
    fn add_assign(&mut self, rhs: Millis) {
        self.0 += rhs.0;
    }
}

/// A sans-IO protocol state machine.
///
/// The driver (simulator or daemon) feeds inputs and deadline expiries, and
/// executes whatever the machine pushes into `out`. A machine never reads a
/// clock, never sleeps and never performs I/O.
pub trait Machine {
    type Input;
    type Output;

    /// Handle one input at time `now`.
    fn handle(&mut self, now: Millis, input: Self::Input, out: &mut Vec<Self::Output>);

    /// Called by the driver once `now >= next_deadline()`.
    fn on_deadline(&mut self, now: Millis, out: &mut Vec<Self::Output>);

    /// The earliest time this machine wants `on_deadline` to be called, if any.
    fn next_deadline(&self) -> Option<Millis>;
}

/// Local radio interface number. A station may have several radios, for
/// example port 0 on the VHF access channel and port 1 on an HF backbone link.
pub type Port = u8;

/// Standard input of a station-level machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input<C> {
    /// A frame decoded by the bearer on `port` (after the modem's own FEC and CRC).
    Frame { port: Port, data: Vec<u8> },
    /// A command from the local application or operator.
    Command(C),
}

/// Standard output of a station-level machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output<E> {
    /// Ask the bearer on `port` to transmit this frame as soon as its radio is free.
    Transmit { port: Port, data: Vec<u8> },
    /// An event for the local application (message delivered, link state, ...).
    Event(E),
}

/// One step of SplitMix64. Used to seed [`DetRng`] and to derive sub-streams.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic xoshiro256** generator.
///
/// Implemented here rather than taken from a crate so that simulator seeds
/// keep producing byte-identical runs across dependency upgrades.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetRng {
    s: [u64; 4],
}

impl DetRng {
    pub fn from_seed(seed: u64) -> DetRng {
        let mut sm = seed;
        let s = [
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
        ];
        DetRng { s }
    }

    /// An independent generator for sub-stream `stream` (for example one per node).
    pub fn fork(&self, stream: u64) -> DetRng {
        let mut sm = self.s[0] ^ self.s[3].rotate_left(17) ^ stream.wrapping_mul(0xA24B_AED4_963E_E407);
        DetRng::from_seed(splitmix64(&mut sm))
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in `[0, 1)` with 53 bits of precision.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in `[0, n)`, without modulo bias. Returns 0 when `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let x = self.next_u64();
            if x < zone {
                return x % n;
            }
        }
    }

    /// True with probability `p` (clamped to `[0, 1]`).
    pub fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            false
        } else if p >= 1.0 {
            true
        } else {
            self.next_f64() < p
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_reference_value() {
        // Widely published first output of SplitMix64 seeded with 0.
        let mut s = 0u64;
        assert_eq!(splitmix64(&mut s), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn rng_is_deterministic_and_forks_differ() {
        let mut a = DetRng::from_seed(42);
        let mut b = DetRng::from_seed(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let root = DetRng::from_seed(42);
        let mut f1 = root.fork(1);
        let mut f2 = root.fork(2);
        assert_ne!(f1.next_u64(), f2.next_u64());
    }

    #[test]
    fn below_and_unit_interval_stay_in_range() {
        let mut r = DetRng::from_seed(7);
        for n in 1..200u64 {
            assert!(r.below(n) < n);
        }
        for _ in 0..10_000 {
            let x = r.next_f64();
            assert!((0.0..1.0).contains(&x));
        }
        assert_eq!(r.below(0), 0);
    }

    #[test]
    fn chance_matches_probability_roughly() {
        let mut r = DetRng::from_seed(3);
        let hits = (0..100_000).filter(|_| r.chance(0.25)).count();
        assert!((24_000..26_000).contains(&hits), "hits = {hits}");
    }

    #[test]
    fn millis_arithmetic() {
        assert_eq!(Millis::from_secs(2) + Millis(5), Millis(2005));
        assert_eq!(Millis(5).saturating_sub(Millis(9)), Millis::ZERO);
    }
}
