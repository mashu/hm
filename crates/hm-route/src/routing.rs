//! Route choice as a decision: the route that maximises expected utility.
//!
//! A route delivers with probability `P` (each hop's link completes, its
//! custodian accepts, intermediate custodians do their part, all estimated
//! from the station's beliefs), arrives at `a`, and costs airtime and money
//! on each hop it reaches. Its utility is
//!
//! ```text
//! U(r) = P · u(a) − C,     C = Σ_h P(reach hop h) · cost(bearer_h)
//! ```
//!
//! where `u` is the value of delivering at `a` relative to delivering now:
//! falling linearly to nothing at the message's expiry, or halving every
//! [`URGENT_HALF_LIFE`] for urgent traffic. Costs are in units of one
//! delivered message's value.
//!
//! The search is A* over partial routes (labels) with the bound
//! `P·u(arrival) − C`, which can only fall as a label is extended: the first
//! complete routes out of the queue are the best. Labels dominated in every
//! respect (less likely, later, dearer, more airtime, fewer stations still
//! open to them) by one already expanded at the same station through the
//! same first hop are dropped.
//!
//! A route that fails is not the end: the custodian tries another. A failed
//! first hop is known within the transfer; a loss further on only when the
//! custody suspect timer fires, much later. The final ranking counts the
//! first: `U(r) + (1 − p₁) · U(best other way still open once r's first hop
//! has failed)`, with `p₁` the first hop's chance. A cheap, likely radio hop
//! that has the internet to fall back on can beat going to the internet
//! straight away, and a slow sure route can beat a fast doubtful one that has
//! no fallback.
//!
//! Urgent traffic may go two ways at once when the time saved outweighs the
//! cost of the second copy (compared by utility, not by a threshold on the
//! gain in probability).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use hm_model::Estimate;
use hm_wire::Callsign;

use crate::{Bearer, ContactGraph, ContactKey, GraphError};

/// An urgent message loses half its value every this many seconds.
pub const URGENT_HALF_LIFE: u64 = 600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteHop {
    pub contact: ContactKey,
    pub depart: u64,
    pub arrive: u64,
    pub airtime_millis: u64,
    pub probability_permillion: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub hops: Vec<RouteHop>,
    pub arrival: u64,
    pub airtime_millis: u64,
    /// Chance the route delivers.
    pub success_probability: f64,
    /// Chance its first hop completes (the link, and the custodian accepting).
    pub first_hop_probability: f64,
    /// `−ln P`.
    pub risk_cost: f64,
    /// Expected attempt cost, in delivered messages' value.
    pub attempt_cost: f64,
    /// Expected utility, fallbacks counted.
    pub utility: f64,
}

impl Route {
    pub fn next_hop(&self) -> Option<Callsign> {
        self.hops.first().map(|hop| hop.contact.to)
    }

    pub fn edge_disjoint(&self, other: &Route) -> bool {
        self.hops.iter().all(|left| {
            other.hops.iter().all(|right| {
                (left.contact.from, left.contact.to, left.contact.bearer)
                    != (right.contact.from, right.contact.to, right.contact.bearer)
            })
        })
    }
}

#[derive(Clone, Debug)]
pub struct RouteRequest<'a> {
    pub source: Callsign,
    pub destination: Callsign,
    pub now: u64,
    pub expires_at: u64,
    pub object_bytes: u64,
    pub max_hops: u8,
    pub airtime_budget_millis: u64,
    pub visited: &'a [Callsign],
    pub excluded_contacts: &'a [ContactKey],
    /// Time-critical: value halves every [`URGENT_HALF_LIFE`], and two copies
    /// may go two ways at once.
    pub urgent: bool,
}

impl RouteRequest<'_> {
    /// Value of delivering at `t` relative to delivering now.
    pub fn value_at(&self, t: u64) -> f64 {
        let elapsed = t.saturating_sub(self.now) as f64;
        if self.urgent {
            (-elapsed / URGENT_HALF_LIFE as f64).exp2()
        } else {
            let life = self.expires_at.saturating_sub(self.now).max(1) as f64;
            (1.0 - elapsed / life).clamp(0.0, 1.0)
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RoutingPolicy {
    pub max_candidates: usize,
    pub max_alternatives: usize,
    pub max_labels: usize,
    /// Cost of an attempt on each bearer ([`Bearer::index`]), in delivered
    /// messages' value.
    pub attempt_cost: [f64; 3],
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            max_candidates: 32,
            max_alternatives: 3,
            max_labels: 16_384,
            attempt_cost: [0.01, 0.02, 0.015],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutePlan {
    /// One route normally, at most two edge-disjoint routes for urgent traffic.
    pub active: Vec<Route>,
    /// Ordered routes activated one at a time after a handoff failure.
    pub alternatives: Vec<Route>,
    /// Chance of delivery with the active routes and the best fallback.
    pub combined_success_probability: f64,
    pub expected_utility: f64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    InvalidRequest(&'static str),
    NoRoute,
    SearchLimit,
    Capacity,
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(formatter, "invalid route request: {message}"),
            Self::NoRoute => formatter.write_str("no feasible route"),
            Self::SearchLimit => formatter.write_str("contact graph search limit reached"),
            Self::Capacity => formatter.write_str("route capacity changed before reservation"),
        }
    }
}

impl std::error::Error for RouteError {}

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

pub fn plan_routes(
    graph: &ContactGraph,
    estimate: &mut dyn Estimate,
    request: &RouteRequest<'_>,
    policy: RoutingPolicy,
) -> Result<RoutePlan, RouteError> {
    validate_request(request, policy)?;
    let mut candidates = find_candidates(graph, estimate, request, policy)?;
    rank_with_fallbacks(graph, request, &mut candidates);
    let Some(primary) = candidates.first().cloned() else {
        return Err(RouteError::NoRoute);
    };
    let fallback = best_fallback(graph, request, &primary, &candidates);
    let mut active = vec![primary.clone()];
    let mut used = vec![0_usize];
    let mut combined = primary.success_probability
        + (1.0 - primary.first_hop_probability) * fallback.map_or(0.0, |f| f.success_probability);
    let mut expected_utility = primary.utility;
    if request.urgent {
        let p1 = primary.success_probability;
        let first = p1 * request.value_at(primary.arrival) - primary.attempt_cost;
        let best = candidates
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, route)| {
                primary.edge_disjoint(route)
                    && primary.airtime_millis.saturating_add(route.airtime_millis)
                        <= request.airtime_budget_millis
            })
            .map(|(index, route)| {
                let both = first + (1.0 - p1) * route.success_probability * request.value_at(route.arrival)
                    - route.attempt_cost;
                (index, route, both)
            })
            .max_by(|a, b| a.2.total_cmp(&b.2));
        if let Some((index, secondary, both)) = best {
            if both > primary.utility {
                combined = 1.0 - (1.0 - p1) * (1.0 - secondary.success_probability);
                expected_utility = both;
                active.push(secondary.clone());
                used.push(index);
            }
        }
    }
    let alternatives = candidates
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !used.contains(index))
        .map(|(_, route)| route)
        .take(policy.max_alternatives)
        .collect();
    Ok(RoutePlan {
        active,
        alternatives,
        combined_success_probability: combined.clamp(0.0, 1.0),
        expected_utility,
    })
}

pub fn reserve_active(
    graph: &mut ContactGraph,
    plan: &RoutePlan,
    object_bytes: u64,
) -> Result<(), RouteError> {
    let keys: Vec<ContactKey> = plan
        .active
        .iter()
        .flat_map(|route| route.hops.iter().map(|hop| hop.contact))
        .collect();
    graph
        .reserve_many(&keys, object_bytes)
        .map_err(|error| match error {
            GraphError::Capacity | GraphError::UnknownContact => RouteError::Capacity,
            GraphError::InvalidContact(_) => RouteError::InvalidRequest("invalid contact"),
        })
}

pub fn release_active(graph: &mut ContactGraph, plan: &RoutePlan, object_bytes: u64) {
    for route in &plan.active {
        let keys: Vec<ContactKey> = route.hops.iter().map(|hop| hop.contact).collect();
        graph.release_many(&keys, object_bytes);
    }
}

/// Utility of `route` on its own: `P·u(a) − C`.
fn own_utility(request: &RouteRequest<'_>, route: &Route, delay: u64) -> f64 {
    route.success_probability * request.value_at(route.arrival.saturating_add(delay)) - route.attempt_cost
}

/// The best other way once `route`'s first hop has failed: a route whose
/// first contact differs and is still open when `route`'s first hop would
/// have arrived, its arrival pushed back by the wait.
fn best_fallback<'r>(
    graph: &ContactGraph,
    request: &RouteRequest<'_>,
    route: &Route,
    candidates: &'r [Route],
) -> Option<&'r Route> {
    let first = route.hops.first()?;
    candidates
        .iter()
        .filter(|other| {
            other.hops.first().is_some_and(|hop| {
                hop.contact != first.contact
                    && graph
                        .contact(hop.contact)
                        .is_some_and(|contact| contact.end > first.arrive.max(hop.depart))
            })
        })
        .max_by(|a, b| {
            fallback_utility(request, first.arrive, a).total_cmp(&fallback_utility(request, first.arrive, b))
        })
}

fn fallback_utility(request: &RouteRequest<'_>, failed_at: u64, route: &Route) -> f64 {
    let delay = route
        .hops
        .first()
        .map_or(0, |hop| failed_at.saturating_sub(hop.depart));
    own_utility(request, route, delay).max(0.0)
}

/// Rank candidates by utility with the best fallback counted.
fn rank_with_fallbacks(graph: &ContactGraph, request: &RouteRequest<'_>, candidates: &mut [Route]) {
    let utilities: Vec<f64> = candidates
        .iter()
        .map(|route| {
            let fallback = best_fallback(graph, request, route, candidates).map_or(0.0, |other| {
                fallback_utility(
                    request,
                    route.hops.first().map_or(request.now, |h| h.arrive),
                    other,
                )
            });
            own_utility(request, route, 0) + (1.0 - route.first_hop_probability) * fallback
        })
        .collect();
    for (route, utility) in candidates.iter_mut().zip(utilities) {
        route.utility = utility;
    }
    candidates.sort_by(route_order);
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

/// Routes to the destination, best bound first. A* over labels, pruned by
/// dominance: a label is not expanded when an expanded one at the same
/// station, with the same first hop, dominates it. Keeping first hops apart
/// keeps alternatives through other neighbours for failover and urgent
/// copies. When the label budget runs out, the routes found so far are
/// returned; only a search that found none fails.
fn find_candidates(
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
    while let Some(label) = queue.pop() {
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
        let reach = (-label.risk_cost).exp();
        for contact in graph.outgoing(label.station, request.now) {
            if request.excluded_contacts.contains(&contact.key)
                || label.visited.contains(&contact.key.to)
                || contact.residual_capacity() < request.object_bytes
            {
                continue;
            }
            let depart = label
                .arrival
                .max(contact.start)
                .saturating_add(u64::from(contact.queue_delay_secs));
            if depart >= contact.end || depart >= contact.fresh_until {
                continue;
            }
            let transfer_secs = request
                .object_bytes
                .saturating_mul(8)
                .div_ceil(u64::from(contact.rate_bps))
                .max(1);
            let arrive = depart.saturating_add(transfer_secs);
            if arrive > contact.end || arrive > request.expires_at {
                continue;
            }
            let edge_airtime = match contact.key.bearer {
                Bearer::Radio | Bearer::Modem => request
                    .object_bytes
                    .saturating_mul(8_000)
                    .div_ceil(u64::from(contact.rate_bps)),
                Bearer::Internet => 0,
            };
            let airtime_millis = label.airtime_millis.saturating_add(edge_airtime);
            if airtime_millis > request.airtime_budget_millis {
                continue;
            }
            let to = contact.key.to;
            let handed = (estimate.link(contact.key.link(), depart, contact.stated())
                * estimate.accepts(to, arrive))
            .clamp(1.0e-9, 1.0);
            let probability = if to == request.destination {
                handed
            } else {
                (handed * estimate.delivers(to)).clamp(1.0e-9, 1.0)
            };
            let first_probability = if label.hops.is_empty() {
                handed
            } else {
                label.first_probability
            };
            let risk_cost = label.risk_cost - probability.ln();
            let attempt_cost = label.attempt_cost + reach * policy.attempt_cost[contact.key.bearer.index()];
            let mut hops = label.hops.clone();
            hops.push(RouteHop {
                contact: contact.key,
                depart,
                arrive,
                airtime_millis: edge_airtime,
                probability_permillion: (probability * 1_000_000.0).round() as u32,
            });
            let mut path = label.visited.clone();
            path.push(to);
            let next = Label {
                station: to,
                arrival: arrive,
                airtime_millis,
                risk_cost,
                attempt_cost,
                first_probability,
                bound: (-risk_cost).exp() * request.value_at(arrive) - attempt_cost,
                hops,
                visited: path,
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
    routes.dedup_by(|left, right| {
        left.hops
            .iter()
            .map(|hop| hop.contact)
            .eq(right.hops.iter().map(|hop| hop.contact))
    });
    Ok(routes)
}

fn validate_request(request: &RouteRequest<'_>, policy: RoutingPolicy) -> Result<(), RouteError> {
    if request.source == request.destination {
        return Err(RouteError::InvalidRequest("source equals destination"));
    }
    if request.now >= request.expires_at
        || request.object_bytes == 0
        || request.max_hops == 0
        || request.max_hops > 16
        || request.airtime_budget_millis == 0
        || request.visited.len() >= usize::from(request.max_hops)
    {
        return Err(RouteError::InvalidRequest("deadline, size, hops, or budget"));
    }
    if policy.max_candidates == 0
        || policy.max_alternatives == 0
        || policy.max_labels == 0
        || policy
            .attempt_cost
            .iter()
            .any(|cost| !cost.is_finite() || *cost < 0.0)
    {
        return Err(RouteError::InvalidRequest("routing policy"));
    }
    if request
        .visited
        .iter()
        .enumerate()
        .any(|(index, station)| request.visited[..index].contains(station))
    {
        return Err(RouteError::InvalidRequest("duplicate visited station"));
    }
    Ok(())
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

/// Best first: higher utility, then earlier, less airtime, fewer hops.
fn route_order(left: &Route, right: &Route) -> Ordering {
    right
        .utility
        .total_cmp(&left.utility)
        .then(left.arrival.cmp(&right.arrival))
        .then(left.airtime_millis.cmp(&right.airtime_millis))
        .then(left.hops.len().cmp(&right.hops.len()))
        .then_with(|| {
            left.hops
                .iter()
                .map(|hop| hop.contact)
                .cmp(right.hops.iter().map(|hop| hop.contact))
        })
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

#[cfg(test)]
mod tests;
