use hm_route::Bearer;
use hm_sim::routing::{compare_routing, ContactOpportunity, RoutingAlgorithm, RoutingScenario, SimBundle};

fn contact(start: u64, from: usize, to: usize, success_permyriad: u16) -> ContactOpportunity {
    ContactOpportunity {
        start,
        end: start + 120,
        from,
        to,
        bearer: Bearer::Radio,
        rate_bps: 1_200,
        capacity_bytes: 24 * 1024,
        success_permyriad,
    }
}

fn scenario(seed: u64) -> RoutingScenario {
    let mut contacts = Vec::new();
    for cycle in 0..12 {
        let base = cycle * 1_800;
        if !(4..=5).contains(&cycle) {
            for hop in 0..7 {
                contacts.push(contact(base + 60 + hop as u64 * 90, hop, hop + 1, 8_500));
                contacts.push(contact(base + 900 + (6 - hop) as u64 * 90, hop + 1, hop, 8_500));
            }
        }
        contacts.push(contact(base + 300, 1, 5, 4_000));
        contacts.push(contact(base + 1_200, 5, 1, 4_000));
        contacts.push(contact(base + 600, 5, 7, 6_000));
        contacts.push(contact(base + 1_500, 1, 0, 6_000));
    }
    let mut bundles = Vec::new();
    for cycle in 0..10 {
        bundles.push(SimBundle {
            id: cycle * 2,
            source: 0,
            destination: 7,
            created: cycle * 1_800,
            ttl_secs: 7_200,
            bytes: 700,
            urgent: cycle % 5 == 0,
        });
        bundles.push(SimBundle {
            id: cycle * 2 + 1,
            source: 7,
            destination: 0,
            created: cycle * 1_800,
            ttl_secs: 7_200,
            bytes: 700,
            urgent: cycle % 5 == 0,
        });
    }
    RoutingScenario {
        nodes: 8,
        contacts,
        bundles,
        seed,
        transfer_overhead_bytes: 120,
        txdelay_ms: 300,
    }
}

fn name(algorithm: RoutingAlgorithm) -> &'static str {
    match algorithm {
        RoutingAlgorithm::BayesianCgr => "Bayesian CGR",
        RoutingAlgorithm::Epidemic => "Epidemic",
        RoutingAlgorithm::SprayAndWait { .. } => "Spray L=2",
        RoutingAlgorithm::Prophet => "PRoPHET-like",
        RoutingAlgorithm::Meed => "MEED",
    }
}

fn main() {
    let seed = std::env::var("HM_ROUTING_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(7);
    let comparison = compare_routing(&scenario(seed)).expect("valid benchmark");
    println!(
        "{:<14} {:>9} {:>10} {:>10} {:>10} {:>10} {:>9}",
        "algorithm", "delivery", "airtime", "p95(s)", "storage", "copies", "brier"
    );
    for report in comparison.reports {
        let latency = report.latency_secs.as_ref().map_or(0, |latency| latency.p95);
        let brier = report
            .calibration_brier
            .map_or("-".to_string(), |score| format!("{score:.3}"));
        println!(
            "{:<14} {:>8.1}% {:>9.1}s {:>10} {:>10} {:>10} {:>9}",
            name(report.algorithm),
            report.delivery_ratio * 100.0,
            (report.payload_airtime_ms + report.control_airtime_ms) as f64 / 1_000.0,
            latency,
            report.storage_high_water_objects,
            report.duplicate_copies,
            brier,
        );
    }
}
