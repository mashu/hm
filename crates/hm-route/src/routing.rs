use std::cmp::Ordering;
use std::collections::BinaryHeap;

use hm_wire::Callsign;

use crate::{Bearer, ContactGraph, ContactKey, GraphError};

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
    pub success_probability: f64,
    pub risk_cost: f64,
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
    pub urgent: bool,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RoutingPolicy {
    pub max_candidates: usize,
    pub max_alternatives: usize,
    pub max_labels: usize,
    pub urgent_min_gain: f64,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            max_candidates: 32,
            max_alternatives: 3,
            max_labels: 16_384,
            urgent_min_gain: 0.05,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutePlan {
    /// One route normally, at most two edge-disjoint routes for urgent traffic.
    pub active: Vec<Route>,
    /// Ordered routes activated one at a time after a handoff failure.
    pub alternatives: Vec<Route>,
    pub combined_success_probability: f64,
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
        // Reverse the natural route order: BinaryHeap pops the best label.
        label_order(other, self)
    }
}

pub fn plan_routes(
    graph: &ContactGraph,
    request: &RouteRequest<'_>,
    policy: RoutingPolicy,
) -> Result<RoutePlan, RouteError> {
    validate_request(request, policy)?;
    let candidates = find_candidates(graph, request, policy)?;
    let Some(primary) = candidates.first().cloned() else {
        return Err(RouteError::NoRoute);
    };
    let mut active = vec![primary.clone()];
    let mut used = vec![0_usize];
    let mut combined = primary.success_probability;
    if request.urgent {
        if let Some((index, secondary)) = candidates.iter().enumerate().skip(1).find(|(_, route)| {
            if !primary.edge_disjoint(route)
                || primary.airtime_millis.saturating_add(route.airtime_millis) > request.airtime_budget_millis
            {
                return false;
            }
            let probability = 1.0 - (1.0 - primary.success_probability) * (1.0 - route.success_probability);
            probability - primary.success_probability >= policy.urgent_min_gain
        }) {
            combined = 1.0 - (1.0 - primary.success_probability) * (1.0 - secondary.success_probability);
            active.push(secondary.clone());
            used.push(index);
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
        combined_success_probability: combined,
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

fn find_candidates(
    graph: &ContactGraph,
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
        hops: Vec::new(),
        visited,
    });
    let mut routes = Vec::new();
    let mut examined = 0_usize;
    while let Some(label) = queue.pop() {
        examined += 1;
        if examined > policy.max_labels {
            return Err(RouteError::SearchLimit);
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
            let probability = graph.conservative_probability(contact, depart);
            let mut hops = label.hops.clone();
            hops.push(RouteHop {
                contact: contact.key,
                depart,
                arrive,
                airtime_millis: edge_airtime,
                probability_permillion: (probability * 1_000_000.0).round() as u32,
            });
            let mut path = label.visited.clone();
            path.push(contact.key.to);
            queue.push(Label {
                station: contact.key.to,
                arrival: arrive,
                airtime_millis,
                risk_cost: label.risk_cost - probability.ln(),
                hops,
                visited: path,
            });
        }
    }
    routes.sort_by(route_order);
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
        || !(0.0..1.0).contains(&policy.urgent_min_gain)
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
        risk_cost: label.risk_cost,
        arrival: label.arrival,
        airtime_millis: label.airtime_millis,
        hops: label.hops,
    }
}

fn route_order(left: &Route, right: &Route) -> Ordering {
    left.risk_cost
        .total_cmp(&right.risk_cost)
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

fn label_order(left: &Label, right: &Label) -> Ordering {
    left.risk_cost
        .total_cmp(&right.risk_cost)
        .then(left.arrival.cmp(&right.arrival))
        .then(left.airtime_millis.cmp(&right.airtime_millis))
        .then(left.hops.len().cmp(&right.hops.len()))
        .then(left.station.cmp(&right.station))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphConfig, ScheduledContact};

    fn call(value: &str) -> Callsign {
        value.parse().unwrap()
    }

    fn add(
        graph: &mut ContactGraph,
        from: &str,
        to: &str,
        bearer: Bearer,
        spec: (u64, u64, u64, u16),
    ) -> ContactKey {
        let (start, end, capacity, probability) = spec;
        graph
            .add_schedule(ScheduledContact {
                from: call(from),
                to: call(to),
                bearer,
                start,
                end,
                rate_bps: 8_000,
                capacity_bytes: capacity,
                success_permyriad: Some(probability),
                flags: 0,
            })
            .unwrap()
    }

    fn request<'a>(visited: &'a [Callsign], excluded: &'a [ContactKey]) -> RouteRequest<'a> {
        RouteRequest {
            source: call("M0AAA"),
            destination: call("M0DDD"),
            now: 0,
            expires_at: 1_000,
            object_bytes: 1_000,
            max_hops: 8,
            airtime_budget_millis: 10_000,
            visited,
            excluded_contacts: excluded,
            urgent: false,
        }
    }

    #[test]
    fn chooses_probability_then_eta_and_keeps_failovers() {
        let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
        add(
            &mut graph,
            "M0AAA",
            "M0BBB",
            Bearer::Internet,
            (0, 500, 5_000, 10_000),
        );
        add(
            &mut graph,
            "M0BBB",
            "M0DDD",
            Bearer::Internet,
            (0, 500, 5_000, 10_000),
        );
        add(
            &mut graph,
            "M0AAA",
            "M0CCC",
            Bearer::Internet,
            (0, 300, 5_000, 7_000),
        );
        add(
            &mut graph,
            "M0CCC",
            "M0DDD",
            Bearer::Internet,
            (0, 300, 5_000, 7_000),
        );
        add(
            &mut graph,
            "M0AAA",
            "M0DDD",
            Bearer::Internet,
            (100, 500, 5_000, 0),
        );
        let plan = plan_routes(&graph, &request(&[], &[]), RoutingPolicy::default()).unwrap();
        assert_eq!(plan.active.len(), 1);
        assert_eq!(plan.active[0].next_hop(), Some(call("M0BBB")));
        assert_eq!(plan.alternatives.len(), 2);
    }

    #[test]
    fn rejects_capacity_deadline_airtime_loops_and_hop_limit() {
        let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
        add(&mut graph, "M0AAA", "M0BBB", Bearer::Radio, (0, 100, 999, 9_000));
        add(
            &mut graph,
            "M0BBB",
            "M0DDD",
            Bearer::Radio,
            (0, 100, 10_000, 9_000),
        );
        assert_eq!(
            plan_routes(&graph, &request(&[], &[]), RoutingPolicy::default()),
            Err(RouteError::NoRoute)
        );

        let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
        add(
            &mut graph,
            "M0AAA",
            "M0BBB",
            Bearer::Internet,
            (0, 100, 10_000, 9_000),
        );
        add(
            &mut graph,
            "M0BBB",
            "M0AAA",
            Bearer::Internet,
            (0, 100, 10_000, 9_000),
        );
        add(
            &mut graph,
            "M0BBB",
            "M0DDD",
            Bearer::Internet,
            (200, 201, 10_000, 9_000),
        );
        let mut deadline = request(&[], &[]);
        deadline.expires_at = 199;
        assert_eq!(
            plan_routes(&graph, &deadline, RoutingPolicy::default()),
            Err(RouteError::NoRoute)
        );
    }

    #[test]
    fn urgent_replication_is_two_edge_disjoint_copies_only() {
        let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
        for middle in ["M0BBB", "M0CCC", "M0EEE"] {
            add(
                &mut graph,
                "M0AAA",
                middle,
                Bearer::Internet,
                (0, 500, 5_000, 7_000),
            );
            add(
                &mut graph,
                middle,
                "M0DDD",
                Bearer::Internet,
                (0, 500, 5_000, 7_000),
            );
        }
        let mut request = request(&[], &[]);
        request.urgent = true;
        let plan = plan_routes(
            &graph,
            &request,
            RoutingPolicy {
                urgent_min_gain: 0.01,
                ..RoutingPolicy::default()
            },
        )
        .unwrap();
        assert_eq!(plan.active.len(), 2);
        assert!(plan.active[0].edge_disjoint(&plan.active[1]));
        assert!(plan.combined_success_probability > plan.active[0].success_probability);
    }

    #[test]
    fn reservation_is_all_or_nothing() {
        let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
        let first = add(
            &mut graph,
            "M0AAA",
            "M0BBB",
            Bearer::Internet,
            (0, 500, 1_000, 9_000),
        );
        let second = add(
            &mut graph,
            "M0BBB",
            "M0DDD",
            Bearer::Internet,
            (0, 500, 1_000, 9_000),
        );
        let plan = plan_routes(&graph, &request(&[], &[]), RoutingPolicy::default()).unwrap();
        reserve_active(&mut graph, &plan, 1_000).unwrap();
        assert_eq!(graph.contact(first).unwrap().residual_capacity(), 0);
        assert_eq!(graph.contact(second).unwrap().residual_capacity(), 0);
        release_active(&mut graph, &plan, 1_000);
        assert_eq!(graph.contact(first).unwrap().residual_capacity(), 1_000);
        assert_eq!(graph.contact(second).unwrap().residual_capacity(), 1_000);
    }
}
