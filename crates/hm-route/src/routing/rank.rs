//! Ranking candidate routes with the best fallback counted.

use std::cmp::Ordering;

use super::{Route, RouteRequest};
use crate::ContactGraph;

/// Utility of `route` on its own: `P·u(a) − C`.
fn own_utility(request: &RouteRequest<'_>, route: &Route, delay: u64) -> f64 {
    route.success_probability * request.value_at(route.arrival.saturating_add(delay)) - route.attempt_cost
}

/// The best other way once `route`'s first hop has failed: a route whose
/// first hop differs and can still be taken when `route`'s first hop would
/// have arrived (a forecast departure always can, later), its arrival
/// pushed back by the wait.
pub(super) fn best_fallback<'r>(
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
                    && (hop.forecast
                        || graph
                            .contact(hop.contact)
                            .is_some_and(|contact| contact.end > first.arrive.max(hop.depart)))
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
pub(super) fn rank_with_fallbacks(
    graph: &ContactGraph,
    request: &RouteRequest<'_>,
    candidates: &mut [Route],
) {
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
