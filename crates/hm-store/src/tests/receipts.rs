//! End-to-end receipts: queued with final delivery in one step, kept while
//! referenced, answered again for a duplicate once the first is gone.

use super::*;

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
fn final_delivery_and_e2e_receipt_queue_are_atomic_and_idempotent() {
    let db = TempDb::new("receive-reply");
    let store = Store::open(&db.0).unwrap();
    let received = ReceivedMessage {
        id: id(1),
        object: b"message",
        from: call("M0AAA"),
        verified: true,
        wire_seq: None,
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

/// A message received again was sent again because its sender never saw the
/// receipt: it is answered again, but only once the first receipt is no
/// longer on its way.
#[test]
fn a_duplicate_is_answered_again_once_the_first_receipt_is_gone() {
    let db = TempDb::new("answer-again");
    let store = Store::open(&db.0).unwrap();
    let origin = call("M0AAA");
    let received = ReceivedMessage {
        id: id(1),
        object: b"message",
        from: origin,
        verified: true,
        wire_seq: None,
    };
    let reply = |n| QueuedMessage {
        id: id(n),
        object: b"receipt",
        to: origin,
        precedence: 1,
        expires_at: 100_000,
        max_hops: 8,
    };
    assert!(store.receive_with_reply(received, reply(2), 100).unwrap());
    // Still queued: the second copy gets no second receipt.
    assert!(!store.receive_with_reply(received, reply(3), 200).unwrap());
    assert!(store.record(id(3)).unwrap().is_none());
    // The first receipt handed on and done with: the next copy is answered.
    assert!(store
        .custody_transferred(
            id(2),
            CustodyHandoff {
                next_hop: origin,
                receipt_verified: true,
                by: "radio",
                now: 300,
                grace_secs: 60,
                suspect_secs: 600,
                eta: 300,
                awaits_receipt: false,
            },
        )
        .unwrap());
    assert_eq!(store.record(id(2)).unwrap().unwrap().state, State::Delivered);
    assert!(!store.receive_with_reply(received, reply(4), 400).unwrap());
    assert_eq!(store.record(id(4)).unwrap().unwrap().state, State::Queued);
    assert_eq!(store.list(Direction::In, 10).unwrap().len(), 1);
}
