//! Everything a station believes about links and custodians, in one place.
//!
//! Each model's prior is the population's: what this station has learned
//! about all links of the same bearer (all custodians), shrunk toward a weak
//! hyperprior. A link never seen before is expected to behave like the links
//! of its kind (empirical Bayes), not like a number written into the code.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use hm_core::DetRng;
use hm_wire::Callsign;

use crate::custodian::{CustodianModel, CustodianObservation, CustodianPrior};
use crate::erasure::Erasure;
use crate::evidence::Prior;
use crate::link::{LinkModel, LinkObservation, LinkPrior, SampledLink};
use crate::{Bearer, LinkKey};

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
/// Version byte in front of every encoded record.
const RECORD_VERSION: u8 = 1;
const LINK_RECORD: u8 = 1;
const CUSTODIAN_RECORD: u8 = 2;

/// What the station's beliefs are about.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Subject {
    Link(LinkKey),
    Custodian(Callsign),
}

/// A summary of one link, for status displays.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LinkEstimate {
    pub p_open: f64,
    pub handoff: f64,
    pub loss: f64,
    pub persistence_secs: f64,
    pub last_open: Option<u64>,
}

/// A record that could not be restored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreError(pub &'static str);

impl core::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

pub struct Beliefs {
    /// By [path](LinkKey::path).
    links: BTreeMap<LinkKey, LinkModel>,
    custodians: BTreeMap<Callsign, CustodianModel>,
    link_priors: [LinkPrior; 3],
    custodian_prior: CustodianPrior,
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
        let prior = self.link_prior(key.bearer);
        match self.links.get(&key.path()) {
            Some(link) => link.success(prior, t, now),
            None => self.unseen(key.bearer, now).success(prior, t, now),
        }
    }

    /// Weight of this station's own handoff evidence on the link.
    fn handoff_weight(&self, key: LinkKey, now: u64) -> f64 {
        self.links.get(&key.path()).map_or(0.0, |link| {
            let (ok, failed) = link.handoff_counts(now);
            ok + failed
        })
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
    #[allow(clippy::too_many_arguments)]
    pub fn suspect_after(
        &self,
        custodian: Callsign,
        downstream: f64,
        alternative: f64,
        value: f64,
        remaining: u64,
        bounds: (u64, u64),
        now: u64,
    ) -> u64 {
        let default = CustodianModel::default();
        self.custodians.get(&custodian).unwrap_or(&default).suspect_after(
            &self.custodian_prior,
            downstream,
            alternative,
            value,
            remaining,
            bounds,
            now,
        )
    }

    /// Summary of every link believed in, for status.
    pub fn link_estimates(&self, now: u64) -> Vec<(LinkKey, LinkEstimate)> {
        self.links
            .iter()
            .map(|(key, link)| {
                let prior = self.link_prior(key.bearer);
                (
                    *key,
                    LinkEstimate {
                        p_open: link.p_open(now, now),
                        handoff: link.handoff(prior, now).mean(),
                        loss: link.erasure(prior, now).mean(),
                        persistence_secs: link.persistence_secs(),
                        last_open: link.last_open(),
                    },
                )
            })
            .collect()
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

    /// Records changed since the last call, to save: `(key, Some(value))` to
    /// write, `(key, None)` to delete.
    pub fn take_changed(&mut self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        for subject in core::mem::take(&mut self.changed) {
            let value = match subject {
                Subject::Link(key) => self.links.get(&key).map(encode_value),
                Subject::Custodian(station) => self.custodians.get(&station).map(encode_value),
            };
            if let Some(value) = value {
                out.push((record_key(subject), Some(value)));
            }
        }
        for subject in core::mem::take(&mut self.removed) {
            out.push((record_key(subject), None));
        }
        out
    }

    /// Take back a record saved from [`Beliefs::take_changed`].
    pub fn restore(&mut self, key: &[u8], value: &[u8]) -> Result<(), RestoreError> {
        let (&version, body) = value.split_first().ok_or(RestoreError("empty record"))?;
        if version != RECORD_VERSION {
            return Err(RestoreError("unknown record version"));
        }
        match parse_key(key)? {
            Subject::Link(link) => {
                let model: LinkModel = minicbor::decode(body).map_err(|_| RestoreError("link record"))?;
                let at = model.observed_at();
                // Saved by a version that kept the two directions apart:
                // keep the direction observed last.
                let path = link.path();
                if path != link {
                    self.removed.insert(Subject::Link(link));
                    self.changed.insert(Subject::Link(path));
                }
                if self.links.get(&path).is_none_or(|kept| kept.observed_at() <= at) {
                    self.links.insert(path, model);
                }
                self.refit_link_prior(link.bearer, at);
            }
            Subject::Custodian(station) => {
                let model: CustodianModel =
                    minicbor::decode(body).map_err(|_| RestoreError("custodian record"))?;
                let at = model.observed_at();
                self.custodians.insert(station, model);
                self.refit_custodian_prior(at);
            }
        }
        Ok(())
    }

    /// Re-estimate the population prior of `bearer`'s links: their typical
    /// chance of being open, averaged over links, and the pooled handoff and
    /// frame-loss rates, each shrunk toward the hyperprior.
    fn refit_link_prior(&mut self, bearer: Bearer, now: u64) {
        let hyper = LinkPrior::for_bearer(bearer);
        let (mut links, mut open) = (0.0, 0.0);
        let (mut ok, mut failed, mut lost, mut got) = (0.0, 0.0, 0.0, 0.0);
        for (_, link) in self.links.iter().filter(|(k, _)| k.bearer == bearer) {
            links += 1.0;
            open += daily_open(link, now);
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
            p_open: (open + HYPER_WEIGHT * hyper.p_open) / (links + HYPER_WEIGHT),
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
}

/// The link's chance of being open, averaged over the day.
fn daily_open(link: &LinkModel, now: u64) -> f64 {
    let base = now - now % DAY;
    (0..24)
        .map(|h| link.availability().p_open(base + h * 3_600, now))
        .sum::<f64>()
        / 24.0
}

fn encode_value<T: minicbor::Encode<()>>(model: &T) -> Vec<u8> {
    let mut out = alloc::vec![RECORD_VERSION];
    out.extend(minicbor::to_vec(model).expect("encoding to a vector cannot fail"));
    out
}

fn record_key(subject: Subject) -> Vec<u8> {
    match subject {
        Subject::Link(key) => {
            let mut out = alloc::vec![LINK_RECORD];
            out.extend_from_slice(&key.from.to_bytes());
            out.extend_from_slice(&key.to.to_bytes());
            out.push(key.bearer.index() as u8);
            out
        }
        Subject::Custodian(station) => {
            let mut out = alloc::vec![CUSTODIAN_RECORD];
            out.extend_from_slice(&station.to_bytes());
            out
        }
    }
}

fn parse_key(key: &[u8]) -> Result<Subject, RestoreError> {
    let call = |bytes: &[u8]| -> Result<Callsign, RestoreError> {
        let bytes: [u8; 6] = bytes.try_into().map_err(|_| RestoreError("callsign"))?;
        Callsign::from_bytes(bytes).map_err(|_| RestoreError("callsign"))
    };
    match key {
        [LINK_RECORD, rest @ ..] if rest.len() == 13 => Ok(Subject::Link(LinkKey {
            from: call(&rest[..6])?,
            to: call(&rest[6..12])?,
            bearer: Bearer::from_index(rest[12]).ok_or(RestoreError("bearer"))?,
        })),
        [CUSTODIAN_RECORD, rest @ ..] if rest.len() == 6 => Ok(Subject::Custodian(call(rest)?)),
        _ => Err(RestoreError("record key")),
    }
}

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

impl Estimate for Thompson<'_> {
    fn link(&mut self, link: LinkKey, t: u64, stated: Option<f64>) -> f64 {
        let (beliefs, now) = (self.beliefs, self.now);
        let rng = &mut self.rng;
        let sampled = *self.links.entry(link.path()).or_insert_with(|| {
            let prior = beliefs.link_prior(link.bearer);
            match beliefs.links.get(&link.path()) {
                Some(model) => model.sample(prior, rng, now),
                None => beliefs.unseen(link.bearer, now).sample(prior, rng, now),
            }
        });
        with_stated(sampled.success(t), beliefs.handoff_weight(link, now), stated)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> Callsign {
        s.parse().unwrap()
    }

    fn key(from: &str, to: &str, bearer: Bearer) -> LinkKey {
        LinkKey {
            from: call(from),
            to: call(to),
            bearer,
        }
    }

    /// Links that keep failing make a new link of the same kind look worse
    /// too: the prior is the population's.
    #[test]
    fn a_new_link_is_expected_to_behave_like_its_kind() {
        let mut beliefs = Beliefs::new();
        let before = beliefs.link_prior(Bearer::Modem).handoff.mean;
        for peer in ["M0AAA", "M0BBB", "M0CCC"] {
            for t in 0..30 {
                let k = key("M0ME", peer, Bearer::Modem);
                beliefs.observe_link(k, t * 60, LinkObservation::Heard);
                beliefs.observe_link(k, t * 60 + 1, LinkObservation::Handoff { ok: false });
            }
        }
        let after = beliefs.link_prior(Bearer::Modem).handoff.mean;
        assert!(after < before - 0.2, "{before} -> {after}");
        // Radio links learned nothing from modem failures.
        assert_eq!(
            beliefs.link_prior(Bearer::Radio).handoff,
            LinkPrior::for_bearer(Bearer::Radio).handoff
        );
    }

    /// A beacon heard from a station and a handoff to it are evidence about
    /// one path: what is learned one way holds the other.
    #[test]
    fn both_directions_of_a_path_share_one_belief() {
        let mut beliefs = Beliefs::new();
        let (out, back) = (
            key("M0ME", "M0AAA", Bearer::Radio),
            key("M0AAA", "M0ME", Bearer::Radio),
        );
        let unseen = beliefs.p_open(out, 0, 0);
        for t in 0..6 {
            beliefs.observe_link(back, t * 600, LinkObservation::Missed);
        }
        let now = 3_000;
        assert!(beliefs.p_open(out, now, now) < unseen / 2.0);
        assert_eq!(beliefs.p_open(out, now, now), beliefs.p_open(back, now, now));
        assert_eq!(beliefs.link(out), beliefs.link(back));
        assert_eq!(beliefs.links().count(), 1);
        // A record saved per direction comes back as the path's.
        let mut restored = Beliefs::new();
        for (k, v) in beliefs.take_changed() {
            restored.restore(&k, &v.unwrap()).unwrap();
        }
        assert_eq!(restored.link(back), beliefs.link(out));
    }

    #[test]
    fn records_round_trip_and_deletions_are_reported() {
        let mut beliefs = Beliefs::new();
        let k = key("M0ME", "M0AAA", Bearer::Radio);
        beliefs.observe_link(k, 100, LinkObservation::Over { sent: 8, got: 6 });
        beliefs.observe_custodian(call("M0AAA"), 100, CustodianObservation::Accepted);
        let saved = beliefs.take_changed();
        assert_eq!(saved.len(), 2);
        assert!(beliefs.take_changed().is_empty());
        let mut restored = Beliefs::new();
        for (key, value) in &saved {
            restored.restore(key, value.as_ref().unwrap()).unwrap();
        }
        assert_eq!(restored.link(k), beliefs.link(k));
        assert_eq!(
            restored.custodian(call("M0AAA")),
            beliefs.custodian(call("M0AAA"))
        );
        beliefs.prune(100 + FORGET_AFTER + 1);
        let deleted = beliefs.take_changed();
        assert_eq!(deleted.len(), 2);
        assert!(deleted.iter().all(|(_, v)| v.is_none()));
        assert!(restored.restore(&[9, 9], &[1]).is_err());
    }

    #[test]
    fn silence_counts_against_a_link_until_the_horizon() {
        let mut beliefs = Beliefs::new();
        let k = key("M0AAA", "M0ME", Bearer::Radio);
        beliefs.observe_link(k, 0, LinkObservation::Beacon);
        let open_then = beliefs.p_open(k, 0, 0);
        assert_eq!(beliefs.note_silence(k, 3_600, 600), 5);
        assert!(beliefs.p_open(k, 3_600, 3_600) < open_then);
        assert_eq!(beliefs.note_silence(k, 3_600, 600), 0);
        let far = MISS_HORIZON + 10 * DAY;
        let missed = beliefs.note_silence(k, far, 3_600);
        // Hourly misses from the last one counted to the horizon, the last
        // half hour of it still in grace.
        assert_eq!(missed as u64, (MISS_HORIZON - 3_600 - 1_800) / 3_600);
    }

    /// Thompson draws scatter around the posterior mean, and stay the same for
    /// the same link within one plan.
    #[test]
    fn thompson_draws_are_consistent_within_a_plan() {
        let mut beliefs = Beliefs::new();
        let k = key("M0ME", "M0AAA", Bearer::Radio);
        for t in 0..5 {
            beliefs.observe_link(k, t * 600, LinkObservation::Heard);
            beliefs.observe_link(k, t * 600 + 1, LinkObservation::Handoff { ok: true });
        }
        let now = 3_000;
        let mut draw = beliefs.thompson(DetRng::from_seed(1), now);
        let first = draw.link(k, now, None);
        assert_eq!(draw.link(k, now, None), first);
        let mean = beliefs.mean(now).link(k, now, None);
        let average = (0..2_000)
            .map(|s| beliefs.thompson(DetRng::from_seed(s), now).link(k, now, None))
            .sum::<f64>()
            / 2_000.0;
        assert!((average - mean).abs() < 0.05, "{average} vs {mean}");
    }

    /// A stated probability counts for a couple of observations: it steers a
    /// link without evidence, and yields to evidence.
    #[test]
    fn stated_probabilities_yield_to_evidence() {
        let mut beliefs = Beliefs::new();
        let k = key("M0ME", "M0AAA", Bearer::Radio);
        let blind = beliefs.mean(0).link(k, 0, Some(0.95));
        assert!((blind - 0.95).abs() < 1e-9);
        for t in 0..40 {
            beliefs.observe_link(k, t * 60, LinkObservation::Heard);
            beliefs.observe_link(k, t * 60 + 1, LinkObservation::Handoff { ok: false });
        }
        let now = 40 * 60;
        let informed = beliefs.mean(now).link(k, now, Some(0.95));
        assert!(informed < 0.3, "{informed}");
    }
}
