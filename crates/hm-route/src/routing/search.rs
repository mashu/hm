//! A* over partial routes: labels, their expansion by contacts and by
//! forecast departures on known links, and dominance.

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

#[derive(Clone, Debug)]
struct Label {
    station: Callsign,
    arrival: u64,
    airtime_millis: u64,
    risk_cost: f64,
    attempt_cost: f64,
    first_probability: f64,
    /// Upper bound on the utility of any route through this label.
    bound: f64,
    hops: Vec<RouteHop>,
    visited: Vec<Callsign>,
}

impl PartialEq for Label {
    fn eq(&self, other: &Self) -> bool {
        label_order(self, other) == Ordering::Equal
    }
}

impl Eq for Label {}

impl PartialOrd for Label {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Label {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap pops the greatest: the best label is the greatest.
        label_order(other, self)
    }
}

/// What a label that was expanded left for later labels to be compared with.
struct Expanded {
    risk_cost: f64,
    attempt_cost: f64,
    arrival: u64,
    airtime_millis: u64,
    visited: Vec<Callsign>,
}

impl Expanded {
    /// Whatever `label` could still reach, this one reached no later, with no
    /// more risk, cost or airtime, and with no more stations ruled out on the
    /// way: every route through `label` has one at least as good through this.
    fn dominates(&self, label: &Label) -> bool {
        self.risk_cost <= label.risk_cost
            && self.attempt_cost <= label.attempt_cost
            && self.arrival <= label.arrival
            && self.airtime_millis <= label.airtime_millis
            && self.visited.iter().all(|station| label.visited.contains(station))
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
    let mut visited = request.visited.to_vec();
    if visited.contains(&request.source) {
        return Err(RouteError::InvalidRequest("source already visited"));
    }
    visited.push(request.source);
    let mut queue = BinaryHeap::new();
    queue.push(Label {
        station: request.source,
        arrival: request.now,
        airtime_millis: 0,
        risk_cost: 0.0,
        attempt_cost: 0.0,
        first_probability: 1.0,
        bound: 1.0,
        hops: Vec::new(),
        visited,
    });
    let mut routes = Vec::new();
    let mut expanded: BTreeMap<(Callsign, Callsign), Vec<Expanded>> = BTreeMap::new();
    let mut examined = 0_usize;
    let mut unprofitable = false;
    let mut steps = Vec::new();
    while let Some(label) = queue.pop() {
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
            routes.push(route_from_label(label));
            if routes.len() == policy.max_candidates {
                break;
            }
            continue;
        }
        if request.visited.len() + label.hops.len() >= usize::from(request.max_hops) {
            continue;
        }
        if let Some(first) = label.hops.first() {
            let seen = expanded.entry((label.station, first.contact.to)).or_default();
            if seen.iter().any(|e| e.dominates(&label)) {
                continue;
            }
            seen.push(Expanded {
                risk_cost: label.risk_cost,
                attempt_cost: label.attempt_cost,
                arrival: label.arrival,
                airtime_millis: label.airtime_millis,
                visited: label.visited.clone(),
            });
        }
        steps.clear();
        contact_steps(graph, estimate, request, &label, &mut steps);
        forecast_steps(graph, estimate, request, policy, &label, &mut steps);
        for step in steps.drain(..) {
            let Some(next) = extend(estimate, request, policy, &label, step) else {
                continue;
            };
            let first = next.hops[0].contact.to;
            let dominated = expanded
                .get(&(next.station, first))
                .is_some_and(|seen| seen.iter().any(|e| e.dominates(&next)));
            if !dominated {
                queue.push(next);
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
    let at_source = label.hops.is_empty();
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
    request: &RouteRequest<'_>,
    label: &Label,
    steps: &mut Vec<Step>,
) {
    for contact in graph.outgoing(label.station, request.now) {
        if label.visited.contains(&contact.key.to) || contact.residual_capacity() < request.object_bytes {
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
    request: &RouteRequest<'_>,
    policy: RoutingPolicy,
    label: &Label,
    steps: &mut Vec<Step>,
) {
    if policy.forecast_horizon_secs == 0 {
        return;
    }
    let horizon = request
        .expires_at
        .min(request.now.saturating_add(policy.forecast_horizon_secs));
    let step = policy.forecast_step_secs;
    for (to, bearer, known) in graph.links_from(label.station) {
        if label.visited.contains(&to) {
            continue;
        }
        let link = LinkKey {
            from: label.station,
            to,
            bearer,
        };
        let transfer = transfer_secs(request.object_bytes, known.rate_bps);
        let later = (label.arrival / step + 1..).map(|k| k * step);
        let mut best = 0.0;
        for depart in std::iter::once(label.arrival).chain(later) {
            if depart >= horizon {
                break;
            }
            let arrive = depart.saturating_add(transfer);
            if arrive > request.expires_at {
                break;
            }
            if !allowed(request, label, link, depart) {
                continue;
            }
            let handed = estimate.link(link, depart, None) * estimate.accepts(to, arrive);
            if handed <= best * FORECAST_GAIN {
                continue;
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
                arrive,
                rate_bps: known.rate_bps,
                handed,
                forecast: true,
            });
        }
    }
}

/// The label one step on, if it fits the deadline and the airtime budget.
fn extend(
    estimate: &mut dyn Estimate,
    request: &RouteRequest<'_>,
    policy: RoutingPolicy,
    label: &Label,
    step: Step,
) -> Option<Label> {
    if step.arrive > request.expires_at {
        return None;
    }
    let bearer = step.key.bearer;
    let rate = f64::from(step.rate_bps.max(1));
    let edge_airtime = match bearer {
        Bearer::Radio | Bearer::Modem => request
            .object_bytes
            .saturating_mul(8_000)
            .div_ceil(u64::from(step.rate_bps.max(1))),
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
        (request.object_bytes + ATTEMPT_OVERHEAD_BYTES) as f64 * 8.0 / rate
    } else {
        0.0
    };
    let hop_cost = policy.attempt_cost[bearer] + policy.airtime_price[bearer] * attempt_airtime;
    let risk_cost = label.risk_cost - probability.ln();
    let attempt_cost = label.attempt_cost + reach * hop_cost;
    let mut hops = label.hops.clone();
    hops.push(RouteHop {
        contact: step.key,
        depart: step.depart,
        arrive: step.arrive,
        airtime_millis: edge_airtime,
        probability_permillion: (probability * 1_000_000.0).round() as u32,
        forecast: step.forecast,
    });
    let mut visited = label.visited.clone();
    visited.push(to);
    Some(Label {
        station: to,
        arrival: step.arrive,
        airtime_millis,
        risk_cost,
        attempt_cost,
        first_probability: if label.hops.is_empty() {
            handed
        } else {
            label.first_probability
        },
        bound: (-risk_cost).exp() * request.value_at(step.arrive) - attempt_cost,
        hops,
        visited,
    })
}

fn route_from_label(label: Label) -> Route {
    Route {
        success_probability: (-label.risk_cost).exp(),
        first_hop_probability: label.first_probability,
        risk_cost: label.risk_cost,
        attempt_cost: label.attempt_cost,
        utility: label.bound,
        arrival: label.arrival,
        airtime_millis: label.airtime_millis,
        hops: label.hops,
    }
}

/// Best first: higher bound, then earlier, less airtime, fewer hops.
fn label_order(left: &Label, right: &Label) -> Ordering {
    right
        .bound
        .total_cmp(&left.bound)
        .then(left.arrival.cmp(&right.arrival))
        .then(left.airtime_millis.cmp(&right.airtime_millis))
        .then(left.hops.len().cmp(&right.hops.len()))
        .then(left.station.cmp(&right.station))
}
