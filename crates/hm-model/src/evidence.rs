//! Beta beliefs about a rate, from a prior and evidence that fades.
//!
//! Generative story: a rate `p` (a link's chance of completing a handoff, a
//! custodian's of delivering) drifts slowly; each trial succeeds with
//! probability `p`. Exponential forgetting of the evidence (a power prior, the
//! discount factor of West and Harrison's dynamic models) is the conjugate
//! approximation to that drift: evidence `half_life` seconds old counts half.
//! Forgetting shrinks the evidence, never the prior, so a belief without
//! recent evidence returns to the prior rather than to nothing.

use hm_core::DetRng;
use minicbor::{Decode, Encode};

use crate::math;

/// A Beta(a, b) belief about a probability.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Beta {
    pub a: f64,
    pub b: f64,
}

impl Beta {
    /// Beta with mean `mean` and `strength` pseudo-observations.
    pub fn with_mean(mean: f64, strength: f64) -> Beta {
        let mean = mean.clamp(1.0e-6, 1.0 - 1.0e-6);
        let strength = strength.max(1.0e-6);
        Beta {
            a: mean * strength,
            b: (1.0 - mean) * strength,
        }
    }

    pub fn mean(&self) -> f64 {
        self.a / (self.a + self.b)
    }

    pub fn variance(&self) -> f64 {
        let n = self.a + self.b;
        self.a * self.b / (n * n * (n + 1.0))
    }

    /// Pseudo-observations behind the belief.
    pub fn strength(&self) -> f64 {
        self.a + self.b
    }

    pub fn quantile(&self, p: f64) -> f64 {
        math::beta_quantile(self.a, self.b, p)
    }

    /// One draw of the probability (Thompson sampling).
    pub fn sample(&self, rng: &mut DetRng) -> f64 {
        math::beta(rng, self.a, self.b)
    }
}

/// A prior for a rate: its mean and how many observations it is worth.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Prior {
    pub mean: f64,
    pub strength: f64,
}

impl Prior {
    pub const fn new(mean: f64, strength: f64) -> Prior {
        Prior { mean, strength }
    }

    pub fn beta(&self) -> Beta {
        Beta::with_mean(self.mean, self.strength)
    }
}

/// Successes and failures seen, faded with time, as of `at` (Unix seconds).
#[derive(Copy, Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct Evidence {
    #[n(0)]
    pub yes: f64,
    #[n(1)]
    pub no: f64,
    #[n(2)]
    pub at: u64,
}

impl Evidence {
    /// The evidence as it counts at `now`.
    pub fn faded(&self, now: u64, half_life: u64) -> (f64, f64) {
        let f = math::fade(now.saturating_sub(self.at), half_life);
        (self.yes * f, self.no * f)
    }

    /// Add `yes` successes and `no` failures observed at `at`. Evidence older
    /// than what is held is faded to the held time instead of turning it back.
    pub fn add(&mut self, yes: f64, no: f64, at: u64, half_life: u64) {
        if !(yes.is_finite() && no.is_finite()) || yes < 0.0 || no < 0.0 {
            return;
        }
        if self.yes == 0.0 && self.no == 0.0 {
            *self = Evidence { yes, no, at };
        } else if at >= self.at {
            let f = math::fade(at - self.at, half_life);
            *self = Evidence {
                yes: self.yes * f + yes,
                no: self.no * f + no,
                at,
            };
        } else {
            let f = math::fade(self.at - at, half_life);
            self.yes += yes * f;
            self.no += no * f;
        }
    }

    /// The posterior from `prior` and this evidence at `now`.
    pub fn posterior(&self, prior: Prior, now: u64, half_life: u64) -> Beta {
        let (yes, no) = self.faded(now, half_life);
        let p = prior.beta();
        Beta {
            a: p.a + yes,
            b: p.b + no,
        }
    }

    /// Faded weight of all the evidence at `now`.
    pub fn weight(&self, now: u64, half_life: u64) -> f64 {
        let (yes, no) = self.faded(now, half_life);
        yes + no
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    #[test]
    fn evidence_fades_back_to_the_prior() {
        let prior = Prior::new(0.8, 2.0);
        let mut e = Evidence::default();
        for t in 0..20 {
            e.add(0.0, 1.0, t, DAY);
        }
        let now = e.posterior(prior, 20, DAY).mean();
        assert!(now < 0.2, "{now}");
        let later = e.posterior(prior, 60 * DAY, DAY).mean();
        assert!((later - 0.8).abs() < 1e-3, "{later}");
    }

    #[test]
    fn late_evidence_does_not_turn_back_the_clock() {
        let mut e = Evidence::default();
        e.add(1.0, 0.0, 2 * DAY, DAY);
        e.add(1.0, 0.0, DAY, DAY);
        assert_eq!(e.at, 2 * DAY);
        assert!((e.yes - 1.5).abs() < 1e-12);
    }

    #[test]
    fn a_beta_with_a_mean_keeps_it() {
        let b = Beta::with_mean(0.3, 10.0);
        assert!((b.mean() - 0.3).abs() < 1e-12);
        assert!((b.strength() - 10.0).abs() < 1e-12);
    }
}
