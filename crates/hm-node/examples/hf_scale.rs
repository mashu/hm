//! Whole stations on simulated HF as the network grows: does delivery hold
//! up, and does the overhead each station pays stay put?
//!
//! ```text
//! cargo run --release -p hm-node --example hf_scale -- [sizes] [days] [side km] [per station per day] [seed]
//! cargo run --release -p hm-node --example hf_scale -- 5,10,20,40 4 800 4 1
//! ```
//!
//! Stations are scattered over a square `side` km wide (see
//! `world::scattered`); a fixed side packs more stations into the same
//! channel as the network grows, `0` grows the square with the network so
//! that each station keeps about as many neighbours. Each station sends the
//! same number of messages a day whatever the size. `HM_UNDELIVERED=1` lists
//! the messages not delivered; `HM_LOG` and `HM_TRACE` work as in `hf_days`.

#[path = "../tests/support/world.rs"]
mod world;

use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, default: &str| args.get(i).cloned().unwrap_or_else(|| default.into());
    let sizes: Vec<usize> = arg(1, "5,10,20,40")
        .split(',')
        .map(|n| n.parse().expect("a size"))
        .collect();
    let days: u64 = arg(2, "4").parse().expect("days");
    let side: f64 = arg(3, "800").parse().expect("side in km");
    let per_station: usize = arg(4, "4").parse().expect("messages per station per day");
    let seed: u64 = arg(5, "1").parse().expect("seed");
    println!(
        "{days} days, {per_station} messages per station per day, {}",
        if side > 0.0 {
            format!("stations in a {side:.0} km square")
        } else {
            "the square grows with the network".into()
        }
    );
    println!(
        "{:>3} {:>5} {:>6} | {:>9} {:>7} {:>7} | {:>6} {:>6} | {:>8} {:>8} {:>8} | {:>6}",
        "n",
        "paths",
        "degree",
        "delivered",
        "late50",
        "late90",
        "busy",
        "max",
        "beacon",
        "control",
        "data",
        "wall"
    );
    for n in sizes {
        let side_km = if side > 0.0 {
            side
        } else {
            360.0 * (n as f64).sqrt()
        };
        let mut scenario = world::scattered(n, side_km, per_station, days, seed);
        scenario.log = true;
        let outcome = world::run(&scenario);
        let busy = world::local_busy(&scenario, &outcome);
        let mean_busy = busy.iter().sum::<f64>() / n as f64;
        let max_busy = busy.iter().copied().fold(0.0, f64::max);
        let mut by_kind: BTreeMap<&str, u64> = BTreeMap::new();
        for ((kind, _, _), (_, ms)) in &outcome.airtime {
            *by_kind.entry(kind.as_str()).or_insert(0) += ms;
        }
        // Seconds on the air per station per hour.
        let per_hour = |kinds: &[&str]| {
            let ms: u64 = kinds.iter().map(|k| by_kind.get(k).copied().unwrap_or(0)).sum();
            ms as f64 / 1_000.0 / n as f64 / (outcome.seconds as f64 / 3_600.0)
        };
        let mut late = outcome.excess();
        late.sort_unstable();
        let q = |p: f64| {
            late.get(((late.len().max(1) - 1) as f64 * p).round() as usize)
                .map_or("-".into(), |s| format!("{:.1}h", *s as f64 / 3600.0))
        };
        println!(
            "{:>3} {:>5} {:>6.1} | {:>4}/{:<4} {:>7} {:>7} | {:>5.1}% {:>5.1}% | {:>7.1}s {:>7.1}s {:>7.1}s | {:>5.0}s",
            n,
            scenario.paths.len(),
            2.0 * scenario.paths.len() as f64 / n as f64,
            outcome.delivered(),
            outcome.sent.len(),
            q(0.5),
            q(0.9),
            100.0 * mean_busy,
            100.0 * max_busy,
            per_hour(&["Beacon"]),
            per_hour(&["Ctrl", "Sync", "Ack"]),
            per_hour(&["Data"]),
            outcome.wall_secs,
        );
        let seconds = outcome.seconds as f64;
        let fates: Vec<String> = outcome
            .fates
            .iter()
            .map(|((kind, fate), (_, ms))| format!("{kind} {fate} {:.1}%", *ms as f64 / 10.0 / seconds))
            .collect();
        println!("    unicast airtime by fate: {}", fates.join(", "));
        let (p50, p90, longest) = outcome.keyup_secs();
        println!("    key-ups: p50 {p50:.1} s, p90 {p90:.1} s, longest {longest:.1} s");
        if std::env::var("HM_UNDELIVERED").is_ok() {
            for m in outcome.sent.iter().filter(|m| m.delivered_after.is_none()) {
                let id: String = m.id.0[..6].iter().map(|b| format!("{b:02x}")).collect();
                println!(
                    "    not delivered: {id} {} -> {} queued at {} h",
                    scenario.stations[m.from],
                    scenario.stations[m.to],
                    m.at / 3600
                );
            }
        }
        if outcome.possible() < outcome.sent.len() {
            println!(
                "    ({} of {} were possible at all)",
                outcome.possible(),
                outcome.sent.len()
            );
        }
    }
    println!(
        "busy: channel time taken where each station is (its own frames and its neighbours'), mean and worst"
    );
    println!("beacon, control, data: seconds on the air per station per hour");
}
