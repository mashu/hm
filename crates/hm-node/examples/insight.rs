//! What a station knows after days on simulated HF, as the daemon's
//! `GET /api/insight` serves it: the web interface's fixture.
//!
//! ```text
//! cargo run --release -p hm-node --example insight -- [days] [station] > insight.json
//! ```
//!
//! Five stations around the Baltic (see `tests/support/world.rs`), sending
//! until three hours before the end (midnight UTC, when the near paths are
//! mostly closed and messages wait); the station is one of SM0AAA
//! (default), OH2BBB, LA1CCC, OZ1DDD, ES1EEE.

#[path = "../tests/support/world.rs"]
mod world;

use hm_core::Millis;

fn main() {
    let days = std::env::args()
        .nth(1)
        .map_or(3, |a| a.parse().expect("a number of days"));
    let me = std::env::args().nth(2).unwrap_or_else(|| "SM0AAA".into());
    let mut scenario = world::baltic(days, 1);
    scenario.tick = Millis(10_000);
    scenario.traffic_hours = days * 24 - 3;
    scenario.messages_per_day = 24;
    let outcome = world::run(&scenario);
    eprintln!("{}", outcome.summary());
    let (_, insight) = outcome
        .insights
        .iter()
        .find(|(call, _)| call.to_string() == me)
        .unwrap_or_else(|| panic!("no station {me} in the scenario"));
    println!("{}", serde_json::to_string_pretty(insight).expect("serialises"));
}
