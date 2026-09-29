use super::*;
use alloc::vec;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

/// A fixed key per callsign, so tests can trust each other's keys.
fn identity(me: &str) -> Identity {
    let mut secret = [0x5Au8; 32];
    secret[..8].copy_from_slice(&call(me).packed().to_be_bytes());
    Identity::from_secret(secret)
}

fn engine(me: &str) -> Xfer {
    let mut cfg = Config::vhf_1200(call(me));
    cfg.duty_cycle_permille = 1000;
    // These tests look at OFFER, DATA and ACK frames one by one; sessions
    // (OPEN before the first over) have tests of their own.
    cfg.sessions = false;
    Xfer::new(cfg, identity(me), DetRng::from_seed(1)).unwrap()
}

fn frames(out: &[Output<Event>]) -> Vec<Vec<u8>> {
    out.iter()
        .filter_map(|o| match o {
            Output::Transmit { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect()
}

fn events(out: &[Output<Event>]) -> Vec<Event> {
    out.iter()
        .filter_map(|o| match o {
            Output::Event(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

/// Deliver `fs` to `rx` in order, then run its deadlines until quiet; returns its outputs.
fn deliver(rx: &mut Xfer, now: Millis, fs: &[Vec<u8>]) -> Vec<Output<Event>> {
    let mut out = Vec::new();
    for f in fs {
        rx.handle(
            now,
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut out,
        );
    }
    let t = rx.next_deadline().unwrap();
    rx.on_deadline(t, &mut out);
    out
}

#[test]
fn rejects_bad_config() {
    let mut cfg = Config::vhf_1200(call("SA0KAM"));
    cfg.symbol_size = 100;
    assert!(Xfer::new(cfg, identity("SA0KAM"), DetRng::from_seed(0)).is_err());
}

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
    // OFFER + K=5 symbols: at 2% assumed loss, 5 of 5 arrive with probability 0.904.
    assert_eq!(burst.len(), 1 + 5);
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

#[test]
fn precedence_orders_the_queue_and_bad_sends_fail_fast() {
    let mut a = engine("SA0KAM");
    let mut out = Vec::new();
    let to = call("SO5KM-1");
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to,
            object: vec![1; 10],
            precedence: 0,
        }),
        &mut out,
    );
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to,
            object: vec![2; 10],
            precedence: 0,
        }),
        &mut out,
    );
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to,
            object: vec![3; 10],
            precedence: 3,
        }),
        &mut out,
    );
    let order: Vec<u8> = a.queue.iter().map(|p| p.object[0]).collect();
    assert_eq!(
        order,
        vec![3, 2],
        "flash goes ahead of routine; the first one is already active"
    );
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to,
            object: vec![],
            precedence: 0,
        }),
        &mut out,
    );
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SA0KAM"),
            object: vec![1],
            precedence: 0,
        }),
        &mut out,
    );
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to,
            object: vec![0; 300 * 1024],
            precedence: 0,
        }),
        &mut out,
    );
    let reasons: Vec<Failure> = events(&out)
        .into_iter()
        .map(|e| match e {
            Event::Failed { reason, .. } => reason,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        reasons,
        vec![Failure::Empty, Failure::SelfAddressed, Failure::TooLarge]
    );
}

#[test]
fn duty_cycle_limits_over_size_and_spacing() {
    let mut cfg = Config::vhf_1200(call("SA0KAM"));
    cfg.duty_cycle_permille = 100;
    cfg.bucket = Millis(5_500);
    cfg.sessions = false;
    let mut a = Xfer::new(cfg, identity("SA0KAM"), DetRng::from_seed(1)).unwrap();
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object: vec![9; 4000],
            precedence: 0,
        }),
        &mut out,
    );
    // A 5.5 s bucket holds TXDELAY + OFFER (0.52 s) + two 222-byte frames
    // (1.64 s each on air as AX.25), not the 20 wanted.
    assert_eq!(frames(&out).len(), 1 + 2);
    // The receiver still needs 18 symbols; at 10% duty the next over waits for the bucket to refill.
    let session = FrameHeader::decode(&frames(&out)[0]).unwrap().0.session;
    let ack = Ack {
        need: 18,
        ..Ack::default()
    }
    .to_vec()
    .unwrap();
    let f = FrameHeader {
        ftype: FrameType::Ack,
        src: call("SO5KM-1"),
        dst: Dest::Station(call("SA0KAM")),
        session,
        index: 0,
    }
    .frame(&ack)
    .unwrap();
    let mut out = Vec::new();
    a.handle(Millis(10_000), Input::Frame { port: 0, data: f }, &mut out);
    assert!(frames(&out).is_empty());
    let t = a.next_deadline().unwrap();
    assert!(
        t >= Millis(10_000 + 15_000),
        "next over waits for the budget: {t:?}"
    );
    let mut out = Vec::new();
    a.on_deadline(t, &mut out);
    // No OFFER this time, so three frames fit: 0.3 s + 3 x 1.64 s < 5.5 s.
    assert_eq!(frames(&out).len(), 3);
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

#[test]
fn burst_sizing_meets_the_target_and_no_more() {
    assert_eq!(burst_size(1, 20, 16), 1);
    assert_eq!(burst_size(5, 20, 16), 5);
    assert_eq!(burst_size(9, 20, 16), 10); // 0.98^9 = 0.83 < 0.9
    assert_eq!(burst_size(10, 0, 16), 10);
    assert_eq!(burst_size(40, 100, 16), 16, "capped at max_burst");
    for (need, loss) in [(1, 100), (5, 100), (12, 300), (3, 500)] {
        let n = burst_size(need, loss, 255);
        let q = 1.0 - loss as f64 / 1000.0;
        assert!(prob_at_least(n, need, q) >= BURST_SUCCESS_TARGET, "{need} {loss}");
        assert!(
            prob_at_least(n - 1, need, q) < BURST_SUCCESS_TARGET || n == need,
            "{need} {loss} not minimal"
        );
    }
    // Exact values: P[Bin(2, 0.5) >= 1] = 0.75, P[Bin(3, 0.9) >= 3] = 0.729.
    assert!((prob_at_least(2, 1, 0.5) - 0.75).abs() < 1e-12);
    assert!((prob_at_least(3, 3, 0.9) - 0.729).abs() < 1e-12);
}

/// Run A -> B through one clean over; returns B's final ACK frame and A.
fn one_transfer(a_trusts_b: bool) -> (Xfer, Vec<u8>, ObjectId) {
    let mut a = engine("SA0KAM");
    if a_trusts_b {
        a.trust(call("SO5KM"), identity("SO5KM-1").public());
    }
    let mut b = engine("SO5KM-1");
    let object = vec![0x42u8; 700];
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
    let b_out = deliver(&mut b, Millis(20_000), &frames(&out));
    (a, frames(&b_out).remove(0), object_id(&object))
}

fn ack_payload(frame: &[u8]) -> Ack {
    Ack::decode(FrameHeader::decode(frame).unwrap().1).unwrap()
}

fn with_payload(frame: &[u8], ack: &Ack) -> Vec<u8> {
    let (h, _) = FrameHeader::decode(frame).unwrap();
    h.frame(&ack.to_vec().unwrap()).unwrap()
}

#[test]
fn receipt_from_a_trusted_receiver_is_verified() {
    let (mut a, ack, id) = one_transfer(true);
    let r = ack_payload(&ack).receipt.expect("final ACK carries a receipt");
    let statement = receipt_statement(
        call("SO5KM-1"),
        call("SA0KAM"),
        FrameHeader::decode(&ack).unwrap().0.session,
        &id,
    );
    assert!(identity("SO5KM-1").public().verify(&statement, &r).is_ok());
    let mut out = Vec::new();
    a.handle(Millis(25_000), Input::Frame { port: 0, data: ack }, &mut out);
    assert!(matches!(
        events(&out)[..],
        [Event::Delivered {
            receipt: Receipt::Verified,
            ..
        }]
    ));
}

#[test]
fn forged_completion_is_ignored_when_the_key_is_known() {
    let (mut a, ack, _) = one_transfer(true);
    // An attacker who heard everything replays the ACK with its own signature,
    // then without any receipt.
    let mut forged = ack_payload(&ack);
    forged.receipt = Some(identity("SP5ZZZ").sign(b"anything"));
    let no_receipt = Ack {
        receipt: None,
        ..forged.clone()
    };
    let mut out = Vec::new();
    a.handle(
        Millis(25_000),
        Input::Frame {
            port: 0,
            data: with_payload(&ack, &forged),
        },
        &mut out,
    );
    a.handle(
        Millis(25_100),
        Input::Frame {
            port: 0,
            data: with_payload(&ack, &no_receipt),
        },
        &mut out,
    );
    assert!(events(&out).is_empty(), "no delivery on a forged receipt");
    assert_eq!(a.rejected_receipts(), 2);
    assert_eq!(a.outgoing_count(), 1, "transfer still pending");
    // The genuine ACK still completes it.
    a.handle(Millis(26_000), Input::Frame { port: 0, data: ack }, &mut out);
    assert!(matches!(
        events(&out)[..],
        [Event::Delivered {
            receipt: Receipt::Verified,
            ..
        }]
    ));
}

#[test]
fn completion_without_a_known_key_is_reported_unverified() {
    let (mut a, ack, _) = one_transfer(false);
    let mut stripped = ack_payload(&ack);
    stripped.receipt = None;
    let mut out = Vec::new();
    a.handle(
        Millis(25_000),
        Input::Frame {
            port: 0,
            data: with_payload(&ack, &stripped),
        },
        &mut out,
    );
    assert!(matches!(
        events(&out)[..],
        [Event::Delivered {
            receipt: Receipt::Unverified,
            ..
        }]
    ));
}

#[test]
fn resent_offer_after_completion_gets_the_receipt_again() {
    let mut a = engine("SA0KAM");
    a.trust(call("SO5KM"), identity("SO5KM-1").public());
    let mut b = engine("SO5KM-1");
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object: vec![1; 300],
            precedence: 0,
        }),
        &mut out,
    );
    let burst = frames(&out);
    deliver(&mut b, Millis(10_000), &burst);
    // Final ACK lost; the OFFER comes again and B's answer must still verify.
    let again = deliver(&mut b, Millis(40_000), &burst[..1]);
    let mut out = Vec::new();
    a.handle(
        Millis(45_000),
        Input::Frame {
            port: 0,
            data: frames(&again)[0].clone(),
        },
        &mut out,
    );
    assert!(matches!(
        events(&out)[..],
        [Event::Delivered {
            receipt: Receipt::Verified,
            ..
        }]
    ));
}

#[test]
fn garbage_sessions_cannot_push_out_a_transfer_in_progress() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let object: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
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
    // The first half of the over arrives, with its OFFER.
    let half = burst.len() / 2;
    let mut t = Millis(1_000);
    let mut b_out = Vec::new();
    for f in &burst[..half] {
        b.handle(
            t,
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut b_out,
        );
    }
    // 200 fake stations each open a garbage session with one DATA frame.
    let fake_data = |i: u32| {
        let src = Callsign::parse(&alloc::format!("Q{}X", 1000 + i)).unwrap();
        let pre = DataPreamble {
            object_len: 4000,
            remaining: 0,
        }
        .to_bytes()
        .unwrap();
        let mut payload = pre.to_vec();
        payload.extend_from_slice(&[0u8; 200]);
        FrameHeader {
            ftype: FrameType::Data,
            src,
            dst: Dest::Station(call("SO5KM-1")),
            session: i as u16,
            index: 0,
        }
        .frame(&payload)
        .unwrap()
    };
    for i in 0..200 {
        t += Millis(10);
        b.handle(
            t,
            Input::Frame {
                port: 0,
                data: fake_data(i),
            },
            &mut b_out,
        );
        assert!(b.incoming.len() <= b.cfg.max_incoming);
    }
    // The rest of the real over still completes the transfer.
    for f in &burst[half..] {
        b.handle(
            t,
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut b_out,
        );
    }
    assert!(
        events(&b_out)
            .iter()
            .any(|e| matches!(e, Event::Received { object: o, .. } if *o == object)),
        "the real transfer survived the flood"
    );
}

#[test]
fn one_sender_cannot_hold_more_than_its_share() {
    let mut b = engine("SO5KM-1");
    let mut out = Vec::new();
    for session in 0..20u16 {
        let pre = DataPreamble {
            object_len: 1000,
            remaining: 0,
        }
        .to_bytes()
        .unwrap();
        let mut payload = pre.to_vec();
        payload.extend_from_slice(&[7u8; 200]);
        let f = FrameHeader {
            ftype: FrameType::Data,
            src: call("SP5ZZZ"),
            dst: Dest::Station(call("SO5KM-1")),
            session,
            index: 0,
        }
        .frame(&payload)
        .unwrap();
        b.handle(
            Millis(session as u64),
            Input::Frame { port: 0, data: f },
            &mut out,
        );
    }
    assert_eq!(b.incoming.len(), b.cfg.max_incoming_per_sender);
}

/// A sender waiting for its ACK that hears an over between other stations
/// keeps waiting until its own over could have followed that one and been
/// answered: the link may have held our over back for the busy channel.
#[test]
fn other_traffic_extends_the_wait_for_an_ack() {
    let mut a = engine("SA0KAM");
    let mut out = Vec::new();
    a.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object: vec![7; 1500],
            precedence: 0,
        }),
        &mut out,
    );
    assert!(!frames(&out).is_empty());
    let first = a.next_deadline().unwrap();

    // SP5AAA sends SP5BBB a DATA frame with 10 more to come in its over.
    let pre = DataPreamble {
        object_len: 4000,
        remaining: 10,
    };
    let mut payload = pre.to_bytes().unwrap().to_vec();
    payload.extend_from_slice(&[0u8; 200]);
    let other = FrameHeader {
        ftype: FrameType::Data,
        src: call("SP5AAA"),
        dst: Dest::Station(call("SP5BBB")),
        session: 9,
        index: 3,
    }
    .frame(&payload)
    .unwrap();
    let heard = Millis(5_000);
    a.handle(heard, Input::Frame { port: 0, data: other }, &mut out);
    let later = a.next_deadline().unwrap();
    let cfg = &a.cfg;
    let over_end = heard + cfg.air(10, cfg.data_frame_len(200));
    let o = &a.active[0];
    assert!(later > first);
    assert_eq!(
        later,
        over_end + cfg.ack_guard + o.last_cost + cfg.ack_guard + a.ack_air() + cfg.ack_guard
    );

    // Frames from our peer to us are its answer, not other traffic: even one
    // the engine ignores (another session) leaves the wait as it is.
    let before = a.next_deadline().unwrap();
    let stray = FrameHeader {
        ftype: FrameType::Ack,
        src: call("SO5KM-1"),
        dst: Dest::Station(call("SA0KAM")),
        session: o.session.wrapping_add(1),
        index: 0,
    }
    .frame(&Ack::default().to_vec().unwrap())
    .unwrap();
    a.handle(
        Millis(before.0 - 1),
        Input::Frame { port: 0, data: stray },
        &mut out,
    );
    assert_eq!(a.next_deadline().unwrap(), before);
}

fn session_engine(me: &str, tweak: impl FnOnce(&mut Config)) -> Xfer {
    let mut cfg = Config::vhf_1200(call(me));
    cfg.duty_cycle_permille = 1000;
    tweak(&mut cfg);
    Xfer::new(cfg, identity(me), DetRng::from_seed(1)).unwrap()
}

fn send(x: &mut Xfer, now: Millis, to: &str, object: Vec<u8>) -> Vec<Output<Event>> {
    let mut out = Vec::new();
    x.handle(
        now,
        Input::Command(Command::Send {
            to: call(to),
            object,
            precedence: 0,
        }),
        &mut out,
    );
    out
}

fn ctrl_type(f: &[u8]) -> Option<u8> {
    let (h, p) = FrameHeader::decode(f).unwrap();
    (h.ftype == FrameType::Ctrl).then(|| p[0])
}

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

#[test]
fn symbols_are_no_larger_than_the_object_needs() {
    // One symbol: the object rounded up to the 8-byte alignment.
    assert_eq!(fit_symbol(119, 200), 120);
    assert_eq!(fit_symbol(1, 200), 8);
    // Several: as few as the largest size allows, each as small as that permits.
    assert_eq!(fit_symbol(1000, 200), 200);
    assert_eq!(fit_symbol(1010, 200), 176); // 6 x 176 = 1056 bytes on air, not 6 x 200
    assert_eq!(fit_symbol(1500, 200), 192);
    for len in 1..3000u32 {
        for max in [8u16, 64, 200, 256] {
            let t = fit_symbol(len, max);
            assert!(t.is_multiple_of(8) && (8..=max).contains(&t), "{len} {max}: {t}");
            assert_eq!(
                len.div_ceil(u32::from(t)),
                len.div_ceil(u32::from(max)),
                "{len} {max}: no more symbols than the largest size needs"
            );
        }
    }
}

/// A 119-byte chat bundle goes out as one 120-byte symbol: the 81 bytes of
/// padding a 200-byte symbol would carry are airtime for nothing.
#[test]
fn a_short_message_is_not_padded_to_the_full_symbol_size() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let object = vec![0x33u8; 119];
    let burst = frames(&send(&mut a, Millis(0), "SO5KM-1", object.clone()));
    let data: Vec<&Vec<u8>> = burst
        .iter()
        .filter(|f| FrameHeader::decode(f).unwrap().0.ftype == FrameType::Data)
        .collect();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].len(), HEADER_LEN + DATA_PREAMBLE_LEN + 120);
    let got = events(&deliver(&mut b, Millis(5_000), &burst));
    assert!(got.contains(&Event::Received {
        from: call("SA0KAM"),
        id: object_id(&object),
        object,
    }));
}

/// Run `x`'s deadlines until it transmits; returns what it sent and when.
fn next_over(x: &mut Xfer) -> (Millis, Vec<Vec<u8>>) {
    for _ in 0..16 {
        let t = x.next_deadline().expect("a deadline");
        let mut out = Vec::new();
        x.on_deadline(t, &mut out);
        let fs = frames(&out);
        if !fs.is_empty() {
            return (t, fs);
        }
    }
    panic!("nothing sent");
}

fn ack_from(from: &str, to: &str, session: u16, need: u16) -> Vec<u8> {
    FrameHeader {
        ftype: FrameType::Ack,
        src: call(from),
        dst: Dest::Station(call(to)),
        session,
        index: 0,
    }
    .frame(
        &Ack {
            need,
            ..Ack::default()
        }
        .to_vec()
        .unwrap(),
    )
    .unwrap()
}

/// A missed ACK halves the congestion window and backs off, but it is not
/// counted as frame loss: on a busy channel, larger overs to make up for
/// "loss" would only collide more. Each ACK that comes grows the window again.
#[test]
fn a_missed_ack_shrinks_the_window_not_the_loss_estimate() {
    let mut a = engine("SA0KAM");
    let peer = call("SO5KM-1");
    let first = frames(&send(&mut a, Millis(0), "SO5KM-1", vec![5; 4000]));
    assert_eq!(first.len(), 1 + 16, "OFFER and a full window of the 20 symbols");
    assert_eq!(a.window(peer), 16);
    // Nobody answers: time out, back off, probe.
    let (at, probe) = next_over(&mut a);
    assert_eq!(a.loss_estimate(peer), 0, "silence is not loss");
    assert_eq!(a.window(peer), 8);
    assert_eq!(probe.len(), 1 + 2, "the probe is an OFFER and two symbols");
    // The answer asks for 18 more: one ACK, one more symbol per over.
    let session = FrameHeader::decode(&probe[0]).unwrap().0.session;
    let mut out = Vec::new();
    a.handle(
        at + Millis(5_000),
        Input::Frame {
            port: 0,
            data: ack_from("SO5KM-1", "SA0KAM", session, 18),
        },
        &mut out,
    );
    assert_eq!(a.window(peer), 9);
    assert_eq!(frames(&out).len(), 9, "the next over fills the window");
    assert_eq!(a.loss_estimate(peer), 0, "both probe symbols arrived");
}

/// A link that holds our over back for a busy channel says when it finally
/// went out; the wait for the ACK starts then.
#[test]
fn the_ack_wait_starts_when_the_over_leaves_the_air() {
    let mut a = engine("SA0KAM");
    send(&mut a, Millis(0), "SO5KM-1", vec![7; 1000]);
    let predicted = a.next_deadline().unwrap();
    let ack_wait = a.active[0].ack_wait;
    // The channel was busy for a minute before the over went out.
    let sent = Millis(60_000) + a.active[0].last_cost;
    a.transmitted(sent, 0);
    assert_eq!(a.next_deadline().unwrap(), sent + ack_wait);
    assert!(sent + ack_wait > predicted);
    // Another port's radio says nothing about ours.
    a.transmitted(Millis(90_000), 1);
    assert_eq!(a.next_deadline().unwrap(), sent + ack_wait);
}

#[test]
fn slow_links_get_smaller_symbols_and_shorter_overs() {
    let me = call("SA0KAM");
    let vhf = Config::for_link(me, 1200, Millis(300));
    assert_eq!(vhf, Config::vhf_1200(me));
    let hf = Config::hf_300(me);
    // 64-byte symbols: a DATA frame of 105 bytes on air, 2.9 s at 300 bd.
    assert_eq!(hf.symbol_size, 64);
    assert_eq!(hf.max_over, SLOW_MAX_OVER);
    let frame = hf.air(1, hf.data_frame_len(64));
    assert!(frame >= Millis(2_800) && frame <= Millis(3_000), "{frame:?}");
    assert_eq!(Config::for_link(me, 600, Millis(300)).symbol_size, 184);
    assert_eq!(Config::for_link(me, 9600, Millis(100)).symbol_size, 200);
    for bitrate in [50, 75, 110, 300, 600, 1200, 2400, 9600] {
        let c = Config::for_link(me, bitrate, Millis(300));
        assert!(c.validate().is_ok(), "{bitrate}");
    }
}

/// On a 300 bd HF link an over stays within `max_over`, however much the
/// receiver still needs.
#[test]
fn an_over_never_lasts_longer_than_max_over() {
    let mut cfg = Config::hf_300(call("SA0KAM"));
    cfg.sessions = false;
    cfg.duty_cycle_permille = 1000;
    cfg.max_over = Millis::from_secs(20);
    let mut a = Xfer::new(cfg.clone(), identity("SA0KAM"), DetRng::from_seed(1)).unwrap();
    let burst = frames(&send(&mut a, Millis(0), "SO5KM-1", vec![3; 4000]));
    let on_air = cfg.txdelay
        + burst
            .iter()
            .map(|f| cfg.air(1, f.len()))
            .fold(Millis::ZERO, |sum, t| sum + t);
    assert!(on_air <= cfg.max_over, "{on_air:?}");
    assert!(burst.len() > 5, "still a useful over: {} frames", burst.len());
}
