//! Real stations on simulated HF for days: delivery, latency against what
//! was possible, and airtime.
//!
//! ```text
//! cargo run --release -p hm-node --example hf_days -- [days] [seeds] [tick seconds]
//! ```
//!
//! Five stations around the Baltic share one 300 bd HF channel; paths open
//! and close through the day and fade while open (see `tests/support/world.rs`).
//! `HM_LOG=<text>` prints the log lines of the stations that contain it
//! (`HM_LOG=LA1CCC:` for one station's).

#[path = "../tests/support/world.rs"]
mod world;

use hm_core::Millis;

fn main() {
    let arg = |i: usize, default: u64| {
        std::env::args()
            .nth(i)
            .map_or(default, |a| a.parse().expect("a number"))
    };
    let (days, seeds, tick) = (arg(1, 7), arg(2, 4), arg(3, 10));
    println!("{days} days, 5 stations, 300 bd HF, node tick {tick} s");
    let (mut sent, mut delivered, mut possible, mut busy) = (0, 0, 0, 0.0);
    let mut late = Vec::new();
    for seed in 1..=seeds {
        let mut scenario = world::baltic(days, seed);
        scenario.tick = Millis(tick * 1_000);
        scenario.log = true;
        let outcome = world::run(&scenario);
        println!("seed {seed}: {}", outcome.summary());
        let mut by_kind = std::collections::BTreeMap::new();
        for ((kind, _, _), (frames, ms)) in &outcome.airtime {
            let entry = by_kind.entry(kind.as_str()).or_insert((0, 0));
            entry.0 += frames;
            entry.1 += ms;
        }
        let seconds = outcome.seconds as f64;
        let shares: Vec<String> = by_kind
            .iter()
            .map(|(kind, (frames, ms))| format!("{kind} {frames} frames {:.1}%", *ms as f64 / 10.0 / seconds))
            .collect();
        println!("        airtime: {}", shares.join(", "));
        sent += outcome.sent.len();
        delivered += outcome.delivered();
        possible += outcome.possible();
        busy += outcome.channel_busy();
        late.extend(outcome.excess());
    }
    late.sort_unstable();
    let q = |p: f64| {
        late.get(((late.len().max(1) - 1) as f64 * p).round() as usize)
            .map_or("-".into(), |s| format!("{:.1} h", *s as f64 / 3600.0))
    };
    println!(
        "all: delivered {delivered}/{sent} ({possible} possible) | behind oracle p50 {} p90 {} | channel busy {:.1}%",
        q(0.5),
        q(0.9),
        100.0 * busy / seeds as f64
    );
}
