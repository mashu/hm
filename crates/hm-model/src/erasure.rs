//! Frame loss on an open link, and how many frames an over should carry.
//!
//! Generative story: while a link is open each frame is lost with a rate `ε`
//! that the link keeps for days (its path loss and noise). Fading makes
//! losses come in bursts, so each over draws its own rate around `ε`:
//!
//! ```text
//! ε ~ Beta(lost, got)                           belief about the link
//! r_over ~ Beta(mean ε, dispersion ρ)            this over's rate (a fade or not)
//! frames lost in an over of n ~ Binomial(n, r_over)
//! ```
//!
//! `ρ` is the intra-over correlation of losses (0: independent frames, as on a
//! quiet VHF path; near 1: an over is all-or-nothing, as in deep slow fades).
//! Marginally, the frames that arrive in an over of `n` follow a Beta-binomial
//! whose concentration combines what is not known about `ε` with the fade
//! spread `ρ`. That predictive, not a point estimate of the loss, sizes bursts.
//!
//! Burst size is a decision: each frame costs airtime; each over that falls
//! short costs a turnaround (key-ups, the ACK, the next OFFER) and another
//! over. [`burst_size`] minimises the expected airtime to finish, by dynamic
//! programming over how many symbols remain. Fountain coding makes a short
//! over cheap to top up, so the optimum is usually smaller than a burst sized
//! to succeed with high probability, and larger when turnarounds are long.

use alloc::vec::Vec;
use libm::{exp, log};
use minicbor::{Decode, Encode};

use crate::evidence::Prior;

/// Belief about frame loss on a link: Beta(`lost`, `got`) over the loss rate,
/// with fade dispersion `dispersion`.
#[derive(Copy, Clone, Debug, PartialEq, Encode, Decode)]
pub struct Erasure {
    #[n(0)]
    pub lost: f64,
    #[n(1)]
    pub got: f64,
    #[n(2)]
    pub dispersion: f64,
}

impl Erasure {
    /// A belief with mean loss `prior.mean` worth `prior.strength` frames.
    pub fn from_prior(prior: Prior, dispersion: f64) -> Erasure {
        let beta = prior.beta();
        Erasure {
            lost: beta.a,
            got: beta.b,
            dispersion: dispersion.clamp(0.0, 0.95),
        }
    }

    /// Expected share of frames lost.
    pub fn mean(&self) -> f64 {
        self.lost / (self.lost + self.got)
    }

    /// An over of `sent` frames of which `got` arrived.
    pub fn observe(&mut self, sent: u32, got: u32) {
        let got = got.min(sent);
        self.lost += f64::from(sent - got);
        self.got += f64::from(got);
    }

    /// Beta over the share of frames that arrive in the next over, as
    /// (arrive, lost) parameters.
    fn over_rate(&self) -> (f64, f64) {
        let n0 = self.lost + self.got;
        let mean_lost = self.mean().clamp(1.0e-6, 1.0 - 1.0e-6);
        let rho = self.dispersion.clamp(0.0, 0.95);
        // Var(r) / (μ(1-μ)) = ρ n0/(n0+1) + 1/(n0+1).
        let share = rho * n0 / (n0 + 1.0) + 1.0 / (n0 + 1.0);
        let concentration = (1.0 / share - 1.0).max(1.0e-3);
        (concentration * (1.0 - mean_lost), concentration * mean_lost)
    }

    /// Probability that exactly `j` of `n` frames arrive, for `j` in `0..=n`.
    /// Computed by the ratio of successive terms,
    /// `P(j+1)/P(j) = (n−j)/(j+1) · (j+a)/(n−j−1+b)`, in logs, and normalised,
    /// which stays exact where differences of large log-gammas would not.
    pub fn arrivals(&self, n: u32) -> Vec<f64> {
        let (a, b) = self.over_rate();
        let mut logs = Vec::with_capacity(n as usize + 1);
        let mut current = 0.0;
        logs.push(current);
        for j in 0..n {
            let (j, n) = (f64::from(j), f64::from(n));
            current += log((n - j) / (j + 1.0)) + log((j + a) / (n - j - 1.0 + b));
            logs.push(current);
        }
        let top = logs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let weights: Vec<f64> = logs.iter().map(|l| exp(l - top)).collect();
        let total: f64 = weights.iter().sum();
        weights.into_iter().map(|w| w / total).collect()
    }

    /// Probability that at least `k` of `n` frames arrive.
    pub fn p_at_least(&self, n: u32, k: u32) -> f64 {
        if k == 0 {
            return 1.0;
        }
        if k > n {
            return 0.0;
        }
        self.arrivals(n)[k as usize..].iter().sum::<f64>().min(1.0)
    }
}

/// What an over costs in airtime, in milliseconds.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct OverCost {
    /// One DATA frame.
    pub frame_ms: f64,
    /// Everything an over costs besides its DATA frames: key-ups, the OFFER,
    /// the receiver's ACK.
    pub turnaround_ms: f64,
}

/// Expected airtime to deliver `need` symbols, and the best first over, when
/// overs carry at most `cap` frames.
pub fn plan_overs(need: u32, cap: u32, erasure: &Erasure, cost: OverCost) -> (f64, u32) {
    let cap = cap.max(1);
    if need == 0 {
        return (0.0, 0);
    }
    let arrivals: Vec<Vec<f64>> = (0..=cap).map(|n| erasure.arrivals(n)).collect();
    let need = need as usize;
    // expected[r]: expected airtime to deliver r more symbols, overs chosen well.
    let mut expected = alloc::vec![0.0_f64; need + 1];
    let mut first = 1;
    for remaining in 1..=need {
        let mut best = (f64::INFINITY, 1_u32);
        for n in 1..=cap {
            let p = &arrivals[n as usize];
            let stay = p[0];
            if stay >= 1.0 - 1.0e-12 {
                continue;
            }
            let mut cost_n = f64::from(n) * cost.frame_ms + cost.turnaround_ms;
            for (j, pj) in p.iter().enumerate().take((n as usize + 1).min(remaining)).skip(1) {
                cost_n += pj * expected[remaining - j];
            }
            let total = cost_n / (1.0 - stay);
            if total < best.0 {
                best = (total, n);
            }
        }
        expected[remaining] = best.0;
        first = best.1;
    }
    (expected[need], first)
}

/// Frames for the next over: the first over of the plan that minimises the
/// expected airtime to deliver `need` symbols. With more symbols to go than an
/// over may carry, a full over.
pub fn burst_size(need: u32, cap: u32, erasure: &Erasure, cost: OverCost) -> u32 {
    let cap = cap.max(1);
    if need >= cap {
        return cap;
    }
    plan_overs(need, cap, erasure, cost).1.max(1)
}

/// Frames for a broadcast over to `listeners` stations that each need `need`
/// symbols. Each listener's arrivals follow the Beta-binomial predictive,
/// independently; after an over the listener worst off asks for its shortfall
/// and a repair over for that many follows, sized the same way. With `J(r)`
/// the expected airtime to finish when the worst listener lacks `r`, and `W`
/// the worst shortfall after an over of `n`,
///
/// ```text
/// P(W ≤ d) = P(X_n ≥ r − d)^listeners
/// J(r) = min_n [ n·frame + turnaround + Σ_{1≤d≤r} P(W = d) · J(d) ]
/// ```
///
/// where `d = r` (nobody got anything) leaves the same state, solved as a
/// geometric wait. Every frame of a bigger over shrinks every listener's
/// shortfall, which a broadcast sized for one listener would miss.
pub fn broadcast_burst(need: u32, cap: u32, erasure: &Erasure, cost: OverCost, listeners: u32) -> u32 {
    let cap = cap.max(1);
    if need >= cap {
        return cap;
    }
    let need = need.max(1) as usize;
    let listeners = f64::from(listeners.max(1));
    let arrivals: Vec<Vec<f64>> = (0..=cap).map(|n| erasure.arrivals(n)).collect();
    // at_least[n][k] = P(X_n >= k).
    let at_least: Vec<Vec<f64>> = arrivals
        .iter()
        .map(|p| {
            let mut tail = alloc::vec![0.0; p.len() + 1];
            for k in (0..p.len()).rev() {
                tail[k] = tail[k + 1] + p[k];
            }
            tail
        })
        .collect();
    let mut expected = alloc::vec![0.0_f64; need + 1];
    let mut first = cap;
    for r in 1..=need {
        let mut best = (f64::INFINITY, cap);
        for n in r as u32..=cap {
            let tail = &at_least[n as usize];
            // P(W <= d) for d = 0..=r.
            let worst = |d: usize| libm::pow(tail[r - d].min(1.0), listeners);
            let stay = worst(r) - worst(r - 1);
            if stay >= 1.0 - 1.0e-12 {
                continue;
            }
            let mut total = f64::from(n) * cost.frame_ms + cost.turnaround_ms;
            for (d, cost_to_go) in expected.iter().enumerate().take(r).skip(1) {
                total += (worst(d) - worst(d - 1)) * cost_to_go;
            }
            let total = total / (1.0 - stay);
            if total < best.0 {
                best = (total, n);
            }
        }
        expected[r] = best.0;
        first = best.1;
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    fn erasure(loss: f64, strength: f64, dispersion: f64) -> Erasure {
        Erasure::from_prior(Prior::new(loss, strength), dispersion)
    }

    #[test]
    fn arrivals_sum_to_one_and_match_the_binomial_when_certain() {
        let e = erasure(0.2, 1.0e7, 0.0);
        let p = e.arrivals(15);
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        // Binomial(15, 0.8): P[X >= 10] = 0.939.
        assert!(
            (e.p_at_least(15, 10) - 0.939).abs() < 2e-3,
            "{}",
            e.p_at_least(15, 10)
        );
    }

    #[test]
    fn fades_and_doubt_widen_the_spread() {
        let certain = erasure(0.2, 1.0e7, 0.0);
        let fading = erasure(0.2, 1.0e7, 0.2);
        let unsure = erasure(0.2, 4.0, 0.0);
        let at_least = |e: &Erasure| e.p_at_least(15, 10);
        assert!(at_least(&fading) < at_least(&certain));
        assert!(at_least(&unsure) < at_least(&certain));
    }

    /// Ten symbols to go at 20 % loss with HF-length frames: the best first
    /// over is shorter than one sized to succeed nine times in ten (15), more
    /// so when turnarounds are short, and it costs less airtime than 15.
    #[test]
    fn short_turnarounds_favour_short_overs() {
        let e = erasure(0.2, 1.0e7, 0.0);
        let quick = OverCost {
            frame_ms: 2_900.0,
            turnaround_ms: 1_000.0,
        };
        let slow = OverCost {
            turnaround_ms: 30_000.0,
            ..quick
        };
        let n_quick = burst_size(10, 16, &e, quick);
        let n_slow = burst_size(10, 16, &e, slow);
        assert!((11..=13).contains(&n_quick), "{n_quick}");
        assert!(n_slow > n_quick && n_slow <= 15, "{n_slow}");
        let (best, _) = plan_overs(10, 16, &e, quick);
        // The same plan, but forced to open with 15 frames.
        let rest = |r: u32| plan_overs(r, 16, &e, quick).0;
        let p = e.arrivals(15);
        let forced = (15.0 * quick.frame_ms
            + quick.turnaround_ms
            + (1..10).map(|j| p[j] * rest(10 - j as u32)).sum::<f64>())
            / (1.0 - p[0]);
        assert!(best < forced * 0.95, "{best} vs {forced}");
    }

    #[test]
    fn more_to_send_than_an_over_holds_fills_the_over() {
        let e = erasure(0.1, 10.0, 0.1);
        let cost = OverCost {
            frame_ms: 500.0,
            turnaround_ms: 2_000.0,
        };
        assert_eq!(burst_size(40, 16, &e, cost), 16);
        assert_eq!(burst_size(0, 16, &e, cost), 1);
    }

    /// More listeners, more frames: every one of them must get enough.
    #[test]
    fn broadcasts_are_sized_for_all_their_listeners() {
        let e = erasure(0.25, 1.0e6, 0.0);
        let cost = OverCost {
            frame_ms: 1_500.0,
            turnaround_ms: 8_000.0,
        };
        let one = broadcast_burst(10, 32, &e, cost, 1);
        let ten = broadcast_burst(10, 32, &e, cost, 10);
        assert!(ten > one, "{ten} vs {one}");
        assert!(e.p_at_least(ten, 10).powi(10) > 0.5);
    }

    #[test]
    fn observing_overs_moves_the_belief() {
        let mut e = erasure(0.3, 4.0, 0.0);
        for _ in 0..20 {
            e.observe(10, 10);
        }
        assert!(e.mean() < 0.01);
    }
}
