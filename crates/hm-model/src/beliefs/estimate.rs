//! What route planning asks of the beliefs, answered two ways: by the
//! posterior means, or by one draw from the posterior (Thompson sampling).

use alloc::collections::BTreeMap;

use hm_core::DetRng;
use hm_wire::Callsign;

use super::{Beliefs, STATED_STRENGTH};
use crate::hearing::{first_hearing, Hearing};
use crate::link::{LinkModel, SampledLink};
use crate::LinkKey;

/// How likely a link or custodian is to do its part: what route planning
/// needs from the beliefs.
pub trait Estimate {
    /// Chance a handoff over `link` started at `t` completes; `stated` is a
    /// probability someone claimed for it (an operator's schedule, a peer's
    /// advert), weighed against this station's own evidence.
    fn link(&mut self, link: LinkKey, t: u64, stated: Option<f64>) -> f64;
    /// Chance `station` accepts custody of a handoff reaching it at `t`.
    fn accepts(&mut self, station: Callsign, t: u64) -> f64;
    /// Chance `station`, holding custody, does its part.
    fn delivers(&mut self, station: Callsign) -> f64;
    /// Expected airtime to get a frame through `link` while it is open, in
    /// frames ([`Erasure::airtime_factor`](crate::Erasure::airtime_factor)).
    fn airtime_factor(&mut self, link: LinkKey) -> f64;
    /// When the far end of radio `link` is next heard, within `window` of
    /// now, if it transmits about every `interval` seconds (the link's own
    /// interval once learned).
    fn hearing(&mut self, link: LinkKey, window: u64, interval: u64) -> Hearing;
    /// Chance a handoff over `link` completes, the path being open.
    fn handoff_if_open(&mut self, link: LinkKey) -> f64;
}

impl Beliefs {
    /// Estimates from posterior means, for deterministic planning.
    pub fn mean(&self, now: u64) -> Mean<'_> {
        Mean { beliefs: self, now }
    }

    /// Estimates drawn once per link and custodian from the beliefs, for
    /// planning by Thompson sampling: routes are chosen as if the draw were
    /// the truth, so uncertain links get tried in proportion to the chance
    /// that they are the best.
    pub fn thompson(&self, rng: DetRng, now: u64) -> Thompson<'_> {
        Thompson {
            beliefs: self,
            now,
            rng,
            links: BTreeMap::new(),
            accepts: BTreeMap::new(),
            delivers: BTreeMap::new(),
        }
    }

    /// How often the far end of `key` transmits: the link's learned
    /// interval, or `interval`.
    fn transmits_every(&self, key: LinkKey, interval: u64) -> u64 {
        self.link(key)
            .and_then(LinkModel::beacon_interval)
            .unwrap_or(interval)
    }
}

/// Weigh a stated probability against the station's own estimate `own`,
/// backed by `weight` observations.
fn with_stated(own: f64, weight: f64, stated: Option<f64>) -> f64 {
    match stated {
        None => own,
        Some(p) => (STATED_STRENGTH * p.clamp(0.0, 1.0) + weight * own) / (STATED_STRENGTH + weight),
    }
}

/// Posterior-mean estimates.
pub struct Mean<'a> {
    beliefs: &'a Beliefs,
    now: u64,
}

impl Estimate for Mean<'_> {
    fn link(&mut self, link: LinkKey, t: u64, stated: Option<f64>) -> f64 {
        let own = self.beliefs.link_success(link, t, self.now);
        with_stated(own, self.beliefs.handoff_weight(link, self.now), stated)
    }

    fn accepts(&mut self, station: Callsign, t: u64) -> f64 {
        self.beliefs.p_accept(station, t, self.now)
    }

    fn delivers(&mut self, station: Callsign) -> f64 {
        self.beliefs.p_delivers(station, self.now)
    }

    fn airtime_factor(&mut self, link: LinkKey) -> f64 {
        self.beliefs.erasure(link, self.now).airtime_factor()
    }

    fn hearing(&mut self, link: LinkKey, window: u64, interval: u64) -> Hearing {
        let (beliefs, now) = (self.beliefs, self.now);
        let through = 1.0 - beliefs.erasure(link, now).mean();
        let every = beliefs.transmits_every(link, interval);
        first_hearing(|t| beliefs.p_open(link, t, now), through, now, window, every)
    }

    fn handoff_if_open(&mut self, link: LinkKey) -> f64 {
        let prior = self.beliefs.link_prior(link.bearer);
        let own = match self.beliefs.links.get(&link.path()) {
            Some(model) => model.handoff(prior, self.now).mean(),
            None => prior.handoff.mean,
        };
        self.beliefs.calibrated(link, own)
    }
}

/// Estimates drawn from the beliefs, the same draw for every question about
/// the same link or custodian.
pub struct Thompson<'a> {
    beliefs: &'a Beliefs,
    now: u64,
    rng: DetRng,
    links: BTreeMap<LinkKey, SampledLink>,
    accepts: BTreeMap<Callsign, f64>,
    delivers: BTreeMap<Callsign, f64>,
}

impl Thompson<'_> {
    /// This plan's draw of `link`, the same for every question about it.
    fn sampled(&mut self, link: LinkKey) -> SampledLink {
        let (beliefs, now) = (self.beliefs, self.now);
        let rng = &mut self.rng;
        *self.links.entry(link.path()).or_insert_with(|| {
            let prior = beliefs.link_prior(link.bearer);
            match beliefs.links.get(&link.path()) {
                Some(model) => model.sample(prior, rng, now),
                None => beliefs.unseen(link.bearer, now).sample(prior, rng, now),
            }
        })
    }
}

impl Estimate for Thompson<'_> {
    fn link(&mut self, link: LinkKey, t: u64, stated: Option<f64>) -> f64 {
        let own = self.beliefs.calibrated(link, self.sampled(link).success(t));
        with_stated(own, self.beliefs.handoff_weight(link, self.now), stated)
    }

    fn accepts(&mut self, station: Callsign, t: u64) -> f64 {
        let (beliefs, now) = (self.beliefs, self.now);
        let busy = beliefs
            .custodians
            .get(&station)
            .is_some_and(|c| t < c.busy_until());
        if busy {
            return 0.0;
        }
        let rng = &mut self.rng;
        *self.accepts.entry(station).or_insert_with(|| {
            let prior = &beliefs.custodian_prior;
            match beliefs.custodians.get(&station) {
                Some(c) => c.accepts(prior, now).sample(rng),
                None => prior.accepts.beta().sample(rng),
            }
        })
    }

    fn delivers(&mut self, station: Callsign) -> f64 {
        let (beliefs, now) = (self.beliefs, self.now);
        let rng = &mut self.rng;
        *self.delivers.entry(station).or_insert_with(|| {
            let prior = &beliefs.custodian_prior;
            match beliefs.custodians.get(&station) {
                Some(c) => c.delivers(prior, now).sample(rng),
                None => prior.delivers.beta().sample(rng),
            }
        })
    }

    /// The expectation, not a draw: what a link costs to use is not what is
    /// explored.
    fn airtime_factor(&mut self, link: LinkKey) -> f64 {
        self.beliefs.erasure(link, self.now).airtime_factor()
    }

    fn hearing(&mut self, link: LinkKey, window: u64, interval: u64) -> Hearing {
        let sampled = self.sampled(link);
        let through = 1.0 - self.beliefs.erasure(link, self.now).mean();
        let every = self.beliefs.transmits_every(link, interval);
        first_hearing(|t| sampled.open(t), through, self.now, window, every)
    }

    fn handoff_if_open(&mut self, link: LinkKey) -> f64 {
        let own = self.sampled(link).handoff();
        self.beliefs.calibrated(link, own)
    }
}
