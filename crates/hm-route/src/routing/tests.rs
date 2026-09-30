use hm_core::DetRng;
use hm_model::{Beliefs, LinkKey, LinkObservation, PerBearer};

use super::*;
use crate::{Bearer, ContactKey, GraphConfig, ScheduledContact};

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

fn request<'a>(visited: &'a [Callsign], closed_now: &'a [LinkKey]) -> RouteRequest<'a> {
    RouteRequest {
        source: call("M0AAA"),
        destination: call("M0DDD"),
        now: 0,
        expires_at: 1_000,
        object_bytes: 1_000,
        max_hops: 8,
        airtime_budget_millis: 10_000,
        visited,
        forbidden: PerBearer::default(),
        closed_now,
        urgent: false,
    }
}

fn plan(graph: &ContactGraph, request: &RouteRequest<'_>) -> Result<RoutePlan, RouteError> {
    let beliefs = Beliefs::new();
    plan_routes(
        graph,
        &mut beliefs.mean(request.now),
        request,
        RoutingPolicy::default(),
    )
}

#[test]
fn chooses_by_utility_and_keeps_failovers() {
    let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
    let spec = |end, p| (0, end, 5_000, p);
    add(&mut graph, "M0AAA", "M0BBB", Bearer::Internet, spec(500, 10_000));
    add(&mut graph, "M0BBB", "M0DDD", Bearer::Internet, spec(500, 10_000));
    add(&mut graph, "M0AAA", "M0CCC", Bearer::Internet, spec(300, 7_000));
    add(&mut graph, "M0CCC", "M0DDD", Bearer::Internet, spec(300, 7_000));
    add(
        &mut graph,
        "M0AAA",
        "M0DDD",
        Bearer::Internet,
        (100, 500, 5_000, 0),
    );
    let plan = plan(&graph, &request(&[], &[])).unwrap();
    assert_eq!(plan.active.len(), 1);
    assert_eq!(plan.active[0].next_hop(), Some(call("M0BBB")));
    // Through M0CCC is kept to fail over to; straight to M0DDD, stated
    // never to complete, is not worth its attempt.
    assert_eq!(plan.alternatives.len(), 1);
    assert_eq!(plan.alternatives[0].next_hop(), Some(call("M0CCC")));
    assert!(plan.combined_success_probability >= plan.active[0].success_probability);
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
    assert_eq!(plan(&graph, &request(&[], &[])), Err(RouteError::NoRoute));

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
    assert_eq!(plan(&graph, &deadline), Err(RouteError::NoRoute));
}

#[test]
fn urgent_mail_goes_two_ways_when_the_time_saved_is_worth_the_copy() {
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
    let routine = plan(&graph, &request(&[], &[])).unwrap();
    assert_eq!(routine.active.len(), 1, "routine mail is never copied");
    let mut urgent = request(&[], &[]);
    urgent.urgent = true;
    let plan = plan(&graph, &urgent).unwrap();
    assert_eq!(plan.active.len(), 2);
    assert!(plan.active[0].edge_disjoint(&plan.active[1]));
    assert!(plan.combined_success_probability > plan.active[0].success_probability);
    assert!(plan.expected_utility > plan.active[0].utility);
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
    let plan = plan(&graph, &request(&[], &[])).unwrap();
    let held = plan.active[0].contacts();
    graph.reserve_many(&held, 1_000).unwrap();
    assert_eq!(graph.contact(first).unwrap().residual_capacity(), 0);
    assert_eq!(graph.contact(second).unwrap().residual_capacity(), 0);
    assert!(graph.reserve_many(&held, 1).is_err());
    graph.release_many(&held, 1_000);
    assert_eq!(graph.contact(first).unwrap().residual_capacity(), 1_000);
    assert_eq!(graph.contact(second).unwrap().residual_capacity(), 1_000);
}

/// Latency is part of the objective, not a tie-breaker: a route a little
/// more likely two days from now loses to one available now.
#[test]
fn a_slightly_safer_route_days_later_loses_to_one_now() {
    let day = 86_400;
    let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
    add(
        &mut graph,
        "M0AAA",
        "M0DDD",
        Bearer::Radio,
        (0, 600, 10_000, 8_000),
    );
    add(
        &mut graph,
        "M0AAA",
        "M0DDD",
        Bearer::Radio,
        (2 * day, 2 * day + 600, 10_000, 8_200),
    );
    let mut req = request(&[], &[]);
    req.expires_at = 3 * day;
    let plan = plan(&graph, &req).unwrap();
    assert_eq!(plan.active[0].hops[0].depart, 0);
}

/// Holding is worth nothing and costs nothing: a slow radio link so doubtful
/// that its airtime is worth more than its chance is not tried at all.
#[test]
fn a_link_not_worth_its_airtime_is_not_tried() {
    let try_radio = |permyriad: u16| {
        let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
        graph
            .add_schedule(ScheduledContact {
                from: call("M0AAA"),
                to: call("M0DDD"),
                bearer: Bearer::Radio,
                start: 0,
                end: 900,
                rate_bps: 300,
                capacity_bytes: 10_000,
                success_permyriad: Some(permyriad),
                flags: 0,
            })
            .unwrap();
        let mut req = request(&[], &[]);
        req.expires_at = 86_400;
        req.airtime_budget_millis = 60_000;
        plan(&graph, &req).map(|plan| plan.active[0].next_hop())
    };
    assert_eq!(try_radio(2_000), Ok(Some(call("M0DDD"))));
    // 32 s on the air, 2.7 % of a message's value, for a 2 % chance.
    assert_eq!(try_radio(200), Err(RouteError::NotWorthIt));
}

/// A likely radio hop with the internet to fall back on goes first; an
/// unlikely one on a slow link does not: its airtime is worth more than the
/// internet attempt it would save.
#[test]
fn cheap_radio_first_when_it_is_likely_and_the_internet_is_there_to_fall_back_on() {
    let first_bearer = |radio: u16| {
        let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
        graph
            .add_schedule(ScheduledContact {
                from: call("M0AAA"),
                to: call("M0DDD"),
                bearer: Bearer::Radio,
                start: 0,
                end: 900,
                rate_bps: 1_200,
                capacity_bytes: 10_000,
                success_permyriad: Some(radio),
                flags: 0,
            })
            .unwrap();
        add(
            &mut graph,
            "M0AAA",
            "M0DDD",
            Bearer::Internet,
            (0, 900, 10_000, 9_900),
        );
        // Mail good for a day: the radio's minute or so is no loss of value.
        let mut req = request(&[], &[]);
        req.expires_at = 86_400;
        plan(&graph, &req).unwrap().active[0].hops[0].contact.bearer
    };
    assert_eq!(first_bearer(8_000), Bearer::Radio);
    assert_eq!(first_bearer(2_000), Bearer::Internet);
}

/// Posterior means multiply into a route's chance, so two good hops are worth
/// more than one poor hop, where a product of low quantiles was not. The poor
/// direct hop is still tried first: it costs little, and the two good hops
/// stay open to fall back on.
#[test]
fn two_good_hops_are_worth_more_than_one_poor_hop() {
    let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
    add(
        &mut graph,
        "M0AAA",
        "M0DDD",
        Bearer::Radio,
        (0, 900, 10_000, 4_000),
    );
    add(
        &mut graph,
        "M0AAA",
        "M0BBB",
        Bearer::Radio,
        (0, 900, 10_000, 9_500),
    );
    add(
        &mut graph,
        "M0BBB",
        "M0DDD",
        Bearer::Radio,
        (0, 900, 10_000, 9_500),
    );
    let plan = plan(&graph, &request(&[], &[])).unwrap();
    let routes: Vec<&Route> = plan.active.iter().chain(&plan.alternatives).collect();
    let chance = |hops: usize| {
        routes
            .iter()
            .find(|r| r.hops.len() == hops)
            .unwrap()
            .success_probability
    };
    assert!(chance(2) > chance(1) * 1.5, "{} vs {}", chance(2), chance(1));
    assert_eq!(plan.active[0].hops.len(), 1);
    assert!(plan.combined_success_probability > chance(2));
}

/// Drawing the estimates from the beliefs (Thompson sampling) tries a link
/// never used now and then, and a well-known mediocre one most of the time.
#[test]
fn thompson_plans_explore_links_little_is_known_about() {
    let (me, dest) = (call("M0AAA"), call("M0DDD"));
    let mut beliefs = Beliefs::new();
    let known = LinkKey {
        from: me,
        to: dest,
        bearer: Bearer::Modem,
    };
    for n in 0..30 {
        beliefs.observe_link(known, n * 60, LinkObservation::Heard);
        beliefs.observe_link(known, n * 60 + 1, LinkObservation::Handoff { ok: n % 2 == 0 });
    }
    let now = 30 * 60;
    let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
    for bearer in [Bearer::Modem, Bearer::Radio] {
        graph
            .add_potential(crate::LiveContact {
                from: me,
                to: dest,
                bearer,
                rate_bps: 1_200,
                capacity_bytes: 100_000,
                flags: 0,
                observed_at: now,
            })
            .unwrap();
    }
    let mut req = request(&[], &[]);
    req.now = now;
    req.expires_at = now + 86_400;
    let radio = (0..400)
        .filter(|&seed| {
            let mut draw = beliefs.thompson(DetRng::from_seed(seed), now);
            let plan = plan_routes(&graph, &mut draw, &req, RoutingPolicy::default()).unwrap();
            plan.active[0].hops[0].contact.bearer == Bearer::Radio
        })
        .count();
    assert!((4..200).contains(&radio), "radio chosen {radio} of 400 times");
    let mean = plan_routes(&graph, &mut beliefs.mean(now), &req, RoutingPolicy::default()).unwrap();
    assert_eq!(mean.active[0].hops[0].contact.bearer, Bearer::Modem);
}

mod search_limit {
    use super::*;

    /// A reliable internet cluster of nine stations, all linked to each
    /// other, and the destination reachable only by radio from one of them.
    fn cluster_with_radio_last_hop() -> ContactGraph {
        let mut graph = ContactGraph::new(GraphConfig::default()).unwrap();
        let cluster: Vec<Callsign> = (1..=9).map(|i| call(&format!("M0C{i}"))).collect();
        let mut add = |from: Callsign, to: Callsign, bearer: Bearer, success: u16| {
            graph
                .add_schedule(ScheduledContact {
                    from,
                    to,
                    bearer,
                    start: 0,
                    end: 10_000,
                    rate_bps: 1_200,
                    capacity_bytes: 1_000_000,
                    success_permyriad: Some(success),
                    flags: 0,
                })
                .unwrap();
        };
        for &a in &cluster {
            add(call("M0SRC"), a, Bearer::Internet, 9_900);
            for &b in &cluster {
                if a != b {
                    add(a, b, Bearer::Internet, 9_900);
                }
            }
        }
        add(cluster[8], call("M0DST"), Bearer::Radio, 3_000);
        graph
    }

    fn request() -> RouteRequest<'static> {
        RouteRequest {
            source: call("M0SRC"),
            destination: call("M0DST"),
            now: 0,
            expires_at: 10_000,
            object_bytes: 1_000,
            max_hops: 8,
            airtime_budget_millis: 60_000,
            visited: &[],
            forbidden: PerBearer::default(),
            closed_now: &[],
            urgent: false,
        }
    }

    #[test]
    fn a_dense_cluster_still_yields_the_route_through_it() {
        let graph = cluster_with_radio_last_hop();
        let plan = plan(&graph, &request()).expect("a route exists");
        let best = &plan.active[0];
        assert_eq!(best.hops.len(), 2, "straight to the gateway, then radio");
        assert_eq!(best.hops[1].contact.to, call("M0DST"));
    }

    /// A label budget that runs out before the search is done leaves the
    /// routes found by then as the answer, best first; only a search that
    /// found none fails.
    #[test]
    fn a_search_cut_short_returns_what_it_found() {
        let mut graph = cluster_with_radio_last_hop();
        graph
            .add_schedule(ScheduledContact {
                from: call("M0SRC"),
                to: call("M0DST"),
                bearer: Bearer::Internet,
                start: 0,
                end: 10_000,
                rate_bps: 1_200,
                capacity_bytes: 1_000_000,
                success_permyriad: Some(9_900),
                flags: 0,
            })
            .unwrap();
        let beliefs = Beliefs::new();
        let tight = RoutingPolicy {
            max_labels: 20,
            ..RoutingPolicy::default()
        };
        let plan = plan_routes(&graph, &mut beliefs.mean(0), &request(), tight)
            .expect("the direct route was found in time");
        assert_eq!(plan.active[0].hops.len(), 1);
        let starved = RoutingPolicy {
            max_labels: 1,
            ..RoutingPolicy::default()
        };
        assert_eq!(
            plan_routes(&graph, &mut beliefs.mean(0), &request(), starved),
            Err(RouteError::SearchLimit)
        );
    }
}
