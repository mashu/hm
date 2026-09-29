//! When in the day a link is open: dynamic Bayesian logistic regression.
//!
//! Generative story: HF propagation follows the sun, so whether a path is open
//! at time `t` depends smoothly on the UTC hour `h(t)`:
//!
//! ```text
//! P(open at t) = σ(w · φ(h(t)))
//! φ(h) = [1, sin 2πh/24, cos 2πh/24, sin 4πh/24, cos 4πh/24]
//! w_{t+Δ} = w_t + noise,  noise ~ N(0, Q Δ)
//! ```
//!
//! The weights `w` are a state that drifts (seasons, the solar cycle), which
//! makes this a state-space model: a Gaussian belief `N(m, S)` over `w` is
//! widened by `QΔ` between observations (predict) and sharpened by each one
//! (update). The logistic likelihood is not conjugate, so each update is one
//! Newton step from the prior mean (the Laplace approximation used for online
//! Bayesian logistic regression): with `z = w·x`, gradient `g` and curvature
//! `h` of the observation's log-likelihood at `z0 = m·x`,
//!
//! ```text
//! S' = S − h S x xᵀ S / (1 + h xᵀ S x)
//! m' = m + g S' x
//! ```
//!
//! Two harmonics let the model learn one or two openings a day (a gray-line
//! path, a daytime band) and share strength between neighbouring hours, where
//! independent hourly buckets would not. Links that do not follow the sun
//! (internet) use the intercept alone.

use hm_core::DetRng;
use libm::{cos, sin, sqrt};
use minicbor::{Decode, Encode};

use crate::math::{self, sigmoid};

/// Number of weights: an intercept and two harmonics of the day.
pub const WEIGHTS: usize = 5;

const DAY_SECS: f64 = 86_400.0;
/// Prior variance of the intercept (log-odds): a new link could be open most
/// of the day or hardly ever.
const INTERCEPT_VARIANCE: f64 = 2.0;
/// Prior variance of each harmonic weight: an amplitude of about 1 in log-odds.
const HARMONIC_VARIANCE: f64 = 1.0;
/// Process noise per day, intercept then harmonics: after a month without
/// news the intercept's standard deviation has grown by about one.
const DRIFT_PER_DAY: [f64; WEIGHTS] = [0.03, 0.02, 0.02, 0.02, 0.02];

/// What an observation says about whether the link was open.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Likelihood {
    /// It was open: a frame came through, or a session was up.
    Open,
    /// It was closed: a session was down.
    Closed,
    /// Something that gets through an open link with probability `detect`
    /// did not get through: closed, or open and lost.
    Missed { detect: f64 },
}

impl Likelihood {
    /// Gradient and curvature (negated second derivative, at least 0) of the
    /// log-likelihood in the log-odds `z`, where `u = σ(z)`.
    fn derivatives(self, u: f64) -> (f64, f64) {
        let v = u * (1.0 - u);
        match self {
            Likelihood::Open => (1.0 - u, v),
            Likelihood::Closed => (-u, v),
            Likelihood::Missed { detect } => {
                let c = detect.clamp(0.0, 1.0 - 1.0e-9);
                let rest = 1.0 - c * u;
                let g = -c * v / rest;
                let h = c * v * (1.0 - 2.0 * u + c * u * u) / (rest * rest);
                (g, h.max(0.0))
            }
        }
    }
}

/// Belief about a link's daily pattern of openings.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Diurnal {
    #[n(0)]
    mean: [f64; WEIGHTS],
    /// Covariance, row by row.
    #[n(1)]
    cov: [f64; WEIGHTS * WEIGHTS],
    /// Time the covariance was last drifted to.
    #[n(2)]
    at: u64,
    /// Whether the link follows the sun; otherwise only the intercept is used.
    #[n(3)]
    daily: bool,
}

/// The features of time `t` (Unix seconds).
fn features(t: u64, daily: bool) -> [f64; WEIGHTS] {
    if !daily {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    }
    let angle = core::f64::consts::TAU * (t as f64 % DAY_SECS) / DAY_SECS;
    [1.0, sin(angle), cos(angle), sin(2.0 * angle), cos(2.0 * angle)]
}

fn dot(a: &[f64; WEIGHTS], b: &[f64; WEIGHTS]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn times(cov: &[f64; WEIGHTS * WEIGHTS], x: &[f64; WEIGHTS]) -> [f64; WEIGHTS] {
    let mut out = [0.0; WEIGHTS];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (0..WEIGHTS).map(|j| cov[i * WEIGHTS + j] * x[j]).sum();
    }
    out
}

impl Diurnal {
    /// A new link believed open with probability about `p_open`, at no
    /// particular time of day.
    pub fn new(p_open: f64, daily: bool, now: u64) -> Diurnal {
        let mut mean = [0.0; WEIGHTS];
        mean[0] = math::logit(p_open);
        let mut cov = [0.0; WEIGHTS * WEIGHTS];
        cov[0] = INTERCEPT_VARIANCE;
        if daily {
            for i in 1..WEIGHTS {
                cov[i * WEIGHTS + i] = HARMONIC_VARIANCE;
            }
        }
        Diurnal {
            mean,
            cov,
            at: now,
            daily,
        }
    }

    pub fn daily(&self) -> bool {
        self.daily
    }

    /// Covariance widened by the drift since the last update, to `now`.
    fn cov_at(&self, now: u64) -> [f64; WEIGHTS * WEIGHTS] {
        let mut cov = self.cov;
        let days = now.saturating_sub(self.at) as f64 / DAY_SECS;
        if days > 0.0 {
            for (i, q) in DRIFT_PER_DAY.iter().enumerate() {
                if i == 0 || self.daily {
                    cov[i * WEIGHTS + i] += q * days;
                }
            }
        }
        cov
    }

    /// Mean and variance of the log-odds of the link being open at `t`, as
    /// believed at `now`.
    pub fn log_odds(&self, t: u64, now: u64) -> (f64, f64) {
        let x = features(t, self.daily);
        let cov = self.cov_at(now.max(self.at));
        (dot(&self.mean, &x), dot(&x, &times(&cov, &x)).max(0.0))
    }

    /// Probability that the link is open at `t`, integrating over what is not
    /// known about the weights (probit approximation of the logistic-normal).
    pub fn p_open(&self, t: u64, now: u64) -> f64 {
        let (mu, var) = self.log_odds(t, now);
        sigmoid(mu / sqrt(1.0 + core::f64::consts::PI * var / 8.0))
    }

    /// Take one observation made at `t`, counted with `weight` (1 for an
    /// independent observation; less when it repeats one made moments before).
    pub fn update(&mut self, t: u64, likelihood: Likelihood, weight: f64) {
        if weight.is_nan() || weight <= 0.0 {
            return;
        }
        if t > self.at {
            self.cov = self.cov_at(t);
            self.at = t;
        }
        let x = features(t, self.daily);
        let (g, h) = likelihood.derivatives(sigmoid(dot(&self.mean, &x)));
        let (g, h) = (g * weight, h * weight);
        let sx = times(&self.cov, &x);
        let s = dot(&x, &sx);
        let k = h / (1.0 + h * s);
        for i in 0..WEIGHTS {
            for j in 0..WEIGHTS {
                self.cov[i * WEIGHTS + j] -= k * sx[i] * sx[j];
            }
        }
        let step = times(&self.cov, &x);
        for (m, d) in self.mean.iter_mut().zip(step) {
            *m += g * d;
        }
    }

    /// One draw of the weights from the belief at `now` (Thompson sampling).
    pub fn sample(&self, rng: &mut DetRng, now: u64) -> SampledDiurnal {
        let cov = self.cov_at(now.max(self.at));
        let l = cholesky(&cov);
        let z: [f64; WEIGHTS] = core::array::from_fn(|_| math::normal(rng));
        let mut w = self.mean;
        for (i, wi) in w.iter_mut().enumerate() {
            *wi += (0..=i).map(|j| l[i * WEIGHTS + j] * z[j]).sum::<f64>();
        }
        SampledDiurnal {
            weights: w,
            daily: self.daily,
        }
    }
}

/// One plausible daily pattern, drawn from a [`Diurnal`] belief.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SampledDiurnal {
    weights: [f64; WEIGHTS],
    daily: bool,
}

impl SampledDiurnal {
    pub fn p_open(&self, t: u64) -> f64 {
        sigmoid(dot(&self.weights, &features(t, self.daily)))
    }
}

/// Lower-triangular `L` with `L Lᵀ = cov`; rows and columns that are not
/// positive (unused harmonics) are left zero.
fn cholesky(cov: &[f64; WEIGHTS * WEIGHTS]) -> [f64; WEIGHTS * WEIGHTS] {
    let mut l = [0.0; WEIGHTS * WEIGHTS];
    for i in 0..WEIGHTS {
        for j in 0..=i {
            let sum: f64 = (0..j).map(|k| l[i * WEIGHTS + k] * l[j * WEIGHTS + k]).sum();
            if i == j {
                let d = cov[i * WEIGHTS + i] - sum;
                l[i * WEIGHTS + i] = if d > 1.0e-12 { sqrt(d) } else { 0.0 };
            } else if l[j * WEIGHTS + j] > 0.0 {
                l[i * WEIGHTS + j] = (cov[i * WEIGHTS + j] - sum) / l[j * WEIGHTS + j];
            }
        }
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;
    const DAY: u64 = 24 * HOUR;

    /// A path open from 12 to 18 UTC every day, heard every 15 minutes while
    /// open and missed otherwise, is learned within days.
    #[test]
    fn learns_an_afternoon_opening() {
        let mut d = Diurnal::new(0.5, true, 0);
        for day in 0..5 {
            for q in 0..96 {
                let t = day * DAY + q * 900;
                let hour = (t % DAY) / HOUR;
                let open = (12..18).contains(&hour);
                let lik = if open {
                    Likelihood::Open
                } else {
                    Likelihood::Missed { detect: 0.8 }
                };
                // Observations 15 minutes apart on an hour-long process count
                // a quarter each.
                d.update(t, lik, 0.25);
            }
        }
        let now = 5 * DAY;
        let afternoon = d.p_open(now + 15 * HOUR, now);
        let night = d.p_open(now + 3 * HOUR, now);
        assert!(afternoon > 0.8, "afternoon {afternoon:.2}");
        assert!(night < 0.2, "night {night:.2}");
    }

    /// Without news the belief widens back toward uncertainty.
    #[test]
    fn belief_drifts_without_observations() {
        let mut d = Diurnal::new(0.5, true, 0);
        for n in 0..200 {
            d.update(n * 600, Likelihood::Open, 1.0);
        }
        let now = 200 * 600;
        let (_, var_now) = d.log_odds(now, now);
        let (_, var_later) = d.log_odds(now, now + 60 * DAY);
        assert!(var_later > var_now + 1.0, "{var_now} -> {var_later}");
        assert!(d.p_open(now, now) > d.p_open(now, now + 60 * DAY));
    }

    #[test]
    fn a_missed_frame_says_less_than_a_closed_link() {
        let mut closed = Diurnal::new(0.5, false, 0);
        let mut missed = closed.clone();
        closed.update(10, Likelihood::Closed, 1.0);
        missed.update(10, Likelihood::Missed { detect: 0.5 }, 1.0);
        let (p_closed, p_missed) = (closed.p_open(10, 10), missed.p_open(10, 10));
        assert!(p_closed < p_missed && p_missed < 0.5, "{p_closed} {p_missed}");
    }

    #[test]
    fn samples_scatter_around_the_mean() {
        let mut d = Diurnal::new(0.3, true, 0);
        for n in 0..20 {
            d.update(n * HOUR, Likelihood::Open, 0.5);
        }
        let mut rng = DetRng::from_seed(3);
        let t = 20 * HOUR;
        let draws: Vec<f64> = (0..4_000).map(|_| d.sample(&mut rng, t).p_open(t)).collect();
        let mean = draws.iter().sum::<f64>() / draws.len() as f64;
        assert!(
            (mean - d.p_open(t, t)).abs() < 0.03,
            "{mean} vs {}",
            d.p_open(t, t)
        );
        assert!(draws.iter().any(|p| (p - mean).abs() > 0.05));
    }

    #[test]
    fn a_link_that_does_not_follow_the_sun_is_the_same_at_every_hour() {
        let mut d = Diurnal::new(0.5, false, 0);
        d.update(14 * HOUR, Likelihood::Open, 1.0);
        assert!((d.p_open(3 * HOUR, DAY) - d.p_open(15 * HOUR, DAY)).abs() < 1e-12);
    }
}
