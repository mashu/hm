//! The database itself: old records, belief records, readers beside a writer.

use super::*;

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
fn belief_records_survive_restart_and_can_be_deleted() {
    let db = TempDb::new("beliefs");
    {
        let store = Store::open(&db.0).unwrap();
        store
            .save_beliefs(&[(vec![1, 2, 3], Some(vec![9, 9])), (vec![4], Some(vec![8]))])
            .unwrap();
        store.save_beliefs(&[(vec![4], None)]).unwrap();
    }
    let store = Store::open(&db.0).unwrap();
    assert_eq!(store.beliefs().unwrap(), vec![(vec![1, 2, 3], vec![9, 9])]);
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
