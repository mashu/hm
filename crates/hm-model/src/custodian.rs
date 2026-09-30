//! A station that takes custody: does it accept, does it deliver, how long
//! does delivery take?
//!
//! Generative story, per custodian `n`:
//!
//! ```text
//! a_n  = P(accepts custody | the handoff reached it)      Beta, fading evidence
//! r_n  = P(does its part | it accepted)                    Beta, fading evidence
//! ℓ    = how late the end-to-end receipt comes back, past the time the
//!        route planned the message to arrive (the way back, and any slip):
//!        ln max(ℓ, 1 min) ~ N(μ, 1/λ),  (μ, λ) ~ Normal-Gamma   fading statistics
//! ```
//!
//! Measuring from the route's own arrival, not from the handoff, keeps what
//! the router already knows out of the custodian's account: a route that
//! waits for the morning opening is not a slow custodian.
//!
//! A message handed to `n` reaches its destination with probability `r_n q`,
//! where `q` is the rest of the route's chance, which the router predicted.
//! No receipt yet, `ℓ` past the planned arrival, is a mixture: `n` dropped
//! it (weight `1 − r_n`), the rest of the way failed (`r_n (1 − q)`), or
//! the receipt is still on its way (`r_n q (1 − F(ℓ))`). Assumed density
//! filtering gives `n` only its share of the blame,
//! `(1 − r_n) / (1 − r_n q F(ℓ))`: silence soon after the planned arrival
//! says little, since receipts are often later than that. (Counting it as a
//! loss outright would teach that custodians drop what they only delay, and
//! reclaim ever sooner.) A receipt that comes after all, even after custody
//! was reclaimed, counts as delivered with its true lateness: the delays are
//! learned from the slow receipts too, not only from those quick enough to
//! beat the timer.
//!
//! A refusal or a "busy" answer is the custodian's, not the link's: the link
//! carried the question and the answer.
//!
//! The custody suspect timer becomes a stopping problem: when to reclaim
//! custody and send again, maybe another way. Resending `τ` past the planned
//! arrival (if no receipt came by then) rescues a lost message, worth
//! `V·u(τ)` with the chance `a` of the other way, where `u` falls from 1 now
//! to 0 at expiry; it costs a copy's airtime `A` whenever no receipt came,
//! including when the first copy was only slow. Up to terms that do not
//! depend on `τ`,
//!
//! ```text
//! G(τ) = V·L·a·u(τ) − A·(L + p·S(τ))      L = 1 − p lost,  p = r_n q,
//!                                          S(τ) = P(ℓ > τ)
//! ```
//!
//! `τ*` maximises `G`; when no `τ` makes it positive, resending never pays
//! and custody is not reclaimed before the bound. Waiting pays while receipts
//! are still likely to arrive (`A p f(τ)`), and stops paying as the rescue
//! loses value (`V L a u'(τ)`): a reliable custodian with quick receipts is
//! given about as long as its receipts take, an unreliable one less.

use libm::{exp, log, sqrt};
use minicbor::{Decode, Encode};

use crate::evidence::{Beta, Evidence, Prior};
use crate::math;

const DAY: u64 = 86_400;
/// Memory of a custodian's behaviour.
pub const CUSTODIAN_HALF_LIFE: u64 = 14 * DAY;

/// Population prior for custodians.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CustodianPrior {
    pub accepts: Prior,
    pub delivers: Prior,
    /// Normal-Gamma prior on ln(delay seconds): mean, its pseudo-count, and
    /// the Gamma shape and rate of the precision.
    pub delay_mean: f64,
    pub delay_count: f64,
    pub delay_shape: f64,
    pub delay_rate: f64,
}

impl Default for CustodianPrior {
    fn default() -> Self {
        CustodianPrior {
            accepts: Prior::new(0.9, 2.0),
            delivers: Prior::new(0.8, 2.0),
            // A receipt comes back about an hour after the planned arrival,
            // give or take a factor of four.
            delay_mean: log(3_600.0),
            delay_count: 1.0,
            delay_shape: 2.0,
            delay_rate: 2.0,
        }
    }
}

/// The shortest lateness learned from: a receipt earlier than planned says
/// the custodian was quick, not how much quicker.
const MIN_LATE_SECS: f64 = 60.0;

/// Something seen that bears on a custodian.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CustodianObservation {
    /// It took custody.
    Accepted,
    /// It refused custody (policy: trust, size, loops).
    Refused,
    /// It is busy for `retry_after` seconds.
    Busy { retry_after: u64 },
    /// An end-to-end receipt came back for a message it took, `late_secs`
    /// past the time the route planned the message to arrive.
    Delivered { late_secs: u64 },
    /// No receipt has come back `late_secs` past the planned arrival, and
    /// custody was reclaimed; the rest of the route beyond it had chance
    /// `downstream`.
    Silent { downstream: f64, late_secs: u64 },
}

/// A handoff of custody, as far as deciding when to reclaim it goes.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct HandedOver {
    /// The route's chance beyond the custodian.
    pub downstream: f64,
    /// The chance of the best other way, were custody reclaimed.
    pub alternative: f64,
    /// A delivered message's worth, in copies sent.
    pub value: f64,
    /// Seconds from the handoff to the route's planned arrival.
    pub expected_secs: u64,
    /// Seconds from the handoff until the message expires.
    pub remaining_secs: u64,
}

/// Fading sufficient statistics of ln(delay).
#[derive(Copy, Clone, Debug, Default, PartialEq, Encode, Decode)]
struct Delays {
    #[n(0)]
    count: f64,
    #[n(1)]
    sum: f64,
    #[n(2)]
    sum_sq: f64,
    #[n(3)]
    at: u64,
}

impl Delays {
    fn add(&mut self, x: f64, at: u64) {
        let f = if self.count == 0.0 {
            0.0
        } else {
            math::fade(at.saturating_sub(self.at), CUSTODIAN_HALF_LIFE)
        };
        *self = Delays {
            count: self.count * f + 1.0,
            sum: self.sum * f + x,
            sum_sq: self.sum_sq * f + x * x,
            at: at.max(self.at),
        };
    }

    /// Posterior predictive of ln(delay): Student t as (degrees, location, scale).
    fn predictive(&self, prior: &CustodianPrior, now: u64) -> (f64, f64, f64) {
        let f = math::fade(now.saturating_sub(self.at), CUSTODIAN_HALF_LIFE);
        let n = self.count * f;
        let (k0, m0, a0, b0) = (
            prior.delay_count,
            prior.delay_mean,
            prior.delay_shape,
            prior.delay_rate,
        );
        let (mean, spread) = if n > 0.0 {
            let mean = self.sum / self.count;
            (mean, (self.sum_sq / self.count - mean * mean).max(0.0) * n)
        } else {
            (m0, 0.0)
        };
        let k = k0 + n;
        let m = (k0 * m0 + n * mean) / k;
        let a = a0 + 0.5 * n;
        let b = b0 + 0.5 * spread + k0 * n * (mean - m0) * (mean - m0) / (2.0 * k);
        (2.0 * a, m, sqrt(b * (k + 1.0) / (a * k)))
    }
}

/// Belief about one custodian.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct CustodianModel {
    #[n(0)]
    accepts: Evidence,
    #[n(1)]
    delivers: Evidence,
    #[n(2)]
    delays: Delays,
    #[n(3)]
    busy_until: u64,
    #[n(4)]
    observed_at: u64,
}

impl CustodianModel {
    pub fn observed_at(&self) -> u64 {
        self.observed_at
    }

    pub fn busy_until(&self) -> u64 {
        self.busy_until
    }

    /// Custody offers seen, faded to `now`: (accepted, refused).
    pub fn accept_counts(&self, now: u64) -> (f64, f64) {
        self.accepts.faded(now, CUSTODIAN_HALF_LIFE)
    }

    /// Messages it took, faded to `now`: (delivered, lost by it).
    pub fn deliver_counts(&self, now: u64) -> (f64, f64) {
        self.delivers.faded(now, CUSTODIAN_HALF_LIFE)
    }

    pub fn accepts(&self, prior: &CustodianPrior, now: u64) -> Beta {
        self.accepts.posterior(prior.accepts, now, CUSTODIAN_HALF_LIFE)
    }

    pub fn delivers(&self, prior: &CustodianPrior, now: u64) -> Beta {
        self.delivers.posterior(prior.delivers, now, CUSTODIAN_HALF_LIFE)
    }

    /// Chance it accepts custody of a handoff that reaches it at `t`: none
    /// while it said it is busy.
    pub fn p_accept(&self, prior: &CustodianPrior, t: u64, now: u64) -> f64 {
        if t < self.busy_until {
            0.0
        } else {
            self.accepts(prior, now).mean()
        }
    }

    /// Chance a receipt for a message it took has come back by `late_secs`
    /// past the planned arrival.
    pub fn delay_cdf(&self, prior: &CustodianPrior, late_secs: u64, now: u64) -> f64 {
        let (nu, m, s) = self.delays.predictive(prior, now);
        math::student_t_cdf((log(late_secs.max(1) as f64) - m) / s, nu)
    }

    pub fn observe(&mut self, prior: &CustodianPrior, at: u64, observation: CustodianObservation) {
        let h = CUSTODIAN_HALF_LIFE;
        match observation {
            CustodianObservation::Accepted => self.accepts.add(1.0, 0.0, at, h),
            CustodianObservation::Refused => self.accepts.add(0.0, 1.0, at, h),
            CustodianObservation::Busy { retry_after } => {
                self.busy_until = self.busy_until.max(at.saturating_add(retry_after));
            }
            CustodianObservation::Delivered { late_secs } => {
                self.delivers.add(1.0, 0.0, at, h);
                self.delays.add(log((late_secs as f64).max(MIN_LATE_SECS)), at);
            }
            CustodianObservation::Silent {
                downstream,
                late_secs,
            } => {
                let r = self.delivers(prior, at).mean();
                let q = downstream.clamp(0.0, 1.0);
                let back = self.delay_cdf(prior, late_secs, at);
                // P(it dropped the message | no receipt yet).
                let blame = (1.0 - r) / (1.0 - r * q * back).max(1.0e-9);
                self.delivers.add(0.0, blame.clamp(0.0, 1.0), at, h);
            }
        }
        self.observed_at = self.observed_at.max(at);
    }

    /// How long after a handoff to wait for the end-to-end receipt before
    /// reclaiming custody: the planned arrival, then the lateness `τ` in
    /// `bounds` that maximises the expected gain of resending (module docs).
    /// Waits to the end of `bounds` when resending never pays.
    pub fn suspect_after(
        &self,
        prior: &CustodianPrior,
        handed: &HandedOver,
        bounds: (u64, u64),
        now: u64,
    ) -> u64 {
        const STEPS: u32 = 200;
        let (low, high) = (bounds.0.max(1), bounds.1.max(bounds.0.max(1)));
        let p = (self.delivers(prior, now).mean() * handed.downstream.clamp(0.0, 1.0)).clamp(0.0, 1.0);
        let lost = 1.0 - p;
        let remaining = handed.remaining_secs.max(1) as f64;
        let rescuing = handed.value * lost * handed.alternative.clamp(0.0, 1.0);
        let gain = |tau: u64| {
            let rescue = (1.0 - handed.expected_secs.saturating_add(tau) as f64 / remaining).max(0.0);
            let slow = 1.0 - self.delay_cdf(prior, tau, now);
            rescuing * rescue - (lost + p * slow)
        };
        let (from, to) = (log(low as f64), log(high as f64));
        let mut best = (0.0, high);
        for step in 0..=STEPS {
            let tau = (exp(from + (to - from) * f64::from(step) / f64::from(STEPS)) as u64).clamp(low, high);
            let g = gain(tau);
            if g > best.0 {
                best = (g, tau);
            }
        }
        handed.expected_secs.saturating_add(best.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;
    const WEEK: u64 = 7 * 24 * HOUR;

    fn handed(downstream: f64, alternative: f64, expected_secs: u64) -> HandedOver {
        HandedOver {
            downstream,
            alternative,
            value: 10.0,
            expected_secs,
            remaining_secs: WEEK,
        }
    }

    #[test]
    fn refusals_lower_acceptance_and_busy_blocks_until_its_time() {
        let prior = CustodianPrior::default();
        let mut c = CustodianModel::default();
        for t in 0..8 {
            c.observe(&prior, t, CustodianObservation::Refused);
        }
        assert!(c.p_accept(&prior, 10, 10) < 0.3);
        c.observe(&prior, 100, CustodianObservation::Busy { retry_after: 60 });
        assert_eq!(c.p_accept(&prior, 150, 150), 0.0);
        assert!(c.p_accept(&prior, 161, 161) > 0.0);
    }

    /// A custodian is blamed less for silence when the rest of the route was
    /// unlikely to get through anyway, and less the sooner after the planned
    /// arrival: the receipt may well be on its way.
    #[test]
    fn blame_is_shared_with_the_rest_of_the_route_and_with_time() {
        let prior = CustodianPrior::default();
        let blamed = |downstream, late_secs| {
            let mut c = CustodianModel::default();
            c.observe(
                &prior,
                0,
                CustodianObservation::Silent {
                    downstream,
                    late_secs,
                },
            );
            c.delivers(&prior, 0).mean()
        };
        assert!(blamed(0.99, 12 * HOUR) < blamed(0.2, 12 * HOUR));
        assert!(blamed(0.99, 12 * HOUR) < blamed(0.99, 10 * 60));
    }

    /// Receipts that come about two hours late teach the lateness; the
    /// suspect time lands past most of them, earlier for a custodian that
    /// often drops, and never before the route's planned arrival.
    #[test]
    fn suspect_time_follows_the_plan_the_lateness_and_the_reliability() {
        let prior = CustodianPrior::default();
        let mut good = CustodianModel::default();
        for n in 0..20 {
            good.observe(
                &prior,
                n * HOUR,
                CustodianObservation::Delivered { late_secs: 2 * HOUR },
            );
        }
        let now = 20 * HOUR;
        let half = good.delay_cdf(&prior, 2 * HOUR, now);
        assert!((half - 0.5).abs() < 0.1, "{half}");
        let bounds = (60, WEEK);
        let wait_good = good.suspect_after(&prior, &handed(0.95, 0.8, 0), bounds, now);
        // Past nearly all its receipts, well before the message expires.
        assert!(good.delay_cdf(&prior, wait_good, now) > 0.95, "{wait_good}");
        assert!(wait_good < 2 * 24 * HOUR, "{wait_good}");
        // A route that waits nine hours for its link is given nine hours more.
        let planned = good.suspect_after(&prior, &handed(0.95, 0.8, 9 * HOUR), bounds, now);
        assert!(planned >= wait_good + 8 * HOUR, "{planned} {wait_good}");
        let mut dropper = good.clone();
        for n in 0..10 {
            dropper.observe(
                &prior,
                now + n,
                CustodianObservation::Silent {
                    downstream: 0.95,
                    late_secs: 24 * HOUR,
                },
            );
        }
        let wait_dropper = dropper.suspect_after(&prior, &handed(0.95, 0.8, 0), bounds, now + 10);
        assert!(wait_dropper < wait_good, "{wait_dropper} {wait_good}");
        // Right after the handoff resending never pays: the first copy is
        // most likely on its way.
        assert!(wait_dropper > HOUR, "{wait_dropper}");
    }

    /// Reclaiming early and counting it a loss would feed on itself: silence
    /// soon after the planned arrival barely moves the belief, and the
    /// receipts that come late after all are learned from.
    #[test]
    fn early_silence_does_not_teach_that_a_slow_custodian_drops() {
        let prior = CustodianPrior::default();
        let mut c = CustodianModel::default();
        for n in 0..20 {
            let at = n * 12 * HOUR;
            c.observe(
                &prior,
                at,
                CustodianObservation::Silent {
                    downstream: 0.9,
                    late_secs: 20 * 60,
                },
            );
            c.observe(
                &prior,
                at + 6 * HOUR,
                CustodianObservation::Delivered { late_secs: 6 * HOUR },
            );
        }
        let now = 20 * 12 * HOUR;
        assert!(
            c.delivers(&prior, now).mean() > 0.6,
            "{}",
            c.delivers(&prior, now).mean()
        );
        let wait = c.suspect_after(&prior, &handed(0.9, 0.8, 0), (60, WEEK), now);
        assert!(wait > 6 * HOUR, "{wait}");
    }

    /// With no alternative worth trying, waiting to the bound is best.
    #[test]
    fn no_alternative_means_no_reclaim() {
        let prior = CustodianPrior::default();
        let c = CustodianModel::default();
        assert_eq!(
            c.suspect_after(&prior, &handed(0.9, 0.0, 0), (60, 86_400), 0),
            86_400
        );
    }
}
