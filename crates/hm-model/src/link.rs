//! One link's path (both directions, one bearer): is it within reach at all,
//! is it open, how well does it carry frames, and does a handoff over it
//! complete?
//!
//! Generative story, per path:
//!
//! ```text
//! r      ∈ {within reach, not}, fixed           (a station too far never opens)
//! π(t)   = P(open at t | r) by time of day       (diurnal::Diurnal)
//! o_t    ∈ {open, closed}, a Markov chain that relaxes to π(t) with
//!          correlation time T:  P(o_{t+Δ}=open | o_t) = π + (1[o_t] − π) e^{−Δ/T}
//! ε      = frame loss while open                 (erasure::Erasure)
//! h      = P(a handoff completes | open)         (Beta, fading evidence)
//! ```
//!
//! Reach is what makes a path that never opens cheap to rule out. The daily
//! pattern alone learns probabilities near zero slowly (a miss says little
//! once the path is believed mostly closed), so draws from it would keep
//! finding hours worth a try. Each miss on a path never seen open multiplies
//! the odds that it is within reach by `P(miss | within reach) = 1 − d·P(open)`,
//! the predictive of the rest of the model: a dozen misses at hours it
//! would be open if it were in reach rule it out. Anything seen open puts it
//! within reach for good.
//!
//! Every observation is a likelihood on this state, so each kind of news
//! moves exactly the parts it bears on:
//!
//! | observation         | open/closed            | erasure                | handoff              |
//! |---------------------|------------------------|------------------------|----------------------|
//! | `Beacon`, `Heard`   | open                   | one frame arrived      |                      |
//! | `Reported`          | open (hearsay)         |                        |                      |
//! | `Missed`            | closed, or open & lost | lost if it was open    |                      |
//! | `Up` / `Down`       | open / closed          |                        |                      |
//! | `Over { sent, got }`| open                   | `sent − got` lost      |                      |
//! | `Handoff { ok }`    | open / closed or failed|                        | completed / failed   |
//!
//! Mixture likelihoods (a miss, a failed handoff) are taken by assumed-density
//! filtering: each part gets the posterior responsibility of its explanation.
//!
//! `T` is a hyperparameter learned online: after each observation, `ln T`
//! takes a small step up the gradient of that observation's predictive
//! log-likelihood (type-II maximum likelihood by stochastic approximation).
//! HF openings that last hours and VHF paths that stay up for days end up with
//! their own `T`.
//!
//! The diurnal belief is updated with each observation weighted by
//! `min(1, Δ/T)`: observations closer together than the correlation time are
//! mostly the same news, and counting them in full would make the daily
//! pattern look better known than it is.

use hm_core::DetRng;
use libm::{exp, log};
use minicbor::{Decode, Encode};

use crate::diurnal::{Diurnal, Likelihood, SampledDiurnal};
use crate::erasure::Erasure;
use crate::evidence::{Beta, Evidence, Prior};
use crate::Bearer;

const DAY: u64 = 86_400;
/// Memory of handoff outcomes.
pub const HANDOFF_HALF_LIFE: u64 = 7 * DAY;
/// Memory of frame counts: a link's noise and path loss change over days.
pub const ERASURE_HALF_LIFE: u64 = 2 * DAY;
/// Step size of the online update of `ln T`.
const PERSISTENCE_STEP: f64 = 0.05;
/// Range `T` is kept in: a minute to a month.
const PERSISTENCE_RANGE: (f64, f64) = (60.0, 30.0 * 86_400.0);
/// Overs' worth of confidence in the prior fade dispersion.
const DISPERSION_PRIOR_OVERS: f64 = 3.0;
/// State probabilities are kept this far from 0 and 1, so one surprising
/// observation can never be impossible.
const STATE_FLOOR: f64 = 1.0e-6;

/// What is believed about a link before anything is seen on it: the
/// population prior for its bearer.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LinkPrior {
    /// Chance the path is within reach at all.
    pub reachable: f64,
    /// Chance the link is open at a random time, when within reach.
    pub p_open: f64,
    /// Whether openings follow the time of day.
    pub daily: bool,
    /// Chance a handoff completes while the link is open.
    pub handoff: Prior,
    /// Frame loss while open.
    pub erasure: Prior,
    /// Fade dispersion of frame loss within an over.
    pub dispersion: f64,
    /// Correlation time of the open/closed state, seconds.
    pub persistence_secs: f64,
}

impl LinkPrior {
    /// Weak defaults for a bearer: the hyperprior the population's own links
    /// refine (see [`crate::Beliefs`]).
    pub fn for_bearer(bearer: Bearer) -> LinkPrior {
        match bearer {
            Bearer::Radio => LinkPrior {
                reachable: 0.5,
                p_open: 0.3,
                daily: true,
                handoff: Prior::new(0.8, 2.0),
                erasure: Prior::new(0.15, 4.0),
                dispersion: 0.1,
                persistence_secs: 3_600.0,
            },
            Bearer::Modem => LinkPrior {
                reachable: 0.5,
                p_open: 0.3,
                daily: true,
                handoff: Prior::new(0.7, 2.0),
                erasure: Prior::new(0.05, 4.0),
                dispersion: 0.1,
                persistence_secs: 3_600.0,
            },
            Bearer::Internet => LinkPrior {
                reachable: 0.95,
                p_open: 0.5,
                daily: false,
                handoff: Prior::new(0.95, 4.0),
                erasure: Prior::new(0.001, 4.0),
                dispersion: 0.0,
                persistence_secs: 6.0 * 3_600.0,
            },
        }
    }
}

/// Something seen that bears on a link.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LinkObservation {
    /// One of the periodic frames sent over the link (a beacon) arrived: it
    /// was open. The gaps between them teach how often they come, so that
    /// one that does not come can be noticed ([`LinkModel::due_misses`]).
    Beacon,
    /// A frame sent over the link arrived: it was open.
    Heard,
    /// Another station's signed report that it heard over this link.
    Reported,
    /// A frame due over the link (a beacon) did not arrive.
    Missed,
    /// A session over the link is up: it is open.
    Up,
    /// A session over the link went down: it is closed.
    Down,
    /// An over of `sent` frames, of which the ACK says `got` arrived.
    Over { sent: u32, got: u32 },
    /// A custody handoff over the link completed, or failed for a reason of
    /// the link (not a refusal, which is the custodian's).
    Handoff { ok: bool },
}

/// Moments of the share of frames lost per over, faded with time, from which
/// the fade dispersion is estimated.
#[derive(Copy, Clone, Debug, Default, PartialEq, Encode, Decode)]
struct Fades {
    #[n(0)]
    weight: f64,
    #[n(1)]
    sum: f64,
    #[n(2)]
    sum_sq: f64,
    #[n(3)]
    frames: f64,
    #[n(4)]
    at: u64,
}

impl Fades {
    fn add(&mut self, share_lost: f64, frames: u32, at: u64) {
        let f = crate::math::fade(at.saturating_sub(self.at), ERASURE_HALF_LIFE);
        let f = if self.weight == 0.0 { 0.0 } else { f };
        *self = Fades {
            weight: self.weight * f + 1.0,
            sum: self.sum * f + share_lost,
            sum_sq: self.sum_sq * f + share_lost * share_lost,
            frames: self.frames * f + f64::from(frames),
            at: at.max(self.at),
        };
    }

    /// Intra-over correlation of losses, by the method of moments, shrunk
    /// toward `prior`: the Beta-binomial's variance is
    /// `μ(1−μ)/n · (1 + (n−1)ρ)`.
    fn dispersion(&self, now: u64, prior: f64) -> f64 {
        let f = crate::math::fade(now.saturating_sub(self.at), ERASURE_HALF_LIFE);
        let w = self.weight * f;
        if w < 1.0 {
            return prior;
        }
        let mean = self.sum / self.weight;
        let var = (self.sum_sq / self.weight - mean * mean).max(0.0);
        let n = (self.frames / self.weight).max(1.0);
        let spread = mean * (1.0 - mean);
        let estimate = if spread < 1.0e-6 || n <= 1.0 {
            prior
        } else {
            ((var - spread / n) / (spread * (1.0 - 1.0 / n))).clamp(0.0, 0.95)
        };
        (w * estimate + DISPERSION_PRIOR_OVERS * prior) / (w + DISPERSION_PRIOR_OVERS)
    }
}

/// Belief about one directed link.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct LinkModel {
    #[n(0)]
    availability: Diurnal,
    /// P(open at `state_at`).
    #[n(1)]
    state: f64,
    #[n(2)]
    state_at: u64,
    /// ln T, the correlation time of the open/closed state in seconds.
    #[n(3)]
    log_persistence: f64,
    /// Handoffs completed (yes) and failed while open (no).
    #[n(4)]
    handoff: Evidence,
    /// Frames lost (yes) and arrived (no).
    #[n(5)]
    erasure: Evidence,
    #[n(6)]
    fades: Fades,
    #[n(7)]
    last_open: Option<u64>,
    /// When the diurnal belief last took an observation.
    #[n(8)]
    diurnal_at: Option<u64>,
    #[n(9)]
    observed_at: u64,
    /// Up to when frames due on the link have been accounted for.
    #[n(10)]
    expected_until: u64,
    /// When the last beacon came, and about how often they come: the
    /// shortest gap between two, let grow slowly so that a station that
    /// beacons less often is followed.
    #[n(11)]
    last_beacon: Option<u64>,
    #[n(12)]
    beacon_gap: Option<f64>,
    /// Chance the path is not within reach at all: never seen open, and
    /// missed where it would have been heard if it were. Zero once anything
    /// came through (and for records saved before reach was modelled, which
    /// were of paths heard).
    #[n(13)]
    #[cbor(default)]
    out_of_reach: f64,
}

/// A beacon gap estimate grows by this share of itself with each beacon, so a
/// peer that lengthens its interval is followed while jitter and misses (which
/// only lengthen gaps) do not inflate it.
const GAP_GROWTH: f64 = 0.05;

impl LinkModel {
    pub fn new(prior: &LinkPrior, now: u64) -> LinkModel {
        LinkModel {
            availability: Diurnal::new(prior.p_open, prior.daily, now),
            // Nothing seen yet: the state is the daily pattern, relaxed long ago.
            state: prior.p_open,
            state_at: 0,
            log_persistence: log(prior
                .persistence_secs
                .clamp(PERSISTENCE_RANGE.0, PERSISTENCE_RANGE.1)),
            handoff: Evidence::default(),
            erasure: Evidence::default(),
            fades: Fades::default(),
            last_open: None,
            diurnal_at: None,
            observed_at: now,
            expected_until: now,
            last_beacon: None,
            beacon_gap: None,
            out_of_reach: 1.0 - prior.reachable.clamp(0.0, 1.0),
        }
    }

    /// Chance the path is within reach at all.
    pub fn reachable(&self) -> f64 {
        1.0 - self.out_of_reach
    }

    /// Correlation time of the open/closed state, seconds.
    pub fn persistence_secs(&self) -> f64 {
        exp(self.log_persistence)
    }

    /// Last time the link was seen open (a frame, a report, a session, an over).
    pub fn last_open(&self) -> Option<u64> {
        self.last_open
    }

    /// About how often the link's periodic frames (beacons) come, once two
    /// have been seen.
    pub fn beacon_interval(&self) -> Option<u64> {
        self.beacon_gap.map(|g| libm::round(g) as u64)
    }

    /// Last time anything was observed on the link.
    pub fn observed_at(&self) -> u64 {
        self.observed_at
    }

    /// The diurnal belief: P(open) by time of day, before conditioning on the
    /// latest observations.
    pub fn availability(&self) -> &Diurnal {
        &self.availability
    }

    /// Probability that the link is open at `t` (Unix seconds), as believed at
    /// `now`: within reach, and then the latest state relaxed toward the
    /// time-of-day pattern.
    pub fn p_open(&self, t: u64, now: u64) -> f64 {
        self.reachable() * self.p_open_in_reach(t, now)
    }

    /// Probability that the link is open at `t`, were it within reach.
    fn p_open_in_reach(&self, t: u64, now: u64) -> f64 {
        let pi = self.availability.p_open(t, now);
        if t <= self.state_at {
            return self.state;
        }
        let decay = exp(-((t - self.state_at) as f64) / self.persistence_secs());
        pi + (self.state - pi) * decay
    }

    /// This station's own handoff outcomes on the link, faded to `now`:
    /// (completed, failed while open).
    pub fn handoff_counts(&self, now: u64) -> (f64, f64) {
        self.handoff.faded(now, HANDOFF_HALF_LIFE)
    }

    /// Frames counted on the link, faded to `now`: (lost, arrived).
    pub fn frame_counts(&self, now: u64) -> (f64, f64) {
        self.erasure.faded(now, ERASURE_HALF_LIFE)
    }

    /// Belief about the chance that a handoff completes while the link is open.
    pub fn handoff(&self, prior: &LinkPrior, now: u64) -> Beta {
        self.handoff.posterior(prior.handoff, now, HANDOFF_HALF_LIFE)
    }

    /// Belief about frame loss while the link is open.
    pub fn erasure(&self, prior: &LinkPrior, now: u64) -> Erasure {
        let beta = self.erasure.posterior(prior.erasure, now, ERASURE_HALF_LIFE);
        Erasure {
            lost: beta.a,
            got: beta.b,
            dispersion: self.fades.dispersion(now, prior.dispersion),
        }
    }

    /// Chance that a handoff started over the link at `t` completes: it is
    /// open, and a handoff over it completes.
    pub fn success(&self, prior: &LinkPrior, t: u64, now: u64) -> f64 {
        self.p_open(t, now) * self.handoff(prior, now).mean()
    }

    /// Take an observation made at `at`.
    pub fn observe(&mut self, prior: &LinkPrior, at: u64, observation: LinkObservation) {
        let now = at.max(self.observed_at);
        let handoff_mean = self.handoff(prior, now).mean();
        let detect = 1.0 - self.erasure(prior, now).mean();
        let likelihood = match observation {
            LinkObservation::Beacon
            | LinkObservation::Heard
            | LinkObservation::Reported
            | LinkObservation::Up
            | LinkObservation::Over { .. }
            | LinkObservation::Handoff { ok: true } => Likelihood::Open,
            LinkObservation::Down => Likelihood::Closed,
            LinkObservation::Missed => Likelihood::Missed { detect },
            LinkObservation::Handoff { ok: false } => Likelihood::Missed { detect: handoff_mean },
        };
        let in_reach = self.reach(at, likelihood);
        // Open, and within reach: what a miss's lost frame or a failed
        // handoff is weighed by.
        let open_after = in_reach * self.filter(at, likelihood, observation != LinkObservation::Reported);
        let weight = self.diurnal_at.map_or(1.0, |last| {
            (at.abs_diff(last) as f64 / self.persistence_secs()).min(1.0)
        });
        self.availability.update(at, likelihood, weight);
        self.diurnal_at = Some(self.diurnal_at.map_or(at, |last| last.max(at)));
        match observation {
            LinkObservation::Beacon => {
                self.erasure.add(0.0, 1.0, at, ERASURE_HALF_LIFE);
                if let Some(previous) = self.last_beacon.filter(|p| at > *p) {
                    let gap = (at - previous) as f64;
                    self.beacon_gap =
                        Some(self.beacon_gap.map_or(gap, |g| (g * (1.0 + GAP_GROWTH)).min(gap)));
                }
                self.last_beacon = Some(self.last_beacon.map_or(at, |p| p.max(at)));
            }
            LinkObservation::Heard => self.erasure.add(0.0, 1.0, at, ERASURE_HALF_LIFE),
            LinkObservation::Missed => self.erasure.add(open_after, 0.0, at, ERASURE_HALF_LIFE),
            LinkObservation::Over { sent, got } if sent > 0 => {
                let got = got.min(sent);
                self.erasure
                    .add(f64::from(sent - got), f64::from(got), at, ERASURE_HALF_LIFE);
                self.fades.add(f64::from(sent - got) / f64::from(sent), sent, at);
            }
            LinkObservation::Handoff { ok: true } => self.handoff.add(1.0, 0.0, at, HANDOFF_HALF_LIFE),
            LinkObservation::Handoff { ok: false } => {
                self.handoff.add(0.0, open_after, at, HANDOFF_HALF_LIFE)
            }
            _ => {}
        }
        if likelihood == Likelihood::Open {
            self.last_open = Some(self.last_open.map_or(at, |t| t.max(at)));
        }
        self.observed_at = now;
        if matches!(observation, LinkObservation::Beacon | LinkObservation::Missed) {
            self.expected_until = self.expected_until.max(at);
        }
    }

    /// Beacons due over the link that have not come by `now`: the times they
    /// were due, one interval apart from the last beacon or miss. The
    /// interval is the link's own, once learned, else `interval`. A beacon
    /// counts as missed only half an interval after it was due: beacons come
    /// a little early or late.
    pub fn due_misses(&self, now: u64, interval: u64) -> impl Iterator<Item = u64> {
        let interval = self.beacon_interval().unwrap_or(interval).max(1);
        let from = self.expected_until;
        let by = now.saturating_sub(interval / 2);
        (1..)
            .map(move |k| from.saturating_add(k * interval))
            .take_while(move |t| *t <= by)
    }

    /// Bayes' rule on whether the path is within reach, for an observation at
    /// `at`: anything seen open puts it there for good; a miss counts
    /// against it by how likely it was to get through were the path within
    /// reach (the predictive of the rest of the model). Returns the chance
    /// it is within reach after the observation.
    fn reach(&mut self, at: u64, likelihood: Likelihood) -> f64 {
        match likelihood {
            Likelihood::Open => self.out_of_reach = 0.0,
            Likelihood::Closed => {}
            Likelihood::Missed { detect } => {
                let r = self.reachable();
                if r > 0.0 && r < 1.0 {
                    let within = r * (1.0 - detect * self.p_open_in_reach(at, at));
                    self.out_of_reach = ((1.0 - r) / (within + 1.0 - r)).clamp(0.0, 1.0);
                }
            }
        }
        self.reachable()
    }

    /// Bayes' rule on the open/closed state for an observation at `at`, and
    /// a step of `ln T` up its predictive log-likelihood (when `learn`).
    /// Returns P(open at `at`) after the observation.
    fn filter(&mut self, at: u64, likelihood: Likelihood, learn: bool) -> f64 {
        if at < self.state_at {
            // An old report: it bears on the daily pattern, not on the state now.
            return match likelihood {
                Likelihood::Open => 1.0,
                Likelihood::Closed => 0.0,
                Likelihood::Missed { detect } => {
                    let b = self.state;
                    b * (1.0 - detect) / (1.0 - detect * b)
                }
            };
        }
        let pi = self.availability.p_open(at, at);
        let t = self.persistence_secs();
        let elapsed = (at - self.state_at) as f64;
        let decay = exp(-elapsed / t);
        let prior = (pi + (self.state - pi) * decay).clamp(STATE_FLOOR, 1.0 - STATE_FLOOR);
        // d prior / d ln T.
        let slope = (self.state - pi) * decay * elapsed / t;
        let (posterior, gradient) = match likelihood {
            Likelihood::Open => (1.0, slope / prior),
            Likelihood::Closed => (0.0, -slope / (1.0 - prior)),
            Likelihood::Missed { detect } => (
                prior * (1.0 - detect) / (1.0 - detect * prior),
                -detect * slope / (1.0 - detect * prior),
            ),
        };
        if learn && elapsed > 0.0 && gradient.is_finite() {
            self.log_persistence = (self.log_persistence + PERSISTENCE_STEP * gradient.clamp(-10.0, 10.0))
                .clamp(log(PERSISTENCE_RANGE.0), log(PERSISTENCE_RANGE.1));
        }
        self.state = posterior.clamp(STATE_FLOOR, 1.0 - STATE_FLOOR);
        self.state_at = at;
        posterior
    }

    /// One plausible link, drawn from the belief (Thompson sampling).
    pub fn sample(&self, prior: &LinkPrior, rng: &mut DetRng, now: u64) -> SampledLink {
        SampledLink {
            reachable: rng.chance(self.reachable()),
            availability: self.availability.sample(rng, now),
            state: self.state,
            state_at: self.state_at,
            persistence_secs: self.persistence_secs(),
            handoff: self.handoff(prior, now).sample(rng),
        }
    }
}

/// A link drawn from a [`LinkModel`]: fixed rates, for planning with
/// Thompson sampling.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SampledLink {
    reachable: bool,
    availability: SampledDiurnal,
    state: f64,
    state_at: u64,
    persistence_secs: f64,
    handoff: f64,
}

impl SampledLink {
    /// Chance the path is open at `t`.
    pub fn open(&self, t: u64) -> f64 {
        if !self.reachable {
            return 0.0;
        }
        let pi = self.availability.p_open(t);
        if t <= self.state_at {
            self.state
        } else {
            let decay = exp(-((t - self.state_at) as f64) / self.persistence_secs);
            pi + (self.state - pi) * decay
        }
    }

    /// Chance a handoff completes while the path is open.
    pub fn handoff(&self) -> f64 {
        self.handoff
    }

    /// Chance a handoff started at `t` completes.
    pub fn success(&self, t: u64) -> f64 {
        self.open(t) * self.handoff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;
    const DAY: u64 = 24 * HOUR;

    fn radio() -> LinkPrior {
        LinkPrior::for_bearer(Bearer::Radio)
    }

    /// Heard now: open now, and less sure the longer ago.
    #[test]
    fn a_hearing_fades_toward_the_daily_pattern() {
        let prior = radio();
        let mut link = LinkModel::new(&prior, 0);
        link.observe(&prior, 1_000, LinkObservation::Heard);
        assert!(link.p_open(1_000, 1_000) > 0.99);
        let soon = link.p_open(1_000 + 600, 1_000);
        let later = link.p_open(1_000 + 6 * HOUR, 1_000);
        assert!(soon > later, "{soon} {later}");
        assert!(later < 0.6, "{later}");
    }

    /// A path heard every 10 minutes from 12 to 18 UTC and missed otherwise:
    /// after a week the model expects it open in the afternoon, closed at
    /// night, and its losses were counted only when it was likely open.
    #[test]
    fn learns_when_an_hf_path_opens() {
        let prior = radio();
        let mut link = LinkModel::new(&prior, 0);
        for day in 0..7 {
            for step in 0..144 {
                let t = day * DAY + step * 600;
                let hour = (t % DAY) / HOUR;
                let observation = if (12..18).contains(&hour) {
                    LinkObservation::Beacon
                } else {
                    LinkObservation::Missed
                };
                link.observe(&prior, t, observation);
            }
        }
        let now = 7 * DAY;
        let afternoon = link.p_open(now + 15 * HOUR, now);
        let night = link.p_open(now + 3 * HOUR, now);
        assert!(afternoon > 0.7, "afternoon {afternoon:.2}");
        assert!(night < 0.15, "night {night:.2}");
        // Closed at night is not frame loss.
        assert!(
            link.erasure(&prior, now).mean() < 0.15,
            "{:?}",
            link.erasure(&prior, now)
        );
    }

    /// Refusals are not the link's; failed handoffs are, when the link was
    /// likely open.
    #[test]
    fn handoff_failures_while_heard_lower_the_handoff_rate() {
        let prior = radio();
        let mut link = LinkModel::new(&prior, 0);
        for n in 0..10 {
            let t = n * 60;
            link.observe(&prior, t, LinkObservation::Heard);
            link.observe(&prior, t + 1, LinkObservation::Handoff { ok: false });
        }
        assert!(link.handoff(&prior, 600).mean() < 0.4);
    }

    /// Over counts say how lossy the link is, and fades show up as dispersion.
    #[test]
    fn overs_measure_loss_and_fades() {
        let prior = radio();
        let mut steady = LinkModel::new(&prior, 0);
        let mut fading = LinkModel::new(&prior, 0);
        for n in 0..40 {
            let t = n * 120;
            steady.observe(&prior, t, LinkObservation::Over { sent: 10, got: 8 });
            let got = if n % 5 == 0 { 0 } else { 10 };
            fading.observe(&prior, t, LinkObservation::Over { sent: 10, got });
        }
        let now = 40 * 120;
        let (s, f) = (steady.erasure(&prior, now), fading.erasure(&prior, now));
        assert!((s.mean() - 0.2).abs() < 0.03, "{}", s.mean());
        assert!((f.mean() - 0.2).abs() < 0.03, "{}", f.mean());
        assert!(
            f.dispersion > 0.5 && s.dispersion < 0.1,
            "{} {}",
            f.dispersion,
            s.dispersion
        );
    }

    /// A link that opens and closes for a day at a time learns a long
    /// correlation time; one that opens for ten minutes an hour, a short one.
    /// (A link heard every time would not do: open all the time, it looks
    /// the same whatever `T`, and `T` rightly stays put. Heard and missed in
    /// strict alternation would not do either: a link that stays open and
    /// loses half its frames explains that as well.)
    #[test]
    fn persistence_is_learned() {
        let prior = radio();
        let mut steady = LinkModel::new(&prior, 0);
        let mut brief = LinkModel::new(&prior, 0);
        for n in 0..4_000 {
            let t = n * 300;
            let obs = if (n / 288) % 2 == 0 {
                LinkObservation::Heard
            } else {
                LinkObservation::Missed
            };
            steady.observe(&prior, t, obs);
            let obs = if n % 12 < 2 {
                LinkObservation::Heard
            } else {
                LinkObservation::Missed
            };
            brief.observe(&prior, t, obs);
        }
        let (long, short) = (steady.persistence_secs(), brief.persistence_secs());
        assert!(long > 3.0 * HOUR as f64, "{long}");
        assert!(short < HOUR as f64 && short < long / 4.0, "{short}");
    }

    #[test]
    fn misses_are_due_once_per_interval_after_the_last_beacon() {
        let prior = radio();
        let mut link = LinkModel::new(&prior, 0);
        link.observe(&prior, 100, LinkObservation::Beacon);
        let due: Vec<u64> = link.due_misses(800, 200).collect();
        assert_eq!(due, vec![300, 500, 700]);
        for t in due {
            link.observe(&prior, t, LinkObservation::Missed);
        }
        assert_eq!(link.due_misses(850, 200).count(), 0);
        // Other frames heard are no beacons: they neither teach the interval
        // nor stand for a beacon that was due.
        link.observe(&prior, 710, LinkObservation::Heard);
        assert_eq!(link.due_misses(1_000, 200).collect::<Vec<_>>(), vec![900]);
    }

    /// A station that beacons every ten minutes, a little early or late, is
    /// not missed by one that beacons every five.
    #[test]
    fn the_beacon_interval_is_the_peers_own() {
        let prior = radio();
        let mut link = LinkModel::new(&prior, 0);
        for (n, jitter) in [0, 40, 0, 55, 10, 0, 30].into_iter().enumerate() {
            link.observe(&prior, 600 * n as u64 + jitter, LinkObservation::Beacon);
        }
        let interval = link.beacon_interval().unwrap();
        assert!((540..=640).contains(&interval), "{interval}");
        // Just after a beacon was due, it is not yet missed.
        assert_eq!(link.due_misses(3_600 + 650, 300).count(), 0);
        assert_eq!(link.due_misses(3_600 + 1_300, 300).count(), 1);
    }

    #[test]
    fn samples_average_to_the_belief() {
        let prior = radio();
        let mut link = LinkModel::new(&prior, 0);
        for n in 0..10 {
            link.observe(&prior, n * 600, LinkObservation::Heard);
            link.observe(&prior, n * 600 + 1, LinkObservation::Handoff { ok: n % 3 != 0 });
        }
        let now = 6_000;
        let t = now + 1_800;
        let mut rng = DetRng::from_seed(11);
        let mean = (0..4_000)
            .map(|_| link.sample(&prior, &mut rng, now).success(t))
            .sum::<f64>()
            / 4_000.0;
        let expected = link.success(&prior, t, now);
        assert!((mean - expected).abs() < 0.04, "{mean} vs {expected}");
    }

    #[test]
    fn the_model_round_trips_through_cbor() {
        let prior = radio();
        let mut link = LinkModel::new(&prior, 5);
        link.observe(&prior, 10, LinkObservation::Over { sent: 4, got: 3 });
        let bytes = minicbor::to_vec(&link).unwrap();
        let back: LinkModel = minicbor::decode(&bytes).unwrap();
        assert_eq!(back, link);
    }
}
