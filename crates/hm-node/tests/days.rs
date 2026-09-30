//! Real stations on simulated HF for days: the node and radio machines the
//! daemon runs, with a store each, on hm-sim's channel physics
//! (`support/world.rs`).

mod support;

use hm_node::insight::Verdict;
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

/// What a station knows is shown as it is: every station it has heard of
/// and where, what it believes of each path, what moved its beliefs last,
/// and where each message went.
#[test]
fn a_station_shows_what_it_knows() {
    let outcome = run(&line(24, 1));
    let (_, insight) = outcome
        .insights
        .iter()
        .find(|(me, _)| me.to_string() == "SM0AAA")
        .expect("SM0AAA ran");
    let station = |call: &str| {
        insight
            .stations
            .iter()
            .find(|s| s.call == call)
            .unwrap_or_else(|| panic!("{call} is known"))
    };
    let me = station("SM0AAA");
    assert!(me.me && me.place.as_ref().is_some_and(|p| p.locator == "JO99ah"));
    let neighbour = station("OH2BBB");
    assert!(neighbour.trusted && neighbour.beacon.is_some());
    assert_eq!(
        neighbour.place.as_ref().map(|p| p.locator.as_str()),
        Some("KP20le")
    );
    // The far end is known from the neighbour's beacons, and never heard.
    assert!(station("LA1CCC").heard_at.is_none());

    let path = |to: &str| {
        insight
            .links
            .iter()
            .find(|l| l.mine && (l.a == to || l.b == to))
            .unwrap_or_else(|| panic!("a path to {to}"))
    };
    let near = path("OH2BBB");
    assert!(near.seen && near.open_now > 0.5, "{near:?}");
    assert_eq!((near.forecast.len(), near.daily.len()), (24, 24));
    let loss = near.frame_loss;
    assert!(loss.low <= loss.mean && loss.mean <= loss.high, "{loss:?}");
    let far = path("LA1CCC");
    assert!(!far.seen && far.reach < 0.5, "{far:?}");

    assert!((1..=3 * hm_model::JOURNAL_LEN).contains(&insight.journal.len()));
    assert!(
        insight.decisions.iter().any(|d| matches!(
            &d.verdict,
            Verdict::Send { route } if route.hops[0].to == "OH2BBB"
        )),
        "{:?}",
        insight.decisions
    );
    let json = serde_json::to_value(insight).expect("serialises");
    assert_eq!(json["decisions"][0]["verdict"], "send");
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
