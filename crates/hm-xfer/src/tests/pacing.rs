//! When overs go and how long they are: precedence, duty cycle, burst size
//! from the loss belief, the ACK wait, giving up on silence.

use hm_model::Openness;

use super::*;

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

/// Bursts come from the station's loss belief: a lossy link gets more
/// frames than need, a clean one about as many as needed.
#[test]
fn bursts_follow_the_loss_belief() {
    let over = |loss: f64| {
        let mut a = engine("SA0KAM");
        let peer = call("SO5KM-1");
        a.handle(
            Millis(0),
            Input::Command(Command::Belief {
                peer,
                belief: PeerBelief::open(Erasure::from_prior(Prior::new(loss, 200.0), 0.0)),
            }),
            &mut Vec::new(),
        );
        // Five 200-byte symbols to go.
        frames(&send(&mut a, Millis(0), "SO5KM-1", vec![3; 1000])).len() - 1
    };
    let (clean, lossy) = (over(0.01), over(0.3));
    assert!((5..=6).contains(&clean), "{clean}");
    assert!(lossy > clean + 1, "{lossy} vs {clean}");
}

/// Overs that bring no answer are evidence the link is closed. Told the
/// link is doubtful and airtime costs something, a sender stops after a few
/// silent overs; told nothing, it goes on for `max_rounds`. Traffic that
/// loses little by waiting to hear the peer again stops sooner still.
#[test]
fn silence_ends_a_transfer_once_another_over_is_not_worth_its_airtime() {
    let overs_into_silence = |belief: Option<PeerBelief>| {
        let mut a = engine("SA0KAM");
        let peer = call("SO5KM-1");
        let mut out = Vec::new();
        if let Some(belief) = belief {
            a.handle(
                Millis(0),
                Input::Command(Command::Belief { peer, belief }),
                &mut out,
            );
        }
        a.handle(
            Millis(0),
            Input::Command(Command::Send {
                to: peer,
                object: vec![7; 300],
                precedence: 0,
            }),
            &mut out,
        );
        let mut overs = 0;
        loop {
            overs += frames(&out)
                .iter()
                .filter(|f| ctrl_type(f) == Some(CTRL_OFFER))
                .count();
            if events(&out).contains(&Event::Failed {
                to: peer,
                id: object_id(&[7; 300]),
                reason: Failure::NoAnswer,
            }) {
                return overs;
            }
            out.clear();
            let t = a.next_deadline().expect("a transfer under way");
            a.on_deadline(t, &mut out);
        }
    };
    let belief = |open: f64, wait_cost: f64| PeerBelief {
        erasure: Erasure::from_prior(Prior::new(0.1, 20.0), 0.1),
        open: Openness {
            p: open,
            daily: 0.5,
            persistence_secs: 3_600.0,
        },
        airtime_cost: 0.01,
        wait_cost,
    };
    let told_nothing = overs_into_silence(None);
    assert_eq!(told_nothing, usize::from(engine("SA0KAM").config().max_rounds));
    let doubtful = overs_into_silence(Some(belief(0.3, 1.0)));
    assert!((1..=3).contains(&doubtful), "{doubtful} overs");
    // A link just heard is given more overs before silence counts against it.
    let heard = overs_into_silence(Some(belief(0.95, 1.0)));
    assert!(heard > doubtful && heard < told_nothing, "{heard} vs {doubtful}");
    // Mail loses little by waiting to hear the peer again: it stops sooner.
    let mail = overs_into_silence(Some(belief(0.95, 0.001)));
    assert!(mail < heard, "{mail} vs {heard}");
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
    let before = a.loss_estimate(peer);
    // Nobody answers: time out, back off, probe.
    let (at, probe) = next_over(&mut a);
    assert_eq!(a.loss_estimate(peer), before, "silence is not loss");
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
    assert_eq!(overs(&out), vec![(peer, 2, 2)], "both probe symbols arrived");
    assert!(a.loss_estimate(peer) < before);
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
