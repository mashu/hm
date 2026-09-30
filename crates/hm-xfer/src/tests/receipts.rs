//! Signed receipts: verified against a trusted key, forgeries ignored, sent
//! again for a resent offer.

use super::*;

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
