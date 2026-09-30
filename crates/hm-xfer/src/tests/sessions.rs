//! Opening a session and the receiver's answers: too large, busy, refused.

use super::*;

#[test]
fn the_first_over_opens_a_session_and_the_answer_opens_it_back() {
    let mut a = session_engine("SA0KAM", |c| c.features = hm_wire::FEATURE_MAILBOX);
    let mut b = session_engine("SO5KM-1", |c| c.max_object_len = 64 * 1024);
    let burst = frames(&send(&mut a, Millis(0), "SO5KM-1", vec![1; 1000]));
    // OPEN, then OFFER, then the symbols.
    assert_eq!(ctrl_type(&burst[0]), Some(CTRL_OPEN));
    assert_eq!(ctrl_type(&burst[1]), Some(CTRL_OFFER));
    let b_out = deliver(&mut b, Millis(20_000), &burst);
    let seen = b.peer(call("SA0KAM")).expect("B heard A's OPEN");
    assert_eq!(
        (seen.features, seen.is_reply()),
        (hm_wire::FEATURE_MAILBOX, false)
    );
    // B answers with its OPEN (a reply) in front of its ACK.
    let answer = frames(&b_out);
    assert_eq!(answer.len(), 2);
    assert_eq!(ctrl_type(&answer[0]), Some(CTRL_OPEN));
    let mut out = Vec::new();
    for f in &answer {
        a.handle(
            Millis(25_000),
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut out,
        );
    }
    assert!(matches!(events(&out)[..], [Event::Delivered { .. }]));
    let theirs = a.peer(call("SO5KM-1")).unwrap();
    assert!(theirs.is_reply());
    assert_eq!((theirs.max_object, theirs.max_parallel), (64 * 1024, 2));
    // The next transfer needs no OPEN, and B's answer carries none.
    let burst = frames(&send(&mut a, Millis(30_000), "SO5KM-1", vec![2; 1000]));
    assert_eq!(ctrl_type(&burst[0]), Some(CTRL_OFFER));
    let answer = frames(&deliver(&mut b, Millis(50_000), &burst));
    assert_eq!(answer.len(), 1);
}

#[test]
fn a_receiver_says_when_an_object_is_too_large() {
    let mut a = session_engine("SA0KAM", |_| {});
    let mut b = session_engine("SO5KM-1", |c| c.max_object_len = 2000);
    let burst = frames(&send(&mut a, Millis(0), "SO5KM-1", vec![3; 5000]));
    let answer = frames(&deliver(&mut b, Millis(20_000), &burst));
    let close = answer
        .iter()
        .find(|f| ctrl_type(f) == Some(CTRL_CLOSE))
        .expect("a CLOSE");
    let (h, p) = FrameHeader::decode(close).unwrap();
    assert_eq!(Close::decode(p).unwrap().reason, CloseReason::TooLarge);
    assert_eq!(h.session, FrameHeader::decode(&burst[0]).unwrap().0.session);
    let mut out = Vec::new();
    for f in &answer {
        a.handle(
            Millis(40_000),
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut out,
        );
    }
    // One over, then a clear answer instead of eleven more.
    assert!(matches!(
        events(&out)[..],
        [Event::Failed {
            reason: Failure::TooLarge,
            ..
        }]
    ));
    // Knowing B's limit, the next large object fails without going on air.
    let out = send(&mut a, Millis(50_000), "SO5KM-1", vec![4; 3000]);
    assert!(frames(&out).is_empty());
    assert!(matches!(
        events(&out)[..],
        [Event::Failed {
            reason: Failure::TooLarge,
            ..
        }]
    ));
    // A small one still goes.
    assert!(!frames(&send(&mut a, Millis(51_000), "SO5KM-1", vec![5; 500])).is_empty());
}

#[test]
fn a_busy_receiver_asks_a_new_sender_to_come_back() {
    let mut b = session_engine("SO5KM-1", |c| {
        c.max_incoming = 1;
        c.busy_retry_secs = 90;
    });
    let mut c = session_engine("SP5AAA", |_| {});
    let mut a = session_engine("SA0KAM", |_| {});
    // C's transfer takes B's only slot, and is still going (half its symbols).
    let mut c_burst = frames(&send(&mut c, Millis(0), "SO5KM-1", vec![6; 4000]));
    c_burst.truncate(8);
    deliver(&mut b, Millis(20_000), &c_burst);
    // A offers: B keeps C's work and tells A it is busy for 90 s.
    let burst = frames(&send(&mut a, Millis(30_000), "SO5KM-1", vec![7; 1000]));
    let answer = frames(&deliver(&mut b, Millis(50_000), &burst));
    let busy = answer
        .iter()
        .filter(|f| FrameHeader::decode(f).unwrap().0.dst == Dest::Station(call("SA0KAM")))
        .find_map(|f| {
            let (h, p) = FrameHeader::decode(f).unwrap();
            (h.ftype == FrameType::Ctrl && p[0] == CTRL_CLOSE).then(|| Close::decode(p).unwrap())
        })
        .expect("a CLOSE to A");
    assert_eq!((busy.reason, busy.retry_after), (CloseReason::Busy, 90));
    assert!(b.incoming.keys().all(|(from, _)| *from == call("SP5AAA")));
    let mut out = Vec::new();
    for f in &answer {
        a.handle(
            Millis(60_000),
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut out,
        );
    }
    assert!(events(&out).is_empty(), "busy is not a failure");
    // A stays quiet until the time B asked for, then offers again.
    assert_eq!(a.next_deadline(), Some(Millis(150_000)));
    let mut out = Vec::new();
    a.on_deadline(Millis(150_000), &mut out);
    assert_eq!(ctrl_type(&frames(&out)[0]), Some(CTRL_OFFER));
}

#[test]
fn a_refusal_ends_the_transfer() {
    let mut a = session_engine("SA0KAM", |_| {});
    let burst = frames(&send(&mut a, Millis(0), "SO5KM-1", vec![8; 800]));
    let session = FrameHeader::decode(&burst[0]).unwrap().0.session;
    let refuse = FrameHeader {
        ftype: FrameType::Ctrl,
        src: call("SO5KM-1"),
        dst: Dest::Station(call("SA0KAM")),
        session,
        index: 0,
    }
    .frame(
        &Close {
            reason: CloseReason::Refused,
            retry_after: 0,
        }
        .to_bytes(),
    )
    .unwrap();
    let mut out = Vec::new();
    a.handle(
        Millis(20_000),
        Input::Frame {
            port: 0,
            data: refuse,
        },
        &mut out,
    );
    assert!(matches!(
        events(&out)[..],
        [Event::Failed {
            reason: Failure::Refused,
            ..
        }]
    ));
    assert_eq!(a.outgoing_count(), 0);
}

#[test]
fn a_corrupted_offer_length_does_not_end_the_transfer() {
    let mut a = session_engine("SA0KAM", |_| {});
    let mut b = session_engine("SO5KM-1", |c| c.max_object_len = 2000);
    let mut burst = frames(&send(&mut a, Millis(0), "SO5KM-1", vec![3; 1000]));
    // The OFFER's length arrives garbled as 0xFF.... The DATA frames say 1000.
    let i = burst
        .iter()
        .position(|f| ctrl_type(f) == Some(CTRL_OFFER))
        .unwrap();
    burst[i][HEADER_LEN + 33] = 0xFF;
    let answer = frames(&deliver(&mut b, Millis(20_000), &burst));
    assert!(
        answer.iter().all(|f| ctrl_type(f) != Some(CTRL_CLOSE)),
        "no CLOSE for a length the DATA contradicts"
    );
    // And a sender told "too large" by a peer whose OPEN says it fits offers again.
    let session = FrameHeader::decode(&burst[0]).unwrap().0.session;
    let mut out = Vec::new();
    for f in answer.iter().filter(|f| ctrl_type(f) == Some(CTRL_OPEN)) {
        a.handle(
            Millis(30_000),
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut out,
        );
    }
    let close = FrameHeader {
        ftype: FrameType::Ctrl,
        src: call("SO5KM-1"),
        dst: Dest::Station(call("SA0KAM")),
        session,
        index: 0,
    }
    .frame(
        &Close {
            reason: CloseReason::TooLarge,
            retry_after: 0,
        }
        .to_bytes(),
    )
    .unwrap();
    let mut out = Vec::new();
    a.handle(Millis(31_000), Input::Frame { port: 0, data: close }, &mut out);
    assert!(events(&out).is_empty());
    assert!(
        frames(&out).iter().any(|f| ctrl_type(f) == Some(CTRL_OFFER)),
        "offered again"
    );
}
