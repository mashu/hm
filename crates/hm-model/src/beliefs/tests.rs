use hm_core::DetRng;

use super::*;

fn call(s: &str) -> Callsign {
    s.parse().unwrap()
}

fn key(from: &str, to: &str, bearer: Bearer) -> LinkKey {
    LinkKey {
        from: call(from),
        to: call(to),
        bearer,
    }
}

/// Links that keep failing make a new link of the same kind look worse
/// too: the prior is the population's.
#[test]
fn a_new_link_is_expected_to_behave_like_its_kind() {
    let mut beliefs = Beliefs::new();
    let before = beliefs.link_prior(Bearer::Modem).handoff.mean;
    for peer in ["M0AAA", "M0BBB", "M0CCC"] {
        for t in 0..30 {
            let k = key("M0ME", peer, Bearer::Modem);
            beliefs.observe_link(k, t * 60, LinkObservation::Heard);
            beliefs.observe_link(k, t * 60 + 1, LinkObservation::Handoff { ok: false });
        }
    }
    let after = beliefs.link_prior(Bearer::Modem).handoff.mean;
    assert!(after < before - 0.2, "{before} -> {after}");
    // Radio links learned nothing from modem failures.
    assert_eq!(
        beliefs.link_prior(Bearer::Radio).handoff,
        LinkPrior::for_bearer(Bearer::Radio).handoff
    );
}

/// A beacon heard from a station and a handoff to it are evidence about
/// one path: what is learned one way holds the other.
#[test]
fn both_directions_of_a_path_share_one_belief() {
    let mut beliefs = Beliefs::new();
    let (out, back) = (
        key("M0ME", "M0AAA", Bearer::Radio),
        key("M0AAA", "M0ME", Bearer::Radio),
    );
    let unseen = beliefs.p_open(out, 0, 0);
    for t in 0..6 {
        beliefs.observe_link(back, t * 600, LinkObservation::Missed);
    }
    let now = 3_000;
    assert!(beliefs.p_open(out, now, now) < unseen / 2.0);
    assert_eq!(beliefs.p_open(out, now, now), beliefs.p_open(back, now, now));
    assert_eq!(beliefs.link(out), beliefs.link(back));
    assert_eq!(beliefs.links().count(), 1);
    // A record saved per direction comes back as the path's.
    let mut restored = Beliefs::new();
    for (k, v) in beliefs.take_changed() {
        restored.restore(&k, &v.unwrap()).unwrap();
    }
    assert_eq!(restored.link(back), beliefs.link(out));
}

#[test]
fn records_round_trip_and_deletions_are_reported() {
    let mut beliefs = Beliefs::new();
    let k = key("M0ME", "M0AAA", Bearer::Radio);
    beliefs.observe_link(k, 100, LinkObservation::Over { sent: 8, got: 6 });
    beliefs.observe_custodian(call("M0AAA"), 100, CustodianObservation::Accepted);
    let saved = beliefs.take_changed();
    assert_eq!(saved.len(), 2);
    assert!(beliefs.take_changed().is_empty());
    let mut restored = Beliefs::new();
    for (key, value) in &saved {
        restored.restore(key, value.as_ref().unwrap()).unwrap();
    }
    assert_eq!(restored.link(k), beliefs.link(k));
    assert_eq!(
        restored.custodian(call("M0AAA")),
        beliefs.custodian(call("M0AAA"))
    );
    beliefs.prune(100 + FORGET_AFTER + 1);
    let deleted = beliefs.take_changed();
    assert_eq!(deleted.len(), 2);
    assert!(deleted.iter().all(|(_, v)| v.is_none()));
    assert!(restored.restore(&[9, 9], &[1]).is_err());
}

#[test]
fn silence_counts_against_a_link_until_the_horizon() {
    let mut beliefs = Beliefs::new();
    let k = key("M0AAA", "M0ME", Bearer::Radio);
    beliefs.observe_link(k, 0, LinkObservation::Beacon);
    let open_then = beliefs.p_open(k, 0, 0);
    assert_eq!(beliefs.note_silence(k, 3_600, 600), 5);
    assert!(beliefs.p_open(k, 3_600, 3_600) < open_then);
    assert_eq!(beliefs.note_silence(k, 3_600, 600), 0);
    let far = MISS_HORIZON + 10 * DAY;
    let missed = beliefs.note_silence(k, far, 3_600);
    // Hourly misses from the last one counted to the horizon, the last
    // half hour of it still in grace.
    assert_eq!(missed as u64, (MISS_HORIZON - 3_600 - 1_800) / 3_600);
}

/// Thompson draws scatter around the posterior mean, and stay the same for
/// the same link within one plan.
#[test]
fn thompson_draws_are_consistent_within_a_plan() {
    let mut beliefs = Beliefs::new();
    let k = key("M0ME", "M0AAA", Bearer::Radio);
    for t in 0..5 {
        beliefs.observe_link(k, t * 600, LinkObservation::Heard);
        beliefs.observe_link(k, t * 600 + 1, LinkObservation::Handoff { ok: true });
    }
    let now = 3_000;
    let mut draw = beliefs.thompson(DetRng::from_seed(1), now);
    let first = draw.link(k, now, None);
    assert_eq!(draw.link(k, now, None), first);
    let mean = beliefs.mean(now).link(k, now, None);
    let average = (0..2_000)
        .map(|s| beliefs.thompson(DetRng::from_seed(s), now).link(k, now, None))
        .sum::<f64>()
        / 2_000.0;
    assert!((average - mean).abs() < 0.05, "{average} vs {mean}");
}

/// A stated probability counts for a couple of observations: it steers a
/// link without evidence, and yields to evidence.
#[test]
fn stated_probabilities_yield_to_evidence() {
    let mut beliefs = Beliefs::new();
    let k = key("M0ME", "M0AAA", Bearer::Radio);
    let blind = beliefs.mean(0).link(k, 0, Some(0.95));
    assert!((blind - 0.95).abs() < 1e-9);
    for t in 0..40 {
        beliefs.observe_link(k, t * 60, LinkObservation::Heard);
        beliefs.observe_link(k, t * 60 + 1, LinkObservation::Handoff { ok: false });
    }
    let now = 40 * 60;
    let informed = beliefs.mean(now).link(k, now, Some(0.95));
    assert!(informed < 0.3, "{informed}");
}
