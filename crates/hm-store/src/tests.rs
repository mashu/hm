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
