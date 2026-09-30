//! Route choice as a decision: the route that maximises expected utility.
//!
//! A route delivers with probability `P` (each hop's link completes, its
//! custodian accepts, intermediate custodians do their part, all estimated
//! from the station's beliefs), arrives at `a`, and costs airtime and money
//! on each hop it reaches. Its utility is
//!
//! ```text
//! U(r) = P · u(a) − C,     C = Σ_h P(reach hop h) · cost_h
//! cost_h = attempt(bearer) + price(bearer) · airtime_h
//! ```
//!
//! where `u` is the value of delivering at `a` relative to delivering now:
//! falling linearly to nothing at the message's expiry, or halving every
//! [`URGENT_HALF_LIFE`] for urgent traffic. Costs are in units of one
//! delivered message's value; airtime is priced per second, so a slow radio
//! link, or one the others keep busy, costs more to try.
//!
//! **When** is part of the choice. Besides the contacts the graph holds as
//! windows, every link the station has seen can be taken later: departures
//! every [`RoutingPolicy::forecast_step_secs`] up to the horizon, each with
//! the chance the beliefs forecast for that moment. A doubtful link now, or
//! a likely one in the morning through a relay: the one of greater expected
//! utility wins, and a route that departs later says to wait. Only
//! departures likelier than every earlier one on the same link are tried
//! (the upper envelope): a custodian can always hold, so arriving earlier
//! with a better chance dominates.
//!
//! The search is A* over partial routes (labels) with the bound
//! `P·u(arrival) − C`, which can only fall as a label is extended: the first
//! complete routes out of the queue are the best. Labels dominated in every
//! respect (less likely, later, dearer, more airtime, fewer stations still
//! open to them) by one already expanded at the same station through the
//! same first hop are dropped.
//!
//! Holding the message is always possible and worth nothing: a route is
//! taken only when its utility is above zero. A link so unlikely to open
//! that its airtime costs more than the chance is worth is not tried, and the
//! message waits for a better time or a better route
//! ([`RouteError::NotWorthIt`]).
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

mod rank;
mod search;

use hm_model::{Estimate, LinkKey, PerBearer};
use hm_wire::Callsign;

use crate::{ContactGraph, ContactKey};

/// An urgent message loses half its value every this many seconds.
pub const URGENT_HALF_LIFE: u64 = 600;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RouteHop {
    pub contact: ContactKey,
    pub depart: u64,
    pub arrive: u64,
    pub airtime_millis: u64,
    pub probability_permillion: u32,
    /// A departure the beliefs forecast on a known link, not a contact the
    /// graph holds: nothing to reserve.
    pub forecast: bool,
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

    /// When the route leaves: now, or later when it waits for its first link.
    pub fn departs(&self) -> Option<u64> {
        self.hops.first().map(|hop| hop.depart)
    }

    /// The route's chance beyond its first hop.
    pub fn downstream_probability(&self) -> f64 {
        self.hops
            .iter()
            .skip(1)
            .map(|hop| f64::from(hop.probability_permillion) / 1_000_000.0)
            .product()
    }

    /// The contacts the route holds room on (forecast departures hold none).
    pub fn contacts(&self) -> Vec<ContactKey> {
        self.hops
            .iter()
            .filter(|hop| !hop.forecast)
            .map(|hop| hop.contact)
            .collect()
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
    /// Bearers the source may not use for this message at all (its origin
    /// may not use this station's airtime).
    pub forbidden: PerBearer<bool>,
    /// Links from the source that cannot carry anything now (the radio is
    /// down, the internet peer not linked). Later departures on them may
    /// still be planned.
    pub closed_now: &'a [LinkKey],
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
    /// Cost of an attempt, in delivered messages' value.
    pub attempt_cost: PerBearer<f64>,
    /// Cost of a second of airtime, in delivered messages' value.
    pub airtime_price: PerBearer<f64>,
    /// Known links are planned at departures this far apart.
    pub forecast_step_secs: u64,
    /// How far ahead known links are planned; zero plans only the contacts
    /// the graph holds.
    pub forecast_horizon_secs: u64,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            max_candidates: 32,
            max_alternatives: 3,
            max_labels: 16_384,
            // An internet attempt 2 % of a delivered message's value, a
            // minute on the air 5 %: a station's defaults.
            attempt_cost: PerBearer([0.0, 0.02, 0.0]),
            airtime_price: PerBearer([0.05 / 60.0, 0.0, 0.05 / 60.0]),
            forecast_step_secs: 15 * 60,
            forecast_horizon_secs: 36 * 3_600,
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
    /// Routes exist, but none is worth its cost now: holding is better.
    NotWorthIt,
    SearchLimit,
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(formatter, "invalid route request: {message}"),
            Self::NoRoute => formatter.write_str("no feasible route"),
            Self::NotWorthIt => formatter.write_str("no route worth its cost now"),
            Self::SearchLimit => formatter.write_str("contact graph search limit reached"),
        }
    }
}

impl std::error::Error for RouteError {}

pub fn plan_routes(
    graph: &ContactGraph,
    estimate: &mut dyn Estimate,
    request: &RouteRequest<'_>,
    policy: RoutingPolicy,
) -> Result<RoutePlan, RouteError> {
    validate_request(request, policy)?;
    let mut candidates = search::find_candidates(graph, estimate, request, policy)?;
    rank::rank_with_fallbacks(graph, request, &mut candidates);
    let Some(primary) = candidates.first().cloned() else {
        return Err(RouteError::NoRoute);
    };
    let fallback = rank::best_fallback(graph, request, &primary, &candidates);
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
    let priced = |cost: &f64| cost.is_finite() && *cost >= 0.0;
    if policy.max_candidates == 0
        || policy.max_alternatives == 0
        || policy.max_labels == 0
        || (policy.forecast_horizon_secs > 0 && policy.forecast_step_secs == 0)
        || !policy.attempt_cost.0.iter().all(priced)
        || !policy.airtime_price.0.iter().all(priced)
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

#[cfg(test)]
mod tests;
