//! One sender, or a flood of garbage sessions, cannot take a receiver's
//! capacity from the others.

use super::*;

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
