//! Whether a path is open, followed from moment to moment through a
//! transfer: the forward step of the two-state hidden Markov model the link
//! model keeps ([`crate::LinkModel`]).
//!
//! Between observations the chance the path is open relaxes toward the daily
//! pattern `π` over the path's correlation time `T`:
//!
//! ```text
//! p(t + Δ) = π + (p(t) − π) · e^{−Δ/T}
//! ```
//!
//! Hearing the far end says the path is open. Something sent that would have
//! been answered with chance `a` were the path open, and was not answered, is
//! Bayes' rule on the two states:
//!
//! ```text
//! p' = p (1 − a) / (p (1 − a) + 1 − p)
//! ```
//!
//! Hearing makes the path certain to be open only then: the next moment the
//! chance starts to relax again, so silence after it keeps counting.

use libm::exp;

/// The chance a path is open, where it relaxes to, and how fast.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Openness {
    /// Chance the path is open now.
    pub p: f64,
    /// The daily pattern's chance for the hours ahead: where `p` relaxes to.
    pub daily: f64,
    /// Correlation time of the open/closed state, seconds.
    pub persistence_secs: f64,
}

impl Openness {
    /// A path taken to be open, and to stay open.
    pub const OPEN: Openness = Openness {
        p: 1.0,
        daily: 1.0,
        persistence_secs: f64::INFINITY,
    };

    /// `secs` later, with nothing observed meanwhile.
    pub fn after(self, secs: f64) -> Openness {
        let decay = exp(-secs.max(0.0) / self.persistence_secs.max(1.0));
        Openness {
            p: self.daily + (self.p - self.daily) * decay,
            ..self
        }
    }

    /// The far end was just heard.
    pub fn heard(self) -> Openness {
        Openness { p: 1.0, ..self }
    }

    /// Something that would have been answered with chance `answered_if_open`
    /// were the path open went unanswered.
    pub fn unanswered(self, answered_if_open: f64) -> Openness {
        let silent = self.p * (1.0 - answered_if_open.clamp(0.0, 1.0));
        let p = silent / (silent + 1.0 - self.p);
        Openness {
            p: if p.is_finite() { p } else { self.p },
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: Openness = Openness {
        p: 1.0,
        daily: 0.4,
        persistence_secs: 3_600.0,
    };

    #[test]
    fn heard_then_silent_the_chance_falls_and_relaxes_to_the_daily_pattern() {
        // Just heard, a probe unanswered at once barely moves it (an open
        // path loses some probes), ...
        let once = PATH.after(10.0).unanswered(0.5);
        assert!(once.p > 0.99 && once.p < 1.0, "{once:?}");
        // ... a few in a row do, ...
        let many = (0..6).fold(PATH, |o, _| o.after(60.0).unanswered(0.8));
        assert!(many.p < 0.5, "{many:?}");
        // ... and with nothing heard it returns to the daily pattern.
        assert!((PATH.after(10.0 * 3_600.0).p - 0.4).abs() < 1e-3);
        assert_eq!(many.heard().p, 1.0);
    }

    #[test]
    fn a_path_taken_to_be_open_stays_open() {
        let o = Openness::OPEN.after(1.0e6);
        assert_eq!(o.p, 1.0);
        assert_eq!(o.unanswered(0.0).p, 1.0);
    }
}
