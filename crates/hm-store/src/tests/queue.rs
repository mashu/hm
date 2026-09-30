//! Received bundles and the outbox: stored once, ordered, retried, kept
//! across restarts, listed, cancelled and deleted.

use super::*;

#[test]
fn received_bundles_are_stored_once_even_across_restarts() {
    let db = TempDb::new("dedup");
    {
        let s = Store::open(&db.0).unwrap();
        assert!(s
            .put_received(id(1), b"envelope", call("SA0KAM"), true, 100)
            .unwrap());
        assert!(!s
            .put_received(id(1), b"envelope", call("SA0KAM"), true, 101)
            .unwrap());
    }
    let s = Store::open(&db.0).unwrap();
    assert!(!s
        .put_received(id(1), b"envelope", call("SA0KAM"), true, 200)
        .unwrap());
    let inbox = s.list(Direction::In, 10).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(
        (inbox[0].at, inbox[0].verified, inbox[0].state),
        (100, true, State::Unread)
    );
    assert_eq!(s.object(id(1)).unwrap().unwrap(), b"envelope");
    s.mark_read(id(1)).unwrap();
    assert_eq!(s.record(id(1)).unwrap().unwrap().state, State::Read);
}

#[test]
fn outbox_order_is_precedence_then_age() {
    let db = TempDb::new("order");
    let s = Store::open(&db.0).unwrap();
    s.enqueue(id(1), b"a", call("SO5KM"), 0, 10).unwrap();
    s.enqueue(id(2), b"b", call("SO5KM"), 3, 11).unwrap();
    s.enqueue(id(3), b"c", call("SO5KM"), 0, 12).unwrap();
    s.enqueue(id(4), b"d", call("SO5KM"), 1, 13).unwrap();
    let order: Vec<u8> = s.due(100).unwrap().iter().map(|r| r.id.0[0]).collect();
    assert_eq!(order, vec![2, 4, 1, 3]);
    assert!(
        !s.enqueue(id(1), b"a", call("SO5KM"), 0, 20).unwrap(),
        "no duplicate queue entry"
    );
}

#[test]
fn delivery_leaves_the_queue_and_failures_back_off_then_give_up() {
    let db = TempDb::new("retry");
    let s = Store::open(&db.0).unwrap();
    let policy = RetryPolicy {
        first_delay_secs: 10,
        max_delay_secs: 25,
        max_attempts: 4,
    };
    s.enqueue(id(1), b"a", call("SO5KM"), 0, 0).unwrap();
    s.enqueue(id(2), b"b", call("SO5KM"), 0, 0).unwrap();
    s.delivered(id(1), true, "radio", 5).unwrap();
    let r = s.record(id(1)).unwrap().unwrap();
    assert_eq!(
        (r.state, r.verified, r.by.as_deref()),
        (State::Delivered, true, Some("radio"))
    );
    assert_eq!(s.due(1000).unwrap().len(), 1);

    assert_eq!(
        s.attempt_failed(id(2), "no answer", policy, 100).unwrap(),
        (Retry::At(110), None)
    );
    assert!(s.due(105).unwrap().is_empty(), "not due before the delay");
    assert_eq!(s.due(110).unwrap().len(), 1);
    assert_eq!(
        s.attempt_failed(id(2), "no answer", policy, 110).unwrap(),
        (Retry::At(130), None)
    );
    assert_eq!(
        s.attempt_failed(id(2), "no answer", policy, 130).unwrap(),
        (Retry::At(155), None),
        "capped at 25 s"
    );
    assert_eq!(
        s.attempt_failed(id(2), "no answer", policy, 155).unwrap(),
        (Retry::GaveUp, None)
    );
    let r = s.record(id(2)).unwrap().unwrap();
    assert_eq!(
        (r.state, r.attempts, r.note.as_deref()),
        (State::Failed, 4, Some("no answer"))
    );
    assert!(s.due(u64::MAX).unwrap().is_empty());
}

#[test]
fn queue_survives_restart() {
    let db = TempDb::new("restart");
    {
        let s = Store::open(&db.0).unwrap();
        s.enqueue(id(7), b"x", call("SO5KM-1"), 2, 50).unwrap();
    }
    let s = Store::open(&db.0).unwrap();
    let due = s.due(50).unwrap();
    assert_eq!(
        (due.len(), due[0].peer, due[0].precedence),
        (1, call("SO5KM-1"), 2)
    );
}

#[test]
fn listing_is_newest_first_and_limited() {
    let db = TempDb::new("list");
    let s = Store::open(&db.0).unwrap();
    for n in 0..5u8 {
        s.put_received(id(n), b"m", call("SA0KAM"), false, 100 + n as u64)
            .unwrap();
    }
    s.enqueue(id(9), b"o", call("SO5KM"), 0, 500).unwrap();
    let newest: Vec<u8> = s
        .list(Direction::In, 3)
        .unwrap()
        .iter()
        .map(|r| r.id.0[0])
        .collect();
    assert_eq!(newest, vec![4, 3, 2]);
    assert_eq!(s.list(Direction::Out, 10).unwrap().len(), 1);
    assert!(matches!(s.mark_read(id(42)), Err(Error::NotFound)));
}

#[test]
fn retry_delays() {
    let p = RetryPolicy::default();
    assert_eq!(
        (p.delay_after(1), p.delay_after(2), p.delay_after(3)),
        (60, 120, 240)
    );
    assert_eq!(p.delay_after(30), 3600);
}

#[test]
fn cancellation_ignores_late_receipt_and_e2e_requires_destination() {
    let db = TempDb::new("cancel-e2e");
    let s = Store::open(&db.0).unwrap();
    let destination = call("M0CCC");
    let relay = call("M0BBB");
    s.enqueue(id(1), b"one", destination, 0, 10).unwrap();
    assert!(s.set_next_hop(id(1), relay).unwrap());
    assert!(s.cancel(id(1)).unwrap());
    assert!(!s
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
    assert_eq!(s.record(id(1)).unwrap().unwrap().state, State::Cancelled);

    s.enqueue(id(2), b"two", destination, 0, 10).unwrap();
    assert!(!s.e2e_delivered(id(2), id(9), relay, 20).unwrap());
    assert!(s.e2e_delivered(id(2), id(9), destination, 21).unwrap());
    let delivered = s.record(id(2)).unwrap().unwrap();
    assert_eq!(delivered.state, State::Delivered);
    assert_eq!(delivered.e2e_receipt, Some(id(9)));
}

#[test]
fn deletion_cancels_first_and_never_removes_active_custody() {
    let db = TempDb::new("delete");
    let store = Store::open(&db.0).unwrap();
    let destination = call("M0CCC");
    let relay = call("M0BBB");

    store.enqueue(id(1), b"queued", destination, 0, 10).unwrap();
    assert_eq!(store.delete(id(1)).unwrap(), DeleteOutcome::Cancelled);
    assert_eq!(store.record(id(1)).unwrap().unwrap().state, State::Cancelled);
    assert_eq!(store.object(id(1)).unwrap().as_deref(), Some(&b"queued"[..]));
    assert_eq!(store.delete(id(1)).unwrap(), DeleteOutcome::Deleted);
    assert!(store.record(id(1)).unwrap().is_none());
    assert!(store.object(id(1)).unwrap().is_none());

    store.enqueue(id(2), b"active", destination, 0, 20).unwrap();
    assert!(store.set_next_hop(id(2), relay).unwrap());
    assert!(store
        .custody_transferred(
            id(2),
            CustodyHandoff {
                next_hop: relay,
                receipt_verified: true,
                by: "radio",
                now: 21,
                grace_secs: 3600,
                suspect_secs: 86_400,
                eta: 0,
                awaits_receipt: true,
            }
        )
        .unwrap());
    assert_eq!(store.delete(id(2)).unwrap(), DeleteOutcome::Active);
    assert_eq!(store.record(id(2)).unwrap().unwrap().state, State::InTransit);
}

#[test]
fn peer_chat_seq_is_monotonic() {
    let db = TempDb::new("peer-seq");
    let s = Store::open(&db.0).unwrap();
    let peer = call("M0CCC");
    assert_eq!(s.next_peer_seq(peer).unwrap(), 1);
    assert_eq!(s.next_peer_seq(peer).unwrap(), 2);
    assert_eq!(s.next_peer_seq(call("M0DDD")).unwrap(), 1);
}

#[test]
fn prefix_lookup_and_admission_are_bounded() {
    let db = TempDb::new("prefix-admission");
    let s = Store::open(&db.0).unwrap();
    s.enqueue(id(7), b"object", call("M0CCC"), 0, 10).unwrap();
    assert_eq!(
        s.object_with_prefix(id(7).prefix8()).unwrap(),
        Some((id(7), b"object".to_vec()))
    );
    let limits = AdmissionLimits {
        max_count: 2,
        max_bytes: 10,
    };
    assert!(limits.admits(HoldingUsage { count: 1, bytes: 3 }, 7));
    assert!(!limits.admits(HoldingUsage { count: 2, bytes: 0 }, 1));
    assert!(!limits.admits(HoldingUsage { count: 1, bytes: 4 }, 7));
}

#[test]
fn a_held_message_outlasts_its_retries_and_wakes_when_its_destination_is_heard() {
    let db = TempDb::new("hold");
    let s = Store::open(&db.0).unwrap();
    let policy = RetryPolicy {
        first_delay_secs: 10,
        max_delay_secs: 25,
        max_attempts: 2,
    };
    let destination = call("SO5KM");
    s.enqueue(id(1), b"a", destination, 0, 0).unwrap();
    s.enqueue(id(2), b"b", call("M0CCC"), 0, 0).unwrap();
    assert_eq!(
        s.attempt_failed_or_hold(id(1), "no answer", policy, 100, true)
            .unwrap(),
        (Retry::At(110), None)
    );
    assert_eq!(
        s.attempt_failed_or_hold(id(1), "no answer", policy, 110, true)
            .unwrap(),
        (Retry::At(135), None),
        "retries used up: held, tried again after the longest delay"
    );
    assert_eq!(
        s.attempt_failed_or_hold(id(1), "no answer", policy, 135, true)
            .unwrap(),
        (Retry::At(160), None)
    );
    let r = s.record(id(1)).unwrap().unwrap();
    assert_eq!(r.state, State::Queued);
    assert_eq!(r.note.as_deref(), Some("no answer; held until it expires"));

    for at in [120, 130] {
        s.attempt_failed_or_hold(id(2), "no answer", policy, at, true)
            .unwrap();
    }
    assert!(s.due(140).unwrap().is_empty());
    assert_eq!(s.wake(destination, 140).unwrap(), 1, "only its own messages");
    let due: Vec<ObjectId> = s.due(140).unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(due, vec![id(1)]);
    assert_eq!(s.wake(destination, 140).unwrap(), 0, "already due");

    // Without a hold the same history gives up.
    s.enqueue(id(3), b"c", destination, 0, 0).unwrap();
    s.attempt_failed_or_hold(id(3), "no answer", policy, 100, false)
        .unwrap();
    assert_eq!(
        s.attempt_failed_or_hold(id(3), "no answer", policy, 110, false)
            .unwrap(),
        (Retry::GaveUp, None)
    );
    assert_eq!(
        s.wake(destination, 200).unwrap(),
        0,
        "a failed message stays failed"
    );
    assert_eq!(s.record(id(3)).unwrap().unwrap().state, State::Failed);
}

#[test]
fn a_station_holds_something_while_mail_waits_or_its_bulletins_live() {
    let db = TempDb::new("holds");
    let s = Store::open(&db.0).unwrap();
    assert!(!s.holds_for_others(100).unwrap());
    s.enqueue(id(1), b"mail", call("SO5KM"), 0, 100).unwrap();
    assert!(s.holds_for_others(100).unwrap());
    s.delivered(id(1), true, "radio", 110).unwrap();
    assert!(!s.holds_for_others(110).unwrap());

    let all = call("ALL");
    s.enqueue_with(
        id(2),
        b"bulletin",
        EnqueueOpts {
            to: all,
            precedence: 0,
            now: 200,
            wire_seq: None,
            expires_at: Some(1_000),
        },
    )
    .unwrap();
    s.delivered(id(2), true, "radio", 210).unwrap();
    assert!(s.holds_for_others(999).unwrap(), "a live bulletin can be pulled");
    assert_eq!(s.holding_ids(call("M0AAA"), false, 999).unwrap(), vec![id(2)]);
    assert!(!s.holds_for_others(1_000).unwrap(), "an expired one cannot");
    assert!(s.holding_ids(call("M0AAA"), false, 1_000).unwrap().is_empty());

    // Stored by an older version without its expiry: a day, as bulletins live.
    s.enqueue(id(3), b"old bulletin", all, 0, 2_000).unwrap();
    s.delivered(id(3), true, "radio", 2_010).unwrap();
    assert!(s.holds_for_others(2_000 + 24 * 3600 - 1).unwrap());
    assert!(!s.holds_for_others(2_000 + 24 * 3600).unwrap());
}
