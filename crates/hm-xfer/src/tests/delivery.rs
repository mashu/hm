//! One object over a clean or lossy link: delivered once, receipts only
//! for what the application kept, garbage refused.

use super::*;

#[test]
fn one_clean_over_delivers() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let object: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object: object.clone(),
            precedence: 0,
        }),
        &mut out,
    );
    let burst = frames(&out);
    // OFFER + K=5 symbols and one to spare: nothing is known of the link, so
    // the radio population's loss (about 15 %) is expected, and one more frame
    // costs less airtime than the turnaround it is likely to save.
    assert_eq!(burst.len(), 1 + 6);
    let b_out = deliver(&mut b, Millis(20_000), &burst);
    assert_eq!(
        events(&b_out),
        vec![Event::Received {
            from: call("SA0KAM"),
            id: object_id(&object),
            object: object.clone()
        }]
    );
    let ack = frames(&b_out);
    assert_eq!(ack.len(), 1);
    let mut out = Vec::new();
    a.handle(
        Millis(25_000),
        Input::Frame {
            port: 0,
            data: ack[0].clone(),
        },
        &mut out,
    );
    assert_eq!(
        events(&out),
        vec![Event::Delivered {
            to: call("SO5KM-1"),
            id: object_id(&object),
            rounds: 1,
            receipt: Receipt::Unverified,
        }]
    );
    assert_eq!(a.outgoing_count(), 0);
}

#[test]
fn custody_receipt_waits_for_durable_application_acceptance() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM");
    b.set_application_ack(true);
    let object = b"persist me first".to_vec();
    let id = object_id(&object);
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM"),
            object: object.clone(),
            precedence: 0,
        }),
        &mut out,
    );
    let received = deliver(&mut b, Millis(20_000), &frames(&out));
    assert_eq!(
        events(&received),
        vec![Event::Received {
            from: call("SA0KAM"),
            id,
            object,
        }]
    );
    let mut sender = Vec::new();
    for frame in frames(&received) {
        a.handle(Millis(21_000), Input::Frame { port: 0, data: frame }, &mut sender);
    }
    assert!(
        events(&sender).is_empty(),
        "no custody receipt before persistence"
    );

    let mut accepted = Vec::new();
    b.handle(
        Millis(22_000),
        Input::Command(Command::Accept {
            from: call("SA0KAM"),
            id,
            accepted: true,
            retry_after: 0,
        }),
        &mut accepted,
    );
    let deadline = b.next_deadline().unwrap();
    b.on_deadline(deadline, &mut accepted);
    assert_eq!(frames(&accepted).len(), 1);
    let mut sender = Vec::new();
    a.handle(
        Millis(23_000),
        Input::Frame {
            port: 0,
            data: frames(&accepted)[0].clone(),
        },
        &mut sender,
    );
    assert!(matches!(
        events(&sender).as_slice(),
        [Event::Delivered { id: delivered, .. }] if *delivered == id
    ));
}

#[test]
fn missed_offer_is_requested_then_delivered() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let object = vec![42u8; 900];
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object: object.clone(),
            precedence: 0,
        }),
        &mut out,
    );
    let burst = frames(&out);
    // Lose the OFFER: B decodes the symbols but cannot verify them yet.
    let b_out = deliver(&mut b, Millis(20_000), &burst[1..]);
    assert!(events(&b_out).is_empty());
    let acks = frames(&b_out);
    let (_, ack) = FrameHeader::decode(&acks[0]).unwrap();
    assert_eq!(Ack::decode(ack).unwrap().need, NEED_OFFER);
    let mut out = Vec::new();
    a.handle(
        Millis(25_000),
        Input::Frame {
            port: 0,
            data: frames(&b_out)[0].clone(),
        },
        &mut out,
    );
    let second = frames(&out);
    let b_out = deliver(&mut b, Millis(40_000), &second);
    assert!(matches!(events(&b_out)[..], [Event::Received { .. }]));
}

#[test]
fn a_resent_object_is_acknowledged_but_delivered_once() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let object = vec![7u8; 500];
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object: object.clone(),
            precedence: 0,
        }),
        &mut out,
    );
    let burst = frames(&out);
    let first = deliver(&mut b, Millis(10_000), &burst);
    assert_eq!(events(&first).len(), 1);
    // The ACK is lost; A times out, backs off, then probes; B answers "done"
    // without a second delivery.
    let timeout = a.next_deadline().unwrap();
    let mut out = Vec::new();
    a.on_deadline(timeout, &mut out);
    let t = a.next_deadline().unwrap();
    assert!(t >= timeout, "probe waits for the random backoff");
    a.on_deadline(t, &mut out);
    let probe = frames(&out);
    assert!(probe.len() <= 3, "probe is an OFFER plus at most two symbols");
    let again = deliver(&mut b, t + Millis(5_000), &probe);
    assert!(events(&again).is_empty());
    let mut out = Vec::new();
    a.handle(
        t + Millis(9_000),
        Input::Frame {
            port: 0,
            data: frames(&again)[0].clone(),
        },
        &mut out,
    );
    assert!(matches!(events(&out)[..], [Event::Delivered { rounds: 2, .. }]));
}

#[test]
fn corrupted_symbol_is_caught_by_the_hash() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let object: Vec<u8> = (0..2000u32).map(|i| i as u8).collect();
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object: object.clone(),
            precedence: 0,
        }),
        &mut out,
    );
    let mut burst = frames(&out);
    let last = burst.len() - 1;
    burst[2][40] ^= 0x01; // flip one bit inside a symbol, header intact
    let b_out = deliver(&mut b, Millis(40_000), &burst[..last]);
    assert!(
        events(&b_out).is_empty(),
        "never deliver data that fails the hash"
    );
    let acks = frames(&b_out);
    let (_, ack) = FrameHeader::decode(&acks[0]).unwrap();
    assert!(Ack::decode(ack).unwrap().need > 0);
}

/// Garbage and mutated frames must never panic the engine (the RaptorQ crate
/// asserts on bad parameters, so every value from the air is checked first).
#[test]
fn hostile_frames_never_panic() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object: vec![5; 3000],
            precedence: 0,
        }),
        &mut out,
    );
    let good = frames(&out);
    let mut g = DetRng::from_seed(77);
    let mut now = Millis(0);
    for _ in 0..20_000 {
        let mut f = good[g.below(good.len() as u64) as usize].clone();
        match g.below(4) {
            0 => {
                let i = g.below(f.len() as u64) as usize;
                f[i] ^= 1 << g.below(8);
            }
            1 => f.truncate(g.below(f.len() as u64 + 1) as usize),
            2 => {
                // Random object length and symbol size in DATA preamble / OFFER fields.
                if f.len() > 24 {
                    for b in &mut f[18..24] {
                        *b = g.next_u64() as u8;
                    }
                }
            }
            _ => f = (0..g.below(300)).map(|_| g.next_u64() as u8).collect(),
        }
        now += Millis(g.below(3_000));
        let mut out = Vec::new();
        b.handle(now, Input::Frame { port: 0, data: f }, &mut out);
        if let Some(t) = b.next_deadline() {
            if t <= now {
                b.on_deadline(now, &mut out);
            }
        }
        assert!(b.incoming.len() <= b.cfg.max_incoming);
    }
}
