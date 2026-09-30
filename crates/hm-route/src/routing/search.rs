//! A* over partial routes: labels, their expansion by contacts and by
//! forecast departures on known links, and dominance.
//!
//! Labels live in an arena, each pointing at the label it extends: extending
//! one copies nothing of the route so far, and the stations a route has
//! passed are found by walking back (routes are at most 16 hops). The chance
//! of a handoff on each known link is forecast once per plan on a grid of
//! departures, the first time a label reaches the link's station, and shared
//! by every label that leaves over it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use hm_model::{Estimate, LinkKey};
use hm_wire::Callsign;

use super::{Route, RouteError, RouteHop, RouteRequest, RoutingPolicy};
use crate::{Bearer, ContactGraph, ContactKey};

/// Airtime an attempt costs besides the object, as bytes on the link:
/// key-ups, the OFFER (and OPEN), the ACK, and the probes that find a
/// closed link closed.
const ATTEMPT_OVERHEAD_BYTES: u64 = 200;
/// A later departure on a known link is tried only when its chance is this
/// much better than every earlier one's: later and barely likelier is not
/// worth the search.
const FORECAST_GAIN: f64 = 1.01;

/// A partial route, from the source to `station`.
struct Label {
    station: Callsign,
    arrival: u64,
    airtime_millis: u64,
    risk_cost: f64,
    attempt_cost: f64,
    first_probability: f64,
    /// Upper bound on the utility of any route through this label.
    bound: f64,
    /// The hop that reached `station`, and the label it extended; none at
    /// the source.
    hop: Option<RouteHop>,
    parent: Option<usize>,
    hops: u8,
    /// The first hop's station: labels through different neighbours are
    /// kept apart, for failover and urgent copies.
    first: Option<Callsign>,
}

/// A label waiting in the queue, by its place in the arena.
struct Queued {
    bound: f64,
    arrival: u64,
    airtime_millis: u64,
    hops: u8,
    station: Callsign,
    index: usize,
}

impl Queued {
    fn of(label: &Label, index: usize) -> Self {
        Queued {
            bound: label.bound,
            arrival: label.arrival,
            airtime_millis: label.airtime_millis,
            hops: label.hops,
            station: label.station,
            index,
        }
    }

    /// Best first: higher bound, then earlier, less airtime, fewer hops.
    fn order(&self, other: &Self) -> Ordering {
        other
            .bound
            .total_cmp(&self.bound)
            .then(self.arrival.cmp(&other.arrival))
            .then(self.airtime_millis.cmp(&other.airtime_millis))
            .then(self.hops.cmp(&other.hops))
            .then(self.station.cmp(&other.station))
    }
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.order(other) == Ordering::Equal
    }
}

impl Eq for Queued {}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap pops the greatest: the best label is the greatest.
        other.order(self)
    }
}

/// The labels of one search.
struct Arena<'r> {
    labels: Vec<Label>,
    request: &'r RouteRequest<'r>,
}

impl Arena<'_> {
    /// Whether the route to label `index` has passed `station` (the stations
    /// the message came through before the source count as passed).
    fn passed(&self, index: usize, station: Callsign) -> bool {
        self.request.visited.contains(&station) || self.chain(index).any(|label| label.station == station)
    }

    /// The label at `index` and those it extends, back to the source.
    fn chain(&self, index: usize) -> impl Iterator<Item = &Label> {
        let mut next = Some(index);
        std::iter::from_fn(move || {
            let label = &self.labels[next?];
            next = label.parent;
            Some(label)
        })
    }

    /// Every station the route to label `index` has passed.
    fn stations(&self, index: usize) -> Vec<Callsign> {
        let mut stations = self.request.visited.to_vec();
        stations.extend(self.chain(index).map(|label| label.station));
        stations
    }

    fn route(&self, index: usize) -> Route {
        let label = &self.labels[index];
        let mut hops: Vec<RouteHop> = self.chain(index).filter_map(|label| label.hop).collect();
        hops.reverse();
        Route {
            success_probability: (-label.risk_cost).exp(),
            first_hop_probability: label.first_probability,
            risk_cost: label.risk_cost,
            attempt_cost: label.attempt_cost,
            utility: label.bound,
            arrival: label.arrival,
            airtime_millis: label.airtime_millis,
            hops,
        }
    }
}

/// What a label that was expanded left for later labels to be compared with.
struct Expanded {
    risk_cost: f64,
    attempt_cost: f64,
    arrival: u64,
    airtime_millis: u64,
    stations: Vec<Callsign>,
}

impl Expanded {
    /// Whatever a label could still reach, this one reached no later, with
    /// no more risk, cost or airtime, and with no more stations ruled out on
    /// the way: every route through the label has one at least as good
    /// through this. `passed` says which stations the label has passed.
    fn dominates(&self, label: &Label, passed: impl Fn(Callsign) -> bool) -> bool {
        self.risk_cost <= label.risk_cost
            && self.attempt_cost <= label.attempt_cost
            && self.arrival <= label.arrival
            && self.airtime_millis <= label.airtime_millis
            && self.stations.iter().all(|station| passed(*station))
    }
}

/// A way to leave a label's station, and the chance that a handoff over it
/// completes and is accepted.
struct Step {
    key: ContactKey,
    depart: u64,
    arrive: u64,
    rate_bps: u32,
    handed: f64,
    forecast: bool,
}

/// The chance of a handoff (the link completes it, the next station accepts)
/// on each known link at each departure on the grid `first + k·step`, worked
/// out once per plan.
struct Forecasts {
    first: u64,
    step: u64,
    /// Departures on the grid before the horizon.
    count: u64,
    handed: BTreeMap<LinkKey, Vec<f64>>,
}

impl Forecasts {
    fn new(request: &RouteRequest<'_>, policy: RoutingPolicy) -> Self {
        let step = policy.forecast_step_secs.max(1);
        let first = (request.now / step + 1) * step;
        let horizon = request
            .expires_at
            .min(request.now.saturating_add(policy.forecast_horizon_secs));
        let count = if policy.forecast_horizon_secs == 0 {
            0
        } else {
            horizon.saturating_sub(first).div_ceil(step)
        };
        Forecasts {
            first,
            step,
            count,
            handed: BTreeMap::new(),
        }
    }

    /// The end of the grid.
    fn horizon(&self) -> u64 {
        self.first + self.count * self.step
    }

    /// The grid for `link`, on which a handoff takes `transfer` seconds.
    fn grid(&mut self, estimate: &mut dyn Estimate, link: LinkKey, transfer: u64) -> &[f64] {
        let (first, step, count) = (self.first, self.step, self.count);
        self.handed.entry(link).or_insert_with(|| {
            (0..count)
                .map(|k| {
                    let depart = first + k * step;
                    estimate.link(link, depart, None) * estimate.accepts(link.to, depart + transfer)
                })
                .collect()
        })
    }
}

/// Routes to the destination, best bound first. A* over labels, pruned by
/// dominance: a label is not expanded when an expanded one at the same
/// station, with the same first hop, dominates it. Keeping first hops apart
/// keeps alternatives through other neighbours for failover and urgent
/// copies. When the label budget runs out, the routes found so far are
/// returned; only a search that found none fails.
///
/// Holding the message is always possible and worth nothing, so the search
/// ends at the first label whose bound is not above zero: no route through
/// it, or through any label after it, is worth its cost. (A route worth
/// less than nothing is worth no more with a fallback counted: the fallback
/// alone is worth more.)
pub(super) fn find_candidates(
    graph: &ContactGraph,
    estimate: &mut dyn Estimate,
    request: &RouteRequest<'_>,
    policy: RoutingPolicy,
) -> Result<Vec<Route>, RouteError> {
    if request.visited.contains(&request.source) {
        return Err(RouteError::InvalidRequest("source already visited"));
    }
    let mut arena = Arena {
        labels: vec![Label {
            station: request.source,
            arrival: request.now,
            airtime_millis: 0,
            risk_cost: 0.0,
            attempt_cost: 0.0,
            first_probability: 1.0,
            bound: 1.0,
            hop: None,
            parent: None,
            hops: 0,
            first: None,
        }],
        request,
    };
    let mut queue = BinaryHeap::from([Queued::of(&arena.labels[0], 0)]);
    let mut forecasts = Forecasts::new(request, policy);
    let mut routes = Vec::new();
    let mut expanded: BTreeMap<(Callsign, Callsign), Vec<Expanded>> = BTreeMap::new();
    let mut examined = 0_usize;
    let mut unprofitable = false;
    let mut steps = Vec::new();
    while let Some(Queued { index, .. }) = queue.pop() {
        let label = &arena.labels[index];
        if label.bound <= 0.0 {
            unprofitable = true;
            break;
        }
        examined += 1;
        if examined > policy.max_labels {
            if routes.is_empty() {
                return Err(RouteError::SearchLimit);
            }
            break;
        }
        if label.station == request.destination {
            routes.push(arena.route(index));
            if routes.len() == policy.max_candidates {
                break;
            }
            continue;
        }
        if request.visited.len() + usize::from(label.hops) >= usize::from(request.max_hops) {
            continue;
        }
        if let Some(first) = label.first {
            let seen = expanded.entry((label.station, first)).or_default();
            if seen
                .iter()
                .any(|e| e.dominates(label, |s| arena.passed(index, s)))
            {
                continue;
            }
            seen.push(Expanded {
                risk_cost: label.risk_cost,
                attempt_cost: label.attempt_cost,
                arrival: label.arrival,
                airtime_millis: label.airtime_millis,
                stations: arena.stations(index),
            });
        }
        steps.clear();
        contact_steps(graph, estimate, &arena, index, &mut steps);
        forecast_steps(graph, estimate, &mut forecasts, &arena, index, &mut steps);
        for step in steps.drain(..) {
            let Some(next) = extend(estimate, request, policy, &arena, index, step) else {
                continue;
            };
            let first = next.first.expect("an extended label has a first hop");
            let dominated = expanded.get(&(next.station, first)).is_some_and(|seen| {
                seen.iter()
                    .any(|e| e.dominates(&next, |s| s == next.station || arena.passed(index, s)))
            });
            if !dominated {
                queue.push(Queued::of(&next, arena.labels.len()));
                arena.labels.push(next);
            }
        }
    }
    if routes.is_empty() {
        return Err(if unprofitable {
            RouteError::NotWorthIt
        } else {
            RouteError::NoRoute
        });
    }
    routes.dedup_by(|left, right| {
        left.hops
            .iter()
            .map(|hop| hop.contact)
            .eq(right.hops.iter().map(|hop| hop.contact))
    });
    Ok(routes)
}

/// Whether the source may leave over `link` at `depart`.
fn allowed(request: &RouteRequest<'_>, label: &Label, link: LinkKey, depart: u64) -> bool {
    let at_source = label.parent.is_none();
    !(at_source
        && (request.forbidden[link.bearer] || (depart <= request.now && request.closed_now.contains(&link))))
}

fn transfer_secs(bytes: u64, rate_bps: u32) -> u64 {
    bytes
        .saturating_mul(8)
        .div_ceil(u64::from(rate_bps.max(1)))
        .max(1)
}

/// Leaving by the contacts the graph holds.
fn contact_steps(
    graph: &ContactGraph,
    estimate: &mut dyn Estimate,
    arena: &Arena<'_>,
    index: usize,
    steps: &mut Vec<Step>,
) {
    let (request, label) = (arena.request, &arena.labels[index]);
    for contact in graph.outgoing(label.station, request.now) {
        if contact.residual_capacity() < request.object_bytes || arena.passed(index, contact.key.to) {
            continue;
        }
        let depart = label.arrival.max(contact.start);
        if depart >= contact.end
            || depart >= contact.fresh_until
            || !allowed(request, label, contact.key.link(), depart)
        {
            continue;
        }
        let arrive = depart.saturating_add(transfer_secs(request.object_bytes, contact.rate_bps));
        if arrive > contact.end {
            continue;
        }
        let handed = estimate.link(contact.key.link(), depart, contact.stated())
            * estimate.accepts(contact.key.to, arrive);
        steps.push(Step {
            key: contact.key,
            depart,
            arrive,
            rate_bps: contact.rate_bps,
            handed,
            forecast: false,
        });
    }
}

/// Leaving later by the links the station has seen: at the label's arrival,
/// then every forecast step to the horizon, each with the chance the beliefs
/// forecast then, keeping only departures likelier than every earlier one.
fn forecast_steps(
    graph: &ContactGraph,
    estimate: &mut dyn Estimate,
    forecasts: &mut Forecasts,
    arena: &Arena<'_>,
    index: usize,
    steps: &mut Vec<Step>,
) {
    if forecasts.count == 0 {
        return;
    }
    let (request, label) = (arena.request, &arena.labels[index]);
    let horizon = forecasts.horizon();
    for (to, bearer, known) in graph.links_from(label.station) {
        if arena.passed(index, to) {
            continue;
        }
        let link = LinkKey {
            from: label.station,
            to,
            bearer,
        };
        let transfer = transfer_secs(request.object_bytes, known.rate_bps);
        let mut best = 0.0;
        let mut offer = |depart: u64, handed: f64| {
            if depart.saturating_add(transfer) > request.expires_at
                || handed <= best * FORECAST_GAIN
                || !allowed(request, label, link, depart)
            {
                return;
            }
            best = handed;
            steps.push(Step {
                key: ContactKey {
                    from: link.from,
                    to,
                    bearer,
                    epoch: depart,
                },
                depart,
                arrive: depart + transfer,
                rate_bps: known.rate_bps,
                handed,
                forecast: true,
            });
        };
        if label.arrival < horizon {
            let now =
                estimate.link(link, label.arrival, None) * estimate.accepts(to, label.arrival + transfer);
            offer(label.arrival, now);
        }
        // Grid departures after the label's arrival.
        let from = (label.arrival / forecasts.step + 1)
            .saturating_mul(forecasts.step)
            .saturating_sub(forecasts.first)
            / forecasts.step;
        let (first, step) = (forecasts.first, forecasts.step);
        for (k, handed) in forecasts
            .grid(estimate, link, transfer)
            .iter()
            .enumerate()
            .skip(from as usize)
        {
            offer(first + k as u64 * step, *handed);
        }
    }
}

/// The label one step on from label `index`, if it fits the deadline and the
/// airtime budget.
fn extend(
    estimate: &mut dyn Estimate,
    request: &RouteRequest<'_>,
    policy: RoutingPolicy,
    arena: &Arena<'_>,
    index: usize,
    step: Step,
) -> Option<Label> {
    let label = &arena.labels[index];
    if step.arrive > request.expires_at {
        return None;
    }
    let bearer = step.key.bearer;
    let rate = f64::from(step.rate_bps.max(1));
    // On the air, frames lost to fades and noise are sent again: the object
    // takes its expected airtime, not its length over the rate.
    let factor = if bearer.on_air() {
        estimate.airtime_factor(step.key.link())
    } else {
        1.0
    };
    let edge_airtime = match bearer {
        Bearer::Radio | Bearer::Modem => {
            (request.object_bytes as f64 * 8_000.0 * factor / rate).ceil() as u64
        }
        Bearer::Internet => 0,
    };
    let airtime_millis = label.airtime_millis.saturating_add(edge_airtime);
    if airtime_millis > request.airtime_budget_millis {
        return None;
    }
    let to = step.key.to;
    let handed = step.handed.clamp(1.0e-9, 1.0);
    // Before the destination, the custodian must also do its part.
    let probability = if to == request.destination {
        handed
    } else {
        (handed * estimate.delivers(to)).clamp(1.0e-9, 1.0)
    };
    let reach = (-label.risk_cost).exp();
    let attempt_airtime = if bearer.on_air() {
        (request.object_bytes as f64 * factor + ATTEMPT_OVERHEAD_BYTES as f64) * 8.0 / rate
    } else {
        0.0
    };
    let hop_cost = policy.attempt_cost[bearer] + policy.airtime_price[bearer] * attempt_airtime;
    let risk_cost = label.risk_cost - probability.ln();
    let attempt_cost = label.attempt_cost + reach * hop_cost;
    Some(Label {
        station: to,
        arrival: step.arrive,
        airtime_millis,
        risk_cost,
        attempt_cost,
        first_probability: if label.parent.is_none() {
            handed
        } else {
            label.first_probability
        },
        bound: (-risk_cost).exp() * request.value_at(step.arrive) - attempt_cost,
        hop: Some(RouteHop {
            contact: step.key,
            depart: step.depart,
            arrive: step.arrive,
            airtime_millis: edge_airtime,
            probability_permillion: (probability * 1_000_000.0).round() as u32,
            forecast: step.forecast,
        }),
        parent: Some(index),
        hops: label.hops + 1,
        first: label.first.or(Some(to)),
    })
}
