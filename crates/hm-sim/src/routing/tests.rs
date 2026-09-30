use super::*;

fn contact(start: u64, from: usize, to: usize) -> ContactOpportunity {
    ContactOpportunity {
        start,
        end: start + 10,
        from,
        to,
        bearer: Bearer::Radio,
        rate_bps: 9_600,
        capacity_bytes: 64 * 1024,
        success_permyriad: 10_000,
    }
}

fn line() -> RoutingScenario {
    let mut contacts = Vec::new();
    for cycle in 0..4 {
        let base = cycle * 100;
        contacts.extend([
            contact(base + 10, 0, 1),
            contact(base + 20, 1, 2),
            contact(base + 30, 2, 3),
            contact(base + 40, 1, 0),
            contact(base + 50, 2, 1),
            contact(base + 60, 3, 2),
        ]);
    }
    RoutingScenario {
        nodes: 4,
        contacts,
        bundles: (0..8)
            .map(|id| SimBundle {
                id,
                source: 0,
                destination: 3,
                created: 0,
                ttl_secs: 500,
                bytes: 256,
                urgent: false,
            })
            .collect(),
        seed: 7,
        transfer_overhead_bytes: 80,
        txdelay_ms: 100,
    }
}

#[test]
fn all_algorithms_run_on_the_same_trace_deterministically() {
    let scenario = line();
    let first = compare_routing(&scenario).unwrap();
    let second = compare_routing(&scenario).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.reports.len(), 5);
    assert!(first
        .reports
        .iter()
        .all(|report| report.delivery_ratio >= 0.0 && report.delivery_ratio <= 1.0));
}

#[test]
fn single_copy_cgr_uses_less_storage_than_the_flood_upper_bound() {
    let comparison = compare_routing(&line()).unwrap();
    let cgr = comparison.report(RoutingAlgorithm::BayesianCgr).unwrap();
    let flood = comparison.report(RoutingAlgorithm::Epidemic).unwrap();
    assert_eq!(cgr.delivered, cgr.generated);
    assert_eq!(flood.delivered, flood.generated);
    assert!(cgr.storage_high_water_objects < flood.storage_high_water_objects);
    assert!(cgr.duplicate_copies < flood.duplicate_copies);
    assert!(cgr.calibration_brier.is_some());
    let active_copy_bound =
        line().bundles.len() + line().bundles.iter().filter(|bundle| bundle.urgent).count();
    assert!(cgr.storage_high_water_objects <= active_copy_bound);
}

#[test]
fn live_cgr_knows_only_open_contacts() {
    // On the line each hop opens as the one before it closes: a message
    // must wait at each station for the next link.
    let planned = run_routing(&line(), RoutingAlgorithm::BayesianCgr).unwrap();
    let live = run_routing(&line(), RoutingAlgorithm::BayesianCgrLive).unwrap();
    assert_eq!(planned.delivered, planned.generated);
    assert_eq!(live.delivered, 0);
    assert_eq!(live.payload_transmissions, 0);
    // With a link straight to the destination it knows enough.
    let mut direct = line();
    direct.contacts.push(contact(15, 0, 3));
    let live = run_routing(&direct, RoutingAlgorithm::BayesianCgrLive).unwrap();
    assert!(live.delivered > 0);
}

#[test]
fn spray_copy_count_and_invalid_inputs_are_bounded() {
    let report = run_routing(&line(), RoutingAlgorithm::SprayAndWait { copies: 2 }).unwrap();
    assert!(report.storage_high_water_objects <= report.generated * 2);
    assert!(run_routing(&line(), RoutingAlgorithm::SprayAndWait { copies: 0 }).is_err());
    let mut invalid = line();
    invalid.contacts[0].success_permyriad = 10_001;
    assert!(compare_routing(&invalid).is_err());
}
