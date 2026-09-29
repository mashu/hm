//! Real stations on simulated HF for days: the node and radio machines the
//! daemon runs, with a store each, on hm-sim's channel physics
//! (`support/world.rs`).

mod support;

use support::world::{baltic, line, run};

/// The ends of a line never hear each other. Each learns from its
/// neighbour's beacons that the other is on the air and not heard here, and
/// sends through the neighbour, whose beacons show the path onward.
#[test]
fn a_station_reaches_one_it_never_hears_through_a_neighbour() {
    let outcome = run(&line(24, 1));
    println!("{}", outcome.summary());
    assert!(!outcome.sent.is_empty());
    assert_eq!(outcome.delivered(), outcome.sent.len());
    let (ends, far) = (["SM0AAA", "LA1CCC"], |me: &str| {
        if me == "SM0AAA" {
            "LA1CCC"
        } else {
            "SM0AAA"
        }
    });
    for (me, estimates) in &outcome.estimates {
        let me = me.to_string();
        if !ends.contains(&me.as_str()) {
            continue;
        }
        let (_, _, p) = estimates
            .iter()
            .find(|(station, _, _)| station.to_string() == far(&me))
            .expect("the far end is known");
        assert!(*p < 0.1, "{me} believes the path to {} open: {p}", far(&me));
    }
}

#[test]
fn runs_are_reproducible() {
    let (a, b) = (run(&line(6, 7)), run(&line(6, 7)));
    assert_eq!(a.trace, b.trace);
    assert_eq!(
        a.sent.iter().map(|s| s.delivered_after).collect::<Vec<_>>(),
        b.sent.iter().map(|s| s.delivered_after).collect::<Vec<_>>()
    );
}

/// A week around the Baltic, over a few seeds (`examples/hf_days.rs` prints
/// more).
#[test]
#[ignore = "measurement: a week of HF, about 20 s per seed in release"]
fn a_week_around_the_baltic() {
    let (mut delivered, mut possible) = (0, 0);
    for seed in 1..=4 {
        let mut scenario = baltic(7, seed);
        scenario.tick = hm_core::Millis(10_000);
        let outcome = run(&scenario);
        println!("seed {seed}: {}", outcome.summary());
        delivered += outcome.delivered();
        possible += outcome.possible();
    }
    assert!(
        delivered * 10 >= possible * 8,
        "{delivered} of {possible} possible"
    );
}
