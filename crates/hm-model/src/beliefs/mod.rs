//! Everything a station believes about links and custodians, in one place.
//!
//! Each model's prior is the population's: what this station has learned
//! about all links of the same bearer (all custodians), shrunk toward a weak
//! hyperprior. A link never seen before is expected to behave like the links
//! of its kind (empirical Bayes), not like a number written into the code.
//! The handoff chances given out are checked against how handoffs end, and
//! recalibrated ([`Calibration`]).

mod estimate;
mod records;
#[cfg(test)]
mod tests;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use hm_wire::Callsign;

use crate::calibration::{Calibration, Forecast};
use crate::custodian::{CustodianModel, CustodianObservation, CustodianPrior, HandedOver};
use crate::erasure::Erasure;
use crate::evidence::Prior;
use crate::link::{LinkModel, LinkObservation, LinkPrior};
use crate::openness::Openness;
use crate::{Bearer, LinkKey};

pub use estimate::{Estimate, Mean, Thompson};
pub use records::RestoreError;

const DAY: u64 = 86_400;
/// Links (custodians) the hyperprior is worth when pooling a population.
const HYPER_WEIGHT: f64 = 3.0;
/// Frame or handoff observations the hyperprior is worth when pooling.
const HYPER_EVIDENCE: f64 = 20.0;
/// A model not observed for this long has returned to its prior: forget it.
pub const FORGET_AFTER: u64 = 120 * DAY;
/// Frames due on a link stop counting as missed this long after it was last
/// open: a station gone for weeks tells nothing more by staying silent.
pub const MISS_HORIZON: u64 = 14 * DAY;
/// Pseudo-observations a stated probability (an operator's schedule, a
/// peer's advert) is worth against this station's own evidence.
pub const STATED_STRENGTH: f64 = 2.0;

/// What the station's beliefs are about.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Subject {
    Link(LinkKey),
    Custodian(Callsign),
    /// A bearer's calibration, for paths seen open (`true`) or not.
    Calibration(Bearer, bool),
}

pub struct Beliefs {
    /// By [path](LinkKey::path).
    links: BTreeMap<LinkKey, LinkModel>,
    custodians: BTreeMap<Callsign, CustodianModel>,
    link_priors: [LinkPrior; 3],
    custodian_prior: CustodianPrior,
    /// By bearer, then by whether the path was seen open: how the handoff
    /// chances given map to those that come true.
    calibration: [[Calibration; 2]; 3],
    changed: BTreeSet<Subject>,
    removed: BTreeSet<Subject>,
}

impl Default for Beliefs {
    fn default() -> Self {
        Beliefs::new()
    }
}

impl Beliefs {
    pub fn new() -> Beliefs {
        Beliefs {
            links: BTreeMap::new(),
            custodians: BTreeMap::new(),
            link_priors: Bearer::ALL.map(LinkPrior::for_bearer),
            custodian_prior: CustodianPrior::default(),
            calibration: Default::default(),
            changed: BTreeSet::new(),
            removed: BTreeSet::new(),
        }
    }

    /// The population prior for a new link of `bearer`.
    pub fn link_prior(&self, bearer: Bearer) -> &LinkPrior {
        &self.link_priors[bearer.index()]
    }

    pub fn custodian_prior(&self) -> &CustodianPrior {
        &self.custodian_prior
    }

    /// What is believed about the path `key` runs over.
    pub fn link(&self, key: LinkKey) -> Option<&LinkModel> {
        self.links.get(&key.path())
    }

    /// Every path believed in.
    pub fn links(&self) -> impl Iterator<Item = (&LinkKey, &LinkModel)> {
        self.links.iter()
    }

    pub fn custodian(&self, station: Callsign) -> Option<&CustodianModel> {
        self.custodians.get(&station)
    }

    pub fn observe_link(&mut self, key: LinkKey, at: u64, observation: LinkObservation) {
        let key = key.path();
        let prior = self.link_priors[key.bearer.index()];
        self.links
            .entry(key)
            .or_insert_with(|| LinkModel::new(&prior, at))
            .observe(&prior, at, observation);
        self.changed.insert(Subject::Link(key));
        self.refit_link_prior(key.bearer, at);
    }

    /// Account for beacons due over `key` that have not arrived by `now`,
    /// while the link was last open within [`MISS_HORIZON`]: every
    /// `interval` seconds until the link's own interval is learned. Returns
    /// how many were missed.
    pub fn note_silence(&mut self, key: LinkKey, now: u64, interval: u64) -> usize {
        let key = key.path();
        let Some(link) = self.links.get(&key) else {
            return 0;
        };
        let Some(last_open) = link.last_open() else {
            return 0;
        };
        let horizon = last_open.saturating_add(MISS_HORIZON).min(now);
        let due: Vec<u64> = link.due_misses(horizon, interval).collect();
        for &t in &due {
            self.observe_link(key, t, LinkObservation::Missed);
        }
        due.len()
    }

    pub fn observe_custodian(&mut self, station: Callsign, at: u64, observation: CustodianObservation) {
        let prior = self.custodian_prior;
        self.custodians
            .entry(station)
            .or_default()
            .observe(&prior, at, observation);
        self.changed.insert(Subject::Custodian(station));
        self.refit_custodian_prior(at);
    }

    /// The model of a link never observed: its bearer's population prior.
    fn unseen(&self, bearer: Bearer, now: u64) -> LinkModel {
        LinkModel::new(self.link_prior(bearer), now)
    }

    /// Chance the link is open at `t`.
    pub fn p_open(&self, key: LinkKey, t: u64, now: u64) -> f64 {
        match self.links.get(&key.path()) {
            Some(link) => link.p_open(t, now),
            None => self.unseen(key.bearer, now).p_open(t, now),
        }
    }

    /// Chance a handoff over the link completes, started at `t`.
    pub fn link_success(&self, key: LinkKey, t: u64, now: u64) -> f64 {
        self.calibrated(key, self.model_success(key, t, now))
    }

    /// The link model's own chance, before calibration.
    fn model_success(&self, key: LinkKey, t: u64, now: u64) -> f64 {
        let prior = self.link_prior(key.bearer);
        match self.links.get(&key.path()) {
            Some(link) => link.success(prior, t, now),
            None => self.unseen(key.bearer, now).success(prior, t, now),
        }
    }

    /// Whether the path `key` runs over has been seen open.
    fn seen(&self, key: LinkKey) -> bool {
        self.links
            .get(&key.path())
            .is_some_and(|link| link.last_open().is_some())
    }

    /// A handoff chance given for `key`, as it comes true.
    fn calibrated(&self, key: LinkKey, p: f64) -> f64 {
        self.calibration[key.bearer.index()][usize::from(self.seen(key))].apply(p)
    }

    /// The chance for a handoff over `key` starting now, to check against how
    /// it ends ([`Beliefs::observe_forecast`]).
    pub fn forecast(&self, key: LinkKey, now: u64) -> Forecast {
        Forecast {
            bearer: key.bearer,
            seen: self.seen(key),
            chance: self.model_success(key, now, now),
        }
    }

    /// A handoff started with `forecast` ended at `at`: `carried` when the
    /// link carried it, whatever the custodian said (a refusal is an answer).
    pub fn observe_forecast(&mut self, forecast: Forecast, carried: bool, at: u64) {
        self.calibration[forecast.bearer.index()][usize::from(forecast.seen)].observe(
            forecast.chance,
            carried,
            at,
        );
        self.changed
            .insert(Subject::Calibration(forecast.bearer, forecast.seen));
    }

    /// Weight of this station's own handoff evidence on the link.
    fn handoff_weight(&self, key: LinkKey, now: u64) -> f64 {
        self.links.get(&key.path()).map_or(0.0, |link| {
            let (ok, failed) = link.handoff_counts(now);
            ok + failed
        })
    }

    /// Whether the link is open now, to follow through a transfer.
    pub fn openness(&self, key: LinkKey, now: u64) -> Openness {
        match self.links.get(&key.path()) {
            Some(link) => link.openness(now),
            None => self.unseen(key.bearer, now).openness(now),
        }
    }

    /// Frame-loss belief for the link, to size overs with.
    pub fn erasure(&self, key: LinkKey, now: u64) -> Erasure {
        let prior = self.link_prior(key.bearer);
        match self.links.get(&key.path()) {
            Some(link) => link.erasure(prior, now),
            None => Erasure::from_prior(prior.erasure, prior.dispersion),
        }
    }

    /// Chance `station` accepts custody of a handoff reaching it at `t`.
    pub fn p_accept(&self, station: Callsign, t: u64, now: u64) -> f64 {
        match self.custodians.get(&station) {
            Some(c) => c.p_accept(&self.custodian_prior, t, now),
            None => self.custodian_prior.accepts.mean,
        }
    }

    /// Chance `station`, holding custody, does its part.
    pub fn p_delivers(&self, station: Callsign, now: u64) -> f64 {
        match self.custodians.get(&station) {
            Some(c) => c.delivers(&self.custodian_prior, now).mean(),
            None => self.custodian_prior.delivers.mean,
        }
    }

    /// How long to wait for an end-to-end receipt from a message handed to
    /// `custodian` before reclaiming it (see [`CustodianModel::suspect_after`]).
    pub fn suspect_after(
        &self,
        custodian: Callsign,
        handed: &HandedOver,
        bounds: (u64, u64),
        now: u64,
    ) -> u64 {
        let default = CustodianModel::default();
        self.custodians.get(&custodian).unwrap_or(&default).suspect_after(
            &self.custodian_prior,
            handed,
            bounds,
            now,
        )
    }

    /// Forget models that have not been observed for [`FORGET_AFTER`].
    pub fn prune(&mut self, now: u64) {
        let stale = |at: u64| at.saturating_add(FORGET_AFTER) < now;
        let links: Vec<LinkKey> = self
            .links
            .iter()
            .filter(|(_, l)| stale(l.observed_at()))
            .map(|(k, _)| *k)
            .collect();
        for key in links {
            self.links.remove(&key);
            self.changed.remove(&Subject::Link(key));
            self.removed.insert(Subject::Link(key));
        }
        let custodians: Vec<Callsign> = self
            .custodians
            .iter()
            .filter(|(_, c)| stale(c.observed_at()))
            .map(|(k, _)| *k)
            .collect();
        for station in custodians {
            self.custodians.remove(&station);
            self.changed.remove(&Subject::Custodian(station));
            self.removed.insert(Subject::Custodian(station));
        }
    }

    /// Re-estimate the population prior of `bearer`'s links: how many are
    /// within reach, how often those are open, averaged over links (each
    /// counted by its chance of being within reach), and the pooled handoff
    /// and frame-loss rates, each shrunk toward the hyperprior.
    fn refit_link_prior(&mut self, bearer: Bearer, now: u64) {
        let hyper = LinkPrior::for_bearer(bearer);
        let (mut links, mut in_reach, mut open) = (0.0, 0.0, 0.0);
        let (mut ok, mut failed, mut lost, mut got) = (0.0, 0.0, 0.0, 0.0);
        for (_, link) in self.links.iter().filter(|(k, _)| k.bearer == bearer) {
            let reach = link.reachable();
            links += 1.0;
            in_reach += reach;
            open += reach * daily_open(link, now);
            let (y, n) = link.handoff_counts(now);
            ok += y;
            failed += n;
            let (y, n) = link.frame_counts(now);
            lost += y;
            got += n;
        }
        let pooled = |yes: f64, total: f64, hyper: Prior| {
            Prior::new(
                (yes + HYPER_EVIDENCE * hyper.mean) / (total + HYPER_EVIDENCE),
                hyper.strength,
            )
        };
        self.link_priors[bearer.index()] = LinkPrior {
            reachable: (in_reach + HYPER_WEIGHT * hyper.reachable) / (links + HYPER_WEIGHT),
            p_open: (open + HYPER_WEIGHT * hyper.p_open) / (in_reach + HYPER_WEIGHT),
            handoff: pooled(ok, ok + failed, hyper.handoff),
            erasure: pooled(lost, lost + got, hyper.erasure),
            ..hyper
        };
    }

    fn refit_custodian_prior(&mut self, now: u64) {
        let hyper = CustodianPrior::default();
        let (mut accepted, mut offered, mut delivered, mut handed) = (0.0, 0.0, 0.0, 0.0);
        for c in self.custodians.values() {
            let (y, n) = c.accept_counts(now);
            accepted += y;
            offered += y + n;
            let (y, n) = c.deliver_counts(now);
            delivered += y;
            handed += y + n;
        }
        let pooled = |yes: f64, total: f64, hyper: Prior| {
            Prior::new(
                (yes + HYPER_EVIDENCE * hyper.mean) / (total + HYPER_EVIDENCE),
                hyper.strength,
            )
        };
        self.custodian_prior = CustodianPrior {
            accepts: pooled(accepted, offered, hyper.accepts),
            delivers: pooled(delivered, handed, hyper.delivers),
            ..hyper
        };
    }
}

/// The link's chance of being open, averaged over the day.
fn daily_open(link: &LinkModel, now: u64) -> f64 {
    let base = now - now % DAY;
    (0..24)
        .map(|h| link.availability().p_open(base + h * 3_600, now))
        .sum::<f64>()
        / 24.0
}
