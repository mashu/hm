//! Bulletins to listeners that never ACK, and repair overs for what they
//! missed.

use super::*;

#[test]
fn broadcast_reaches_listeners_without_acks() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let mut c = engine("SP5AAA");
    let object: Vec<u8> = (0..400u32).map(|i| (i * 3) as u8).collect();
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Broadcast {
            object: object.clone(),
            precedence: 0,
        }),
        &mut out,
    );
    let burst = frames(&out);
    assert!(!burst.is_empty());
    for f in &burst {
        let (h, _) = FrameHeader::decode(f).unwrap();
        assert_eq!(h.dst, Dest::Broadcast);
        assert_eq!(h.src, call("SA0KAM"));
    }
    // The publisher listens for repair requests a while, then is done.
    assert!(events(&out).is_empty());
    assert_eq!(a.outgoing_count(), 1);

    let want = Event::Received {
        from: call("SA0KAM"),
        id: object_id(&object),
        object: object.clone(),
    };
    let b_out = deliver(&mut b, Millis(20_000), &burst);
    assert_eq!(events(&b_out), vec![want.clone()]);
    assert!(
        frames(&b_out).is_empty(),
        "a listener with everything asks for nothing"
    );
    let c_out = deliver(&mut c, Millis(20_000), &burst);
    assert_eq!(events(&c_out), vec![want]);
    assert!(frames(&c_out).is_empty());

    // Two quiet repair windows (a request may have been lost in a collision).
    let mut done = Vec::new();
    for _ in 0..2 {
        let t = a.next_deadline().unwrap();
        a.on_deadline(t, &mut done);
    }
    assert_eq!(
        events(&done),
        vec![Event::Delivered {
            to: broadcast_peer(),
            id: object_id(&object),
            rounds: 1,
            receipt: Receipt::Unverified,
        }]
    );
    assert!(frames(&done).is_empty(), "nobody asked: no repair");
    assert_eq!(a.outgoing_count(), 0);
}

/// A listener that missed symbols asks once the over ends; another listener
/// missing as much hears that request and keeps quiet; the publisher answers
/// with fresh symbols, which complete both.
#[test]
fn listeners_missing_symbols_get_a_repair_over() {
    let mut a = engine("SA0KAM");
    let (mut b, mut c) = (engine("SO5KM-1"), engine("SP5AAA"));
    let object: Vec<u8> = (0..3_000u32).map(|i| (i * 7) as u8).collect();
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Broadcast {
            object: object.clone(),
            precedence: 0,
        }),
        &mut out,
    );
    // One over carries every source symbol and then some; then it listens.
    let sent = frames(&out);
    assert!(events(&out).is_empty());
    // Both listeners lose the same half of the DATA frames.
    let heard: Vec<Vec<u8>> = sent
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            let (h, _) = FrameHeader::decode(f).unwrap();
            h.ftype != FrameType::Data || i % 2 == 0
        })
        .map(|(_, f)| f.clone())
        .collect();
    let b_out = deliver(&mut b, Millis(60_000), &heard);
    assert!(events(&b_out).is_empty(), "not enough symbols yet");
    let nack = frames(&b_out);
    assert_eq!(nack.len(), 1, "one request");
    let (h, payload) = FrameHeader::decode(&nack[0]).unwrap();
    assert_eq!((h.ftype, h.dst), (FrameType::Ack, Dest::Station(call("SA0KAM"))));
    assert!(Ack::decode(payload).unwrap().need > 0);

    // D heard the same over but not B, so it asks by itself.
    let mut d = engine("SP5DDD");
    let d_out = deliver(&mut d, Millis(60_000), &heard);
    let d_nack = frames(&d_out);
    assert_eq!(d_nack.len(), 1);

    // C heard the same over, then both requests before its own moment came:
    // two requests for as much, so C keeps quiet. (One could have been lost
    // on the way to the publisher, so one alone would not silence it.)
    let mut c_out = Vec::new();
    for f in heard.iter().chain(nack.iter()) {
        c.handle(
            Millis(60_000),
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut c_out,
        );
    }
    let mut c_alone = engine("SP5AAA");
    let mut alone_out = Vec::new();
    for f in heard.iter().chain(nack.iter()) {
        c_alone.handle(
            Millis(60_000),
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut alone_out,
        );
    }
    c_alone.on_deadline(Millis(80_000), &mut alone_out);
    assert_eq!(frames(&alone_out).len(), 1, "one request heard: ask anyway");
    c.handle(
        Millis(60_000),
        Input::Frame {
            port: 0,
            data: d_nack[0].clone(),
        },
        &mut c_out,
    );
    // Past the moment C would have asked (the whole spread after the over).
    c.on_deadline(Millis(80_000), &mut c_out);
    assert!(frames(&c_out).is_empty(), "two asked for as much: C keeps quiet");

    // The publisher answers after its repair window with fresh symbols.
    let mut repair = Vec::new();
    a.handle(
        Millis(60_001),
        Input::Frame {
            port: 0,
            data: nack[0].clone(),
        },
        &mut repair,
    );
    while frames(&repair)
        .iter()
        .all(|f| FrameHeader::decode(f).unwrap().0.ftype != FrameType::Data)
    {
        let t = a.next_deadline().unwrap();
        a.on_deadline(t.max(Millis(60_002)), &mut repair);
    }
    let fresh = frames(&repair);
    let b_done = deliver(&mut b, Millis(90_000), &fresh);

    let c_done = deliver(&mut c, Millis(90_000), &fresh);
    for done in [&b_done, &c_done] {
        assert!(
            events(done)
                .iter()
                .any(|e| matches!(e, Event::Received { object: got, .. } if *got == object)),
            "the repair completes the object"
        );
    }
}
