//! Routing over HF links that open for a few hours a day.
//!
//! Four stations in a chain, 0 - 1 - 2 - 3. Each hop's band is open three
//! hours a day at its own UTC time; while open, the pair gets a 15-minute
//! contact slot each way every quarter hour, each slot a 70 % chance. Mail
//! goes both ways end to end, one message every two hours, three days to live.
//!
//! "staggered": hop windows 00-03, 08-11 and 16-19 UTC, never open together.
//! A message can only cross by being carried: held at 1 until 1 - 2 opens.
//! "overlapping": windows 06-09, 07-10 and 08-11 UTC, open together at 08.
//!
//! Bayesian CGR is given the whole contact plan; CGR (live) knows only the
//! contacts open at the moment, which is what a station learns from beacons,
//! links and live adverts. The gap between the two is what predicting when a
//! closed band opens again would be worth.
//!
//! cargo run --release -p hm-sim --example routing_diurnal

use hm_route::Bearer;
use hm_sim::routing::{run_routing, ContactOpportunity, RoutingAlgorithm, RoutingScenario, SimBundle};

const HOUR: u64 = 3_600;
const DAY: u64 = 24 * HOUR;
const SLOT: u64 = 15 * 60;
const DAYS: u64 = 7;

fn scenario(window_starts: [u64; 3], seed: u64) -> RoutingScenario {
    let mut contacts = Vec::new();
    for day in 0..DAYS {
        for (hop, start_hour) in window_starts.iter().enumerate() {
            let open = day * DAY + start_hour * HOUR;
            for slot in 0..(3 * HOUR / SLOT) {
                let start = open + slot * SLOT;
                for (from, to) in [(hop, hop + 1), (hop + 1, hop)] {
                    contacts.push(ContactOpportunity {
                        start,
                        end: start + SLOT,
                        from,
                        to,
                        bearer: Bearer::Radio,
                        rate_bps: 300,
                        // Half the slot's bits, for overhead and the other direction.
                        capacity_bytes: 300 * SLOT / 8 / 2,
                        success_permyriad: 7_000,
                    });
                }
            }
        }
    }
    let mut bundles = Vec::new();
    for n in 0..(4 * 12) {
        let created = n / 2 * 2 * HOUR;
        let (source, destination) = if n % 2 == 0 { (0, 3) } else { (3, 0) };
        bundles.push(SimBundle {
            id: n,
            source,
            destination,
            created,
            ttl_secs: 3 * DAY,
            bytes: 600,
            urgent: false,
        });
    }
    RoutingScenario {
        nodes: 4,
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
        RoutingAlgorithm::BayesianCgrLive => "CGR (live)",
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
    for (label, windows) in [("staggered", [0, 8, 16]), ("overlapping", [6, 7, 8])] {
        println!("{label} windows (UTC hours {windows:?}, 3 h each)");
        println!(
            "{:<14} {:>9} {:>10} {:>10} {:>9} {:>7}",
            "algorithm", "delivery", "airtime", "p50(h)", "p95(h)", "brier"
        );
        let scenario = scenario(windows, seed);
        let mut algorithms = vec![RoutingAlgorithm::BayesianCgrLive];
        algorithms.extend(RoutingAlgorithm::comparison_set());
        for algorithm in algorithms {
            let report = run_routing(&scenario, algorithm).expect("valid scenario");
            let (p50, p95) = report
                .latency_secs
                .as_ref()
                .map_or((0.0, 0.0), |l| (l.p50 as f64 / 3_600.0, l.p95 as f64 / 3_600.0));
            let brier = report
                .calibration_brier
                .map_or("-".to_string(), |score| format!("{score:.3}"));
            println!(
                "{:<14} {:>8.1}% {:>9.0}s {:>10.1} {:>9.1} {:>7}",
                name(report.algorithm),
                report.delivery_ratio * 100.0,
                (report.payload_airtime_ms + report.control_airtime_ms) as f64 / 1_000.0,
                p50,
                p95,
                brier,
            );
        }
        println!();
    }
}
