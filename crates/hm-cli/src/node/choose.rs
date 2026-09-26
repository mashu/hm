//! Which bearer carries the next delivery to a station.
//!
//! Each (station, bearer) pair keeps a Beta belief about its delivery success
//! rate, starting from an optimistic Beta(2, 1): a new link is assumed to work
//! until shown otherwise. For every delivery the node draws a success rate from each available
//! bearer's belief and picks the lowest expected cost, `cost / rate` (discounted
//! Thompson sampling). Evidence fades with a half-life, so:
//!
//! - the cheapest bearer (radio by default) carries traffic while it works;
//! - when it keeps failing, its expected cost rises until another bearer wins;
//! - as the failures fade into the past it gets tried again, and takes the
//!   traffic back once it succeeds.
//!
//! No bearer is special-cased: preference comes from the costs alone.

use std::collections::BTreeMap;

use hm_core::DetRng;
use hm_wire::Callsign;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bearer {
    Radio,
    Internet,
}

impl Bearer {
    pub fn name(self) -> &'static str {
        match self {
            Bearer::Radio => "radio",
            Bearer::Internet => "internet",
        }
    }
}

/// Relative cost of one delivery attempt on each bearer.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Costs {
    pub radio: f64,
    pub internet: f64,
}

impl Default for Costs {
    fn default() -> Self {
        Costs {
            radio: 1.0,
            internet: 2.0,
        }
    }
}

/// Optimistic prior: as if one success had already been seen.
const PRIOR_SUCCESS: f64 = 2.0;
const PRIOR_FAILURE: f64 = 1.0;

#[derive(Copy, Clone, Debug, Default)]
struct Arm {
    successes: f64,
    failures: f64,
    at: u64,
}

pub struct Chooser {
    costs: Costs,
    half_life_secs: f64,
    arms: BTreeMap<(Callsign, Bearer), Arm>,
    rng: DetRng,
}

/// Standard normal by Box–Muller.
fn normal(rng: &mut DetRng) -> f64 {
    let u1 = rng.next_f64().max(f64::MIN_POSITIVE);
    let u2 = rng.next_f64();
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// Gamma(shape, 1) for shape >= 1 (Marsaglia and Tsang, 2000).
fn gamma(rng: &mut DetRng, shape: f64) -> f64 {
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    loop {
        let x = normal(rng);
        let v = (1.0 + c * x).powi(3);
        if v <= 0.0 {
            continue;
        }
        let u = rng.next_f64().max(f64::MIN_POSITIVE);
        if u.ln() < 0.5 * x * x + d - d * v + d * v.ln() {
            return d * v;
        }
    }
}

/// Beta(a, b) for a, b >= 1.
fn beta(rng: &mut DetRng, a: f64, b: f64) -> f64 {
    let x = gamma(rng, a);
    let y = gamma(rng, b);
    x / (x + y)
}

impl Chooser {
    pub fn new(costs: Costs, half_life_secs: u64, rng: DetRng) -> Chooser {
        Chooser {
            costs,
            half_life_secs: half_life_secs.max(1) as f64,
            arms: BTreeMap::new(),
            rng,
        }
    }

    fn cost(&self, b: Bearer) -> f64 {
        match b {
            Bearer::Radio => self.costs.radio,
            Bearer::Internet => self.costs.internet,
        }
    }

    /// Evidence for `(peer, bearer)` faded to time `now` (Unix seconds).
    fn faded(&self, peer: Callsign, b: Bearer, now: u64) -> (f64, f64) {
        match self.arms.get(&(peer, b)) {
            None => (0.0, 0.0),
            Some(a) => {
                let f = 0.5f64.powf(now.saturating_sub(a.at) as f64 / self.half_life_secs);
                (a.successes * f, a.failures * f)
            }
        }
    }

    /// Pick one of `available` for a delivery to `peer`; `None` if none is available.
    pub fn choose(&mut self, peer: Callsign, available: &[Bearer], now: u64) -> Option<Bearer> {
        let mut best: Option<(f64, Bearer)> = None;
        for &b in available {
            let (s, f) = self.faded(peer, b, now);
            let rate = beta(&mut self.rng, PRIOR_SUCCESS + s, PRIOR_FAILURE + f).max(1e-3);
            let expected_cost = self.cost(b) / rate;
            if best.is_none_or(|(c, _)| expected_cost < c) {
                best = Some((expected_cost, b));
            }
        }
        best.map(|(_, b)| b)
    }

    pub fn record(&mut self, peer: Callsign, b: Bearer, success: bool, now: u64) {
        let (s, f) = self.faded(peer, b, now);
        let arm = Arm {
            successes: s + success as u8 as f64,
            failures: f + (!success) as u8 as f64,
            at: now,
        };
        self.arms.insert((peer, b), arm);
    }

    /// Posterior mean success rate, for display.
    pub fn estimate(&self, peer: Callsign, b: Bearer, now: u64) -> f64 {
        let (s, f) = self.faded(peer, b, now);
        (PRIOR_SUCCESS + s) / (PRIOR_SUCCESS + PRIOR_FAILURE + s + f)
    }

    /// Stations with any evidence, for display.
    pub fn peers(&self) -> Vec<Callsign> {
        let mut v: Vec<Callsign> = self.arms.keys().map(|(c, _)| *c).collect();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: [Bearer; 2] = [Bearer::Radio, Bearer::Internet];

    fn share(c: &mut Chooser, peer: Callsign, now: u64, bearer: Bearer) -> f64 {
        let n = 2000;
        (0..n)
            .filter(|_| c.choose(peer, &BOTH, now) == Some(bearer))
            .count() as f64
            / n as f64
    }

    #[test]
    fn beta_samples_have_the_right_mean() {
        let mut rng = DetRng::from_seed(3);
        for (a, b) in [(1.0, 1.0), (2.0, 5.0), (30.0, 3.0)] {
            let n = 20_000;
            let mean = (0..n).map(|_| beta(&mut rng, a, b)).sum::<f64>() / n as f64;
            assert!((mean - a / (a + b)).abs() < 0.01, "Beta({a},{b}) mean {mean}");
        }
    }

    #[test]
    fn radio_carries_traffic_while_it_works() {
        let peer = Callsign::parse("SO5KM").unwrap();
        let mut c = Chooser::new(Costs::default(), 3600, DetRng::from_seed(1));
        for t in 0..30 {
            c.record(peer, Bearer::Radio, true, t);
            c.record(peer, Bearer::Internet, true, t);
        }
        assert!(share(&mut c, peer, 30, Bearer::Radio) > 0.97);
    }

    #[test]
    fn failing_radio_hands_over_and_takes_back_after_recovery() {
        let peer = Callsign::parse("SO5KM-1").unwrap();
        let mut c = Chooser::new(Costs::default(), 3600, DetRng::from_seed(2));
        for t in 0..10 {
            c.record(peer, Bearer::Radio, false, t * 60);
            c.record(peer, Bearer::Internet, true, t * 60);
        }
        let now = 600;
        assert!(
            share(&mut c, peer, now, Bearer::Internet) > 0.9,
            "internet takes over"
        );
        // Hours later the failures have faded: radio gets tried again...
        let later = now + 4 * 3600;
        assert!(
            share(&mut c, peer, later, Bearer::Radio) > 0.2,
            "radio is probed again"
        );
        // ...and once it works, it takes the traffic back.
        for k in 0..5 {
            c.record(peer, Bearer::Radio, true, later + k);
        }
        assert!(share(&mut c, peer, later + 5, Bearer::Radio) > 0.8);
    }

    #[test]
    fn with_no_evidence_radio_is_preferred() {
        let peer = Callsign::parse("SO5KM").unwrap();
        let mut c = Chooser::new(Costs::default(), 3600, DetRng::from_seed(6));
        // Analytically P(radio) = 1 - E[p^2]/4 = 0.875 for Beta(2, 1) priors and costs 1 : 2.
        let r = share(&mut c, peer, 0, Bearer::Radio);
        assert!((r - 0.875).abs() < 0.03, "{r}");
    }

    #[test]
    fn only_available_bearers_are_chosen() {
        let peer = Callsign::parse("SO5KM").unwrap();
        let mut c = Chooser::new(Costs::default(), 3600, DetRng::from_seed(4));
        assert_eq!(c.choose(peer, &[Bearer::Internet], 0), Some(Bearer::Internet));
        assert_eq!(c.choose(peer, &[], 0), None);
    }

    #[test]
    fn costs_set_the_preference() {
        let peer = Callsign::parse("SO5KM").unwrap();
        let mut c = Chooser::new(
            Costs {
                radio: 5.0,
                internet: 1.0,
            },
            3600,
            DetRng::from_seed(5),
        );
        for t in 0..30 {
            c.record(peer, Bearer::Radio, true, t);
            c.record(peer, Bearer::Internet, true, t);
        }
        assert!(share(&mut c, peer, 30, Bearer::Internet) > 0.97);
    }
}
