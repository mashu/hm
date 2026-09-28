use super::*;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn id(n: u8) -> ObjectId {
    ObjectId([n; 32])
}

struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p = std::env::temp_dir().join(format!("hm-store-{}-{}.db", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        TempDb(p)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

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
        Retry::At(110)
    );
    assert!(s.due(105).unwrap().is_empty(), "not due before the delay");
    assert_eq!(s.due(110).unwrap().len(), 1);
    assert_eq!(
        s.attempt_failed(id(2), "no answer", policy, 110).unwrap(),
        Retry::At(130)
    );
    assert_eq!(
        s.attempt_failed(id(2), "no answer", policy, 130).unwrap(),
        Retry::At(155),
        "capped at 25 s"
    );
    assert_eq!(
        s.attempt_failed(id(2), "no answer", policy, 155).unwrap(),
        Retry::GaveUp
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
    };
    assert!(s.enqueue_relay(id(1), b"bundle", metadata, 100).unwrap());
    assert_eq!(s.relay_usage(100).unwrap(), HoldingUsage { count: 1, bytes: 6 });
    assert_eq!(s.holding_ids(destination, false, 100).unwrap(), vec![id(1)]);
    assert_eq!(s.holding_ids(relay, true, 100).unwrap(), vec![id(1)]);
    assert!(s.set_next_hop(id(1), relay).unwrap());
    assert!(s.custody_transferred(id(1), relay, true, "radio", 110).unwrap());
    let record = s.record(id(1)).unwrap().unwrap();
    assert_eq!(record.state, State::InTransit);
    assert_eq!(record.custody_by, Some(relay));
    assert_eq!(s.relay_usage(110).unwrap().count, 0);
    assert!(s.due(110).unwrap().is_empty());
}

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
        .custody_transferred(id(2), first, true, "radio", 20)
        .unwrap());
    assert!(store
        .custody_transferred(id(2), second, true, "internet", 21)
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
    assert!(!s.custody_transferred(id(1), relay, false, "radio", 20).unwrap());
    let record = s.record(id(1)).unwrap().unwrap();
    assert_eq!(record.state, State::Queued);
    assert!(record.custody_by.is_none());
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
    assert!(!s.custody_transferred(id(1), relay, true, "radio", 20).unwrap());
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
        .custody_transferred(id(2), relay, true, "radio", 21)
        .unwrap());
    assert_eq!(store.delete(id(2)).unwrap(), DeleteOutcome::Active);
    assert_eq!(store.record(id(2)).unwrap().unwrap().state, State::InTransit);
}

#[test]
fn deletion_keeps_an_e2e_receipt_object_while_it_is_referenced() {
    let db = TempDb::new("delete-references");
    let store = Store::open(&db.0).unwrap();
    let destination = call("M0CCC");
    store
        .put_received(id(9), b"receipt", destination, true, 20)
        .unwrap();
    store.enqueue(id(1), b"message", destination, 0, 10).unwrap();
    assert!(store.e2e_delivered(id(1), id(9), destination, 21).unwrap());

    assert_eq!(store.delete(id(9)).unwrap(), DeleteOutcome::Deleted);
    assert!(store.record(id(9)).unwrap().is_none());
    assert_eq!(store.object(id(9)).unwrap().as_deref(), Some(&b"receipt"[..]));

    assert_eq!(store.delete(id(1)).unwrap(), DeleteOutcome::Deleted);
    assert!(store.object(id(1)).unwrap().is_none());
    assert!(store.object(id(9)).unwrap().is_none());
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
fn legacy_records_decode_with_empty_relay_metadata() {
    #[derive(Encode)]
    #[cbor(map)]
    struct LegacyRecord {
        #[n(0)]
        id: ObjectId,
        #[n(1)]
        direction: Direction,
        #[n(2)]
        peer: Callsign,
        #[n(3)]
        at: u64,
        #[n(4)]
        state: State,
        #[n(5)]
        precedence: u8,
        #[n(6)]
        attempts: u32,
        #[n(7)]
        next_attempt: u64,
        #[n(8)]
        verified: bool,
        #[n(9)]
        seq: u64,
    }
    let bytes = minicbor::to_vec(LegacyRecord {
        id: id(1),
        direction: Direction::Out,
        peer: call("M0CCC"),
        at: 1,
        state: State::Queued,
        precedence: 0,
        attempts: 0,
        next_attempt: 1,
        verified: false,
        seq: 1,
    })
    .unwrap();
    let record = decode(&bytes).unwrap();
    assert_eq!(record.final_peer, None);
    assert_eq!(record.final_destination(), call("M0CCC"));
    assert_eq!(record.max_hops, None);
}

#[test]
fn bayesian_contact_evidence_survives_restart() {
    let db = TempDb::new("contact-evidence");
    let key = EdgeKey {
        from: call("M0AAA"),
        to: call("M0BBB"),
        bearer: Bearer::Radio,
        utc_hour: Some(17),
    };
    let evidence = Evidence {
        successes: 4.5,
        failures: 1.25,
        at: 1234,
    };
    {
        let store = Store::open(&db.0).unwrap();
        store.save_contact_evidence(key, evidence).unwrap();
    }
    let store = Store::open(&db.0).unwrap();
    assert_eq!(store.contact_evidence().unwrap(), vec![(key, evidence)]);
}

#[test]
fn read_only_open_works_while_a_writer_holds_the_store() {
    let db = TempDb::new("read-only-concurrent");
    let writer = Store::open(&db.0).unwrap();
    writer.enqueue(id(9), b"hello", call("M0BBB"), 0, 10).unwrap();
    let reader = Store::open_read_only(&db.0).unwrap();
    let list = reader.list(Direction::Out, 10).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(reader.object(id(9)).unwrap().unwrap(), b"hello");
    assert!(matches!(
        reader.enqueue(id(8), b"nope", call("M0CCC"), 0, 11),
        Err(Error::ReadOnly)
    ));
    drop(writer);
}

#[test]
fn read_only_open_works_when_no_writer_holds_the_store() {
    let db = TempDb::new("read-only-alone");
    {
        let writer = Store::open(&db.0).unwrap();
        writer.enqueue(id(9), b"hello", call("M0BBB"), 0, 10).unwrap();
    }
    let reader = Store::open_read_only(&db.0).unwrap();
    let list = reader.list(Direction::Out, 10).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(reader.object(id(9)).unwrap().unwrap(), b"hello");
}

#[test]
fn final_delivery_and_e2e_receipt_queue_are_atomic_and_idempotent() {
    let db = TempDb::new("receive-reply");
    let store = Store::open(&db.0).unwrap();
    let received = ReceivedMessage {
        id: id(1),
        object: b"message",
        from: call("M0AAA"),
        verified: true,
    };
    let reply = QueuedMessage {
        id: id(2),
        object: b"receipt",
        to: call("M0AAA"),
        precedence: 1,
        expires_at: 1_000,
        max_hops: 8,
    };
    assert!(store.receive_with_reply(received, reply, 100).unwrap());
    assert!(!store.receive_with_reply(received, reply, 101).unwrap());
    assert_eq!(store.list(Direction::In, 10).unwrap().len(), 1);
    assert_eq!(store.list(Direction::Out, 10).unwrap().len(), 1);
    assert_eq!(store.due(100).unwrap()[0].id, id(2));
}
