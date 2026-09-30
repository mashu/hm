//! What the search knows before it starts: the chance of a handoff on each
//! known link at each departure on the forecast grid, and, from those, the
//! least risk any route from each station to the destination can have.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use hm_model::{Estimate, LinkKey};
use hm_wire::Callsign;

use super::{RouteRequest, RoutingPolicy};
use crate::ContactGraph;

/// Seconds to send `bytes` at `rate_bps`.
pub(super) fn transfer_secs(bytes: u64, rate_bps: u32) -> u64 {
    bytes
        .saturating_mul(8)
        .div_ceil(u64::from(rate_bps.max(1)))
        .max(1)
}

/// Known links are forecast an hour apart once the fine steps are over: the
/// daily pattern's resolution (its shortest period is twelve hours).
const COARSE_STEP_SECS: u64 = 3_600;

/// The chance of a handoff (the link completes it, the next station accepts)
/// on each known link at each departure on the grid, worked out once per
/// plan. The grid is fine at first, while what the path is doing now still
/// tells about it ([`RoutingPolicy::forecast_step_secs`] apart for
/// [`RoutingPolicy::forecast_fine_secs`]), then hourly to the horizon.
pub(super) struct Forecasts {
    departures: Vec<u64>,
    horizon: u64,
    handed: BTreeMap<LinkKey, Vec<f64>>,
}

impl Forecasts {
    pub(super) fn new(request: &RouteRequest<'_>, policy: RoutingPolicy) -> Self {
        let step = policy.forecast_step_secs.max(1);
        let coarse = COARSE_STEP_SECS.max(step);
        let horizon = request
            .expires_at
            .min(request.now.saturating_add(policy.forecast_horizon_secs));
        let fine_until = request.now.saturating_add(policy.forecast_fine_secs);
        let mut departures = Vec::new();
        let mut t = (request.now / step + 1) * step;
        while policy.forecast_horizon_secs > 0 && t < horizon {
            departures.push(t);
            t = if t < fine_until {
                t + step
            } else {
                (t / coarse + 1) * coarse
            };
        }
        Forecasts {
            departures,
            horizon,
            handed: BTreeMap::new(),
        }
    }

    /// Whether known links are forecast at all.
    pub(super) fn is_empty(&self) -> bool {
        self.departures.is_empty()
    }

    /// The end of the grid.
    pub(super) fn horizon(&self) -> u64 {
        self.horizon
    }

    /// The departures on the grid, and the chance of a handoff over `link` at
    /// each, when one takes `transfer` seconds.
    pub(super) fn grid(
        &mut self,
        estimate: &mut dyn Estimate,
        link: LinkKey,
        transfer: u64,
    ) -> (&[u64], &[f64]) {
        let Forecasts {
            departures, handed, ..
        } = self;
        let grid = handed.entry(link).or_insert_with(|| {
            departures
                .iter()
                .map(|&depart| {
                    estimate.link(link, depart, None) * estimate.accepts(link.to, depart + transfer)
                })
                .collect()
        });
        (departures, grid)
    }
}

/// The least risk (`−ln P`) of any route from each station to the
/// destination: a shortest path back from the destination, each link counted
/// at its best chance in the plan (now, at its best departure on the grid,
/// or over a contact the graph holds) and each custodian before the
/// destination at its chance of doing its part.
///
/// No route from a station can be likelier, so `P·e^{−togo}·u − C` stays an
/// upper bound on the utility of any route through a label there, and a far
/// better one than `P·u − C` alone: a label far from the destination, or at a
/// station that only reaches it through unlikely links, is known to be worth
/// little before it is expanded, and one at a station with no way on is not
/// kept at all. (The chance is taken now and at the grid's departures; a label
/// arriving between two of them may leave a little likelier, by as much as
/// the chance changes between two departures.)
pub(super) struct RiskToGo {
    risk: BTreeMap<Callsign, f64>,
}

impl RiskToGo {
    pub(super) fn new(
        graph: &ContactGraph,
        estimate: &mut dyn Estimate,
        request: &RouteRequest<'_>,
        forecasts: &mut Forecasts,
    ) -> Self {
        let now = request.now;
        // The likeliest handoff over each pair of stations, whatever the bearer.
        let mut best: BTreeMap<(Callsign, Callsign), f64> = BTreeMap::new();
        let mut offer = |from: Callsign, to: Callsign, handed: f64| {
            let entry = best.entry((from, to)).or_insert(0.0);
            *entry = entry.max(handed);
        };
        for (from, to, bearer, known) in graph.links() {
            if from == request.destination {
                continue;
            }
            let link = LinkKey { from, to, bearer };
            let transfer = transfer_secs(request.object_bytes, known.rate_bps);
            let soon = estimate.link(link, now, None) * estimate.accepts(to, now + transfer);
            let later = forecasts
                .grid(estimate, link, transfer)
                .1
                .iter()
                .copied()
                .fold(0.0, f64::max);
            offer(from, to, soon.max(later));
        }
        for contact in graph.contacts(now) {
            let link = contact.key.link();
            if link.from == request.destination {
                continue;
            }
            let depart = contact.start.max(now);
            let arrive = depart + transfer_secs(request.object_bytes, contact.rate_bps);
            let handed = estimate.link(link, depart, contact.stated()) * estimate.accepts(link.to, arrive);
            offer(link.from, link.to, handed);
        }
        let mut into: BTreeMap<Callsign, Vec<(Callsign, f64)>> = BTreeMap::new();
        for ((from, to), handed) in best {
            let custodian = if to == request.destination {
                1.0
            } else {
                estimate.delivers(to)
            };
            // As a hop's chance is in the search: a link believed never to
            // complete is still a way, only one not worth trying.
            let chance = (handed * custodian).clamp(1.0e-9, 1.0);
            into.entry(to).or_default().push((from, -chance.ln()));
        }
        let mut risk = BTreeMap::new();
        let mut queue = BinaryHeap::from([Least(0.0, request.destination)]);
        while let Some(Least(so_far, station)) = queue.pop() {
            if risk.contains_key(&station) {
                continue;
            }
            risk.insert(station, so_far);
            for &(from, edge) in into.get(&station).into_iter().flatten() {
                if !risk.contains_key(&from) {
                    queue.push(Least(so_far + edge, from));
                }
            }
        }
        RiskToGo { risk }
    }

    /// The least risk from `station` on, or `None` when no known link leads
    /// from it to the destination.
    pub(super) fn from(&self, station: Callsign) -> Option<f64> {
        self.risk.get(&station).copied()
    }
}

/// A station by the risk found so far to it, least first out of the heap.
struct Least(f64, Callsign);

impl PartialEq for Least {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Least {}

impl PartialOrd for Least {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Least {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.total_cmp(&self.0).then(other.1.cmp(&self.1))
    }
}
