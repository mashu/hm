//! Custody: handed on only against a verified receipt, reclaimed when the
//! custodian is suspect or says it failed, and what the outcome says about
//! the custodian.

use super::*;

#[test]
fn active_custody_is_l_one_normally_and_l_two_only_when_urgent() {
    let db = TempDb::new("copy-bounds");
    let store = Store::open(&db.0).unwrap();
    let (first, second, third) = (call("M0R01"), call("M0R02"), call("M0R03"));
    store.enqueue(id(1), b"routine", call("M0DST"), 1, 10).unwrap();
    assert!(store.set_next_hop(id(1), first).unwrap());
    assert!(!store.set_next_hop(id(1), second).unwrap());

    store.enqueue(id(2), b"urgent", call("M0DST"), 2, 10).unwrap();
    assert!(store.set_next_hop(id(2), first).unwrap());
    assert!(store.set_next_hop(id(2), second).unwrap());
    assert!(!store.set_next_hop(id(2), third).unwrap());
    assert!(store
        .custody_transferred(
            id(2),
            CustodyHandoff {
                next_hop: first,
                receipt_verified: true,
                by: "radio",
                now: 20,
                grace_secs: 3600,
                suspect_secs: 86_400,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    assert!(store
        .custody_transferred(
            id(2),
            CustodyHandoff {
                next_hop: second,
                receipt_verified: true,
                by: "internet",
                now: 21,
                grace_secs: 3600,
                suspect_secs: 86_400,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    let record = store.record(id(2)).unwrap().unwrap();
    assert_eq!(record.custody_copies.as_deref(), Some(&[first, second][..]));
}

#[test]
fn unverified_receipt_does_not_transfer_custody() {
    let db = TempDb::new("unverified-custody");
    let s = Store::open(&db.0).unwrap();
    let destination = call("M0CCC");
    let relay = call("M0BBB");
    s.enqueue(id(1), b"one", destination, 0, 10).unwrap();
    assert!(s.set_next_hop(id(1), relay).unwrap());
    assert!(!s
        .custody_transferred(
            id(1),
            CustodyHandoff {
                next_hop: relay,
                receipt_verified: false,
                by: "radio",
                now: 20,
                grace_secs: 3600,
                suspect_secs: 86_400,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    let record = s.record(id(1)).unwrap().unwrap();
    assert_eq!(record.state, State::Queued);
    assert!(record.custody_by.is_none());
}

#[test]
fn suspect_reclaims_then_delivered_unconfirmed_when_expired() {
    let db = TempDb::new("suspect");
    let s = Store::open(&db.0).unwrap();
    let destination = call("M0CCC");
    let relay = call("M0BBB");
    s.enqueue(id(1), b"one", destination, 0, 10).unwrap();
    assert!(s.set_next_hop(id(1), relay).unwrap());
    assert!(s
        .custody_transferred(
            id(1),
            CustodyHandoff {
                next_hop: relay,
                receipt_verified: true,
                by: "radio",
                now: 100,
                grace_secs: 50,
                suspect_secs: 30,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    assert!(s.suspect_due(120).unwrap().is_empty());
    assert_eq!(s.suspect_due(130).unwrap().len(), 1);
    assert_eq!(
        s.reclaim_custody(id(1), 130, "custody suspect; reclaiming")
            .unwrap(),
        ReclaimOutcome::Requeued
    );
    assert_eq!(s.record(id(1)).unwrap().unwrap().state, State::Queued);
    assert_eq!(s.due(130).unwrap().len(), 1);

    // Fresh message that expires before reclaim.
    s.enqueue_with(
        id(2),
        b"two",
        EnqueueOpts {
            to: destination,
            precedence: 0,
            now: 200,
            wire_seq: None,
            expires_at: Some(220),
        },
    )
    .unwrap();
    assert!(s.set_next_hop(id(2), relay).unwrap());
    assert!(s
        .custody_transferred(
            id(2),
            CustodyHandoff {
                next_hop: relay,
                receipt_verified: true,
                by: "radio",
                now: 200,
                grace_secs: 10,
                suspect_secs: 5,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    // suspect_at is min(205, expires 220) = 205
    assert_eq!(
        s.reclaim_custody(id(2), 230, "expired").unwrap(),
        ReclaimOutcome::DeliveredUnconfirmed
    );
    assert_eq!(
        s.record(id(2)).unwrap().unwrap().state,
        State::DeliveredUnconfirmed
    );
}

#[test]
fn custody_fail_notice_reclaims() {
    let db = TempDb::new("custody-fail");
    let s = Store::open(&db.0).unwrap();
    let destination = call("M0CCC");
    let relay = call("M0BBB");
    s.enqueue(id(1), b"one", destination, 0, 10).unwrap();
    assert!(s.set_next_hop(id(1), relay).unwrap());
    assert!(s
        .custody_transferred(
            id(1),
            CustodyHandoff {
                next_hop: relay,
                receipt_verified: true,
                by: "radio",
                now: 20,
                grace_secs: 3600,
                suspect_secs: 86_400,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    assert_eq!(
        s.apply_custody_fail(id(1), relay, 50, "relay abandoned").unwrap(),
        ReclaimOutcome::Requeued
    );
    assert_eq!(s.record(id(1)).unwrap().unwrap().state, State::Queued);
    assert_eq!(
        s.apply_custody_fail(id(1), relay, 60, "stale").unwrap(),
        ReclaimOutcome::Ignored
    );
}

/// A receipt for a message that went through a custodian leaves a custody
/// outcome for the node to learn from, once.
#[test]
fn end_to_end_receipts_report_custody_outcomes_once() {
    let db = TempDb::new("custody-outcomes");
    let store = Store::open(&db.0).unwrap();
    let (me, relay, dest) = (call("M0AAA"), call("M0BBB"), call("M0CCC"));
    let _ = me;
    store.enqueue(id(1), b"mail", dest, 0, 100).unwrap();
    assert!(store.set_next_hop(id(1), relay).unwrap());
    assert!(store
        .custody_transferred(
            id(1),
            CustodyHandoff {
                next_hop: relay,
                receipt_verified: true,
                by: "radio",
                now: 200,
                grace_secs: 60,
                suspect_secs: 3_600,
                eta: 500,
                awaits_receipt: true,
            },
        )
        .unwrap());
    assert!(store.e2e_delivered(id(1), id(2), dest, 900).unwrap());
    assert_eq!(
        store.take_custody_outcomes().unwrap(),
        vec![CustodyOutcome {
            id: id(1),
            custodian: relay,
            expected_at: 500,
            delivered_at: 900,
        }]
    );
    assert!(store.take_custody_outcomes().unwrap().is_empty());
}

/// Custody reclaimed too soon: the receipt that comes after all is the first
/// custodian's, with its true lateness, not lost, and not the next one's.
#[test]
fn a_receipt_after_a_reclaim_is_credited_to_the_first_custodian() {
    let db = TempDb::new("custody-late");
    let store = Store::open(&db.0).unwrap();
    let (first, second, dest) = (call("M0BBB"), call("M0DDD"), call("M0CCC"));
    store.enqueue(id(1), b"mail", dest, 0, 100_000).unwrap();
    let hand = |to, now, eta| {
        assert!(store.set_next_hop(id(1), to).unwrap());
        assert!(store
            .custody_transferred(
                id(1),
                CustodyHandoff {
                    next_hop: to,
                    receipt_verified: true,
                    by: "radio",
                    now,
                    grace_secs: 60,
                    suspect_secs: 600,
                    eta,
                    awaits_receipt: true,
                },
            )
            .unwrap());
    };
    hand(first, 200, 300);
    assert_eq!(
        store
            .reclaim_custody(id(1), 900, "custody suspect; reclaiming")
            .unwrap(),
        ReclaimOutcome::Requeued
    );
    hand(second, 1_000, 1_100);
    assert!(store.e2e_delivered(id(1), id(2), dest, 5_000).unwrap());
    assert_eq!(
        store.take_custody_outcomes().unwrap(),
        vec![CustodyOutcome {
            id: id(1),
            custodian: first,
            expected_at: 300,
            delivered_at: 5_000,
        }]
    );
}

/// Nothing answers a receipt end to end: handed to a custodian, it is that
/// custodian's, and no suspect timer brings it back.
#[test]
fn an_unanswered_handoff_ends_with_custody() {
    let db = TempDb::new("unanswered");
    let store = Store::open(&db.0).unwrap();
    let (relay, dest) = (call("M0BBB"), call("M0CCC"));
    store.enqueue(id(1), b"receipt", dest, 1, 0).unwrap();
    assert!(store.set_next_hop(id(1), relay).unwrap());
    assert!(store
        .custody_transferred(
            id(1),
            CustodyHandoff {
                next_hop: relay,
                receipt_verified: true,
                by: "radio",
                now: 100,
                grace_secs: 60,
                suspect_secs: 600,
                eta: 100,
                awaits_receipt: false,
            },
        )
        .unwrap());
    let record = store.record(id(1)).unwrap().unwrap();
    assert_eq!(record.state, State::DeliveredUnconfirmed);
    assert!(store.suspect_due(10_000).unwrap().is_empty());
}
