//! What a relay holds for others: released at the next custodian's receipt,
//! taken back when it could not deliver, taken on again when offered anew.

use super::*;

#[test]
fn relay_custody_moves_without_claiming_final_delivery() {
    let db = TempDb::new("relay-custody");
    let s = Store::open(&db.0).unwrap();
    let origin = call("M0AAA");
    let relay = call("M0BBB");
    let destination = call("M0CCC");
    let metadata = RelayMetadata {
        custody_from: origin,
        destination,
        precedence: 1,
        hop_count: 1,
        visited: &[origin],
        max_hops: 8,
        expires_at: 1_000,
        wire_seq: None,
    };
    assert!(s.enqueue_relay(id(1), b"bundle", metadata, 100).unwrap());
    assert_eq!(s.relay_usage(100).unwrap(), HoldingUsage { count: 1, bytes: 6 });
    assert_eq!(s.holding_ids(destination, false, 100).unwrap(), vec![id(1)]);
    assert_eq!(s.holding_ids(relay, true, 100).unwrap(), vec![id(1)]);
    assert!(s.set_next_hop(id(1), relay).unwrap());
    assert!(s
        .custody_transferred(
            id(1),
            CustodyHandoff {
                next_hop: relay,
                receipt_verified: true,
                by: "radio",
                now: 110,
                grace_secs: 3600,
                suspect_secs: 86_400,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    let record = s.record(id(1)).unwrap().unwrap();
    assert_eq!(record.state, State::InTransit);
    assert_eq!(record.custody_by, Some(relay));
    assert_eq!(record.shadow_until, Some(110 + 3600));
    assert_eq!(record.next_attempt, 1000, "suspect capped by expires_at");
    assert_eq!(s.relay_usage(110).unwrap().count, 0);
    assert!(s.due(110).unwrap().is_empty());
    // Shadow still advertised for holdings pull.
    assert_eq!(s.holding_ids(destination, false, 110).unwrap(), vec![id(1)]);
    assert!(s.holding_ids(destination, false, 110 + 3601).unwrap().is_empty());
}

/// A relay's part ends when it hands a holding on; the custodian after it
/// that cannot deliver hands it back with a custody-fail notice, and the
/// relay takes it on again.
#[test]
fn a_relay_takes_back_what_its_custodian_could_not_deliver() {
    let db = TempDb::new("relay-fail-back");
    let s = Store::open(&db.0).unwrap();
    let (origin, next, destination) = (call("M0AAA"), call("M0BBB"), call("M0CCC"));
    let metadata = RelayMetadata {
        custody_from: origin,
        destination,
        precedence: 1,
        hop_count: 1,
        visited: &[origin],
        max_hops: 8,
        expires_at: 100_000,
        wire_seq: None,
    };
    assert!(s.enqueue_relay(id(1), b"bundle", metadata, 100).unwrap());
    assert!(s.set_next_hop(id(1), next).unwrap());
    assert!(s
        .custody_transferred(
            id(1),
            CustodyHandoff {
                next_hop: next,
                receipt_verified: true,
                by: "radio",
                now: 110,
                grace_secs: 3600,
                suspect_secs: 86_400,
                eta: 200,
                awaits_receipt: false,
            }
        )
        .unwrap());
    let record = s.record(id(1)).unwrap().unwrap();
    assert!(record.handed_on());
    assert!(s.suspect_due(1_000_000).unwrap().is_empty());
    // Someone else's notice about it changes nothing.
    assert_eq!(
        s.apply_custody_fail(id(1), destination, 500, "not mine").unwrap(),
        ReclaimOutcome::Ignored
    );
    assert_eq!(
        s.apply_custody_fail(id(1), next, 600, "no route").unwrap(),
        ReclaimOutcome::Requeued
    );
    assert_eq!(s.record(id(1)).unwrap().unwrap().state, State::Queued);
    assert_eq!(s.due(600).unwrap()[0].id, id(1));
}

#[test]
fn a_failed_relay_holding_is_taken_on_again_when_custody_is_offered_anew() {
    let db = TempDb::new("revive");
    let s = Store::open(&db.0).unwrap();
    let origin = call("M0AAA");
    let other = call("M0DDD");
    let destination = call("M0CCC");
    let metadata = |from: Callsign, visited: &'static [Callsign]| RelayMetadata {
        custody_from: from,
        destination,
        precedence: 1,
        hop_count: visited.len() as u8,
        visited,
        max_hops: 8,
        expires_at: 1_000,
        wire_seq: None,
    };
    let first: &'static [Callsign] = Box::leak(Box::new([origin]));
    let second: &'static [Callsign] = Box::leak(Box::new([origin, other]));
    assert!(s
        .enqueue_relay(id(1), b"bundle", metadata(origin, first), 100)
        .unwrap());
    assert!(
        !s.revive_relay(id(1), metadata(other, second), 150).unwrap(),
        "still active"
    );
    assert_eq!(s.abandon(id(1), "no route").unwrap(), Some(origin));
    assert_eq!(s.relay_usage(200).unwrap().count, 0);

    let mut wrong = metadata(other, second);
    wrong.destination = other;
    assert!(
        !s.revive_relay(id(1), wrong, 200).unwrap(),
        "a different destination"
    );
    assert!(s.revive_relay(id(1), metadata(other, second), 200).unwrap());
    let r = s.record(id(1)).unwrap().unwrap();
    assert_eq!(
        (r.state, r.attempts, r.custody_from, r.hop_count, r.next_attempt),
        (State::Queued, 0, Some(other), Some(2), 200)
    );
    assert_eq!(r.visited.as_deref(), Some(second));
    assert_eq!(s.relay_usage(200).unwrap().count, 1);
    assert_eq!(s.due(200).unwrap()[0].id, id(1));
    assert!(
        s.revive_relay(id(1), metadata(other, second), 1_000).is_err(),
        "expired"
    );

    s.enqueue(id(2), b"own", destination, 0, 0).unwrap();
    s.abandon(id(2), "no route").unwrap();
    assert!(
        !s.revive_relay(id(2), metadata(other, second), 200).unwrap(),
        "not a relay holding"
    );
}

#[test]
fn a_receipt_passing_through_closes_the_relay_holding_it_answers() {
    let db = TempDb::new("relay-receipt");
    let s = Store::open(&db.0).unwrap();
    let origin = call("M0AAA");
    let next = call("M0BBB");
    let destination = call("M0CCC");
    let metadata = RelayMetadata {
        custody_from: origin,
        destination,
        precedence: 1,
        hop_count: 1,
        visited: &[origin],
        max_hops: 8,
        expires_at: 1_000,
        wire_seq: None,
    };
    assert!(s.enqueue_relay(id(1), b"bundle", metadata, 100).unwrap());
    assert!(s.set_next_hop(id(1), next).unwrap());
    assert!(s
        .custody_transferred(
            id(1),
            CustodyHandoff {
                next_hop: next,
                receipt_verified: true,
                by: "radio",
                now: 110,
                grace_secs: 50,
                suspect_secs: 30,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    assert!(
        !s.relay_receipted(id(1), id(9), next, 120).unwrap(),
        "only the destination's"
    );
    assert!(
        !s.relay_receipted(id(7), id(9), destination, 120).unwrap(),
        "unknown holding"
    );
    assert!(s.relay_receipted(id(1), id(9), destination, 120).unwrap());
    let r = s.record(id(1)).unwrap().unwrap();
    assert_eq!((r.state, r.e2e_receipt), (State::Delivered, Some(id(9))));
    assert!(s.suspect_due(1_000).unwrap().is_empty(), "never resent");
    assert!(!s.relay_receipted(id(1), id(9), destination, 130).unwrap());

    // Our own messages are completed by e2e_delivered, not this.
    s.enqueue(id(2), b"own", destination, 0, 0).unwrap();
    assert!(!s.relay_receipted(id(2), id(8), destination, 120).unwrap());
}
