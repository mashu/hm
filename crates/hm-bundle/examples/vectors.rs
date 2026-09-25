//! Prints the test vectors published in SPEC.md.
//!
//! `cargo run -p hm-bundle --example vectors`
//!
//! Keys come from fixed secrets so the output is reproducible; Ed25519
//! signatures are deterministic.

use hm_bundle::{Address, Bundle, Kind, Precedence};
use hm_ident::{Attestation, BindingRecord, Identity, BINDING, BUNDLE};
use hm_wire::{Ack, Callsign, Dest, FrameHeader, FrameType};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn main() {
    println!("## Callsigns");
    for s in ["SA0KAM", "SO5KM-7", "Q0CLUB"] {
        let c = call(s);
        println!("{s:<8} packed {:>16}  bytes {}", c.packed(), hex(&c.to_bytes()));
    }

    println!(
        "\n## Frame header (DATA, SA0KAM -> SO5KM-1, session 0xBEEF, index 0x012345, payload \"hello\")"
    );
    let h = FrameHeader {
        ftype: FrameType::Data,
        src: call("SA0KAM"),
        dst: Dest::Station(call("SO5KM-1")),
        session: 0xBEEF,
        index: 0x012345,
    };
    println!("{}", hex(&h.frame(b"hello").unwrap()));

    println!("\n## ACK (need 3, snr -4 dB, mode 2, credit 1500 ms, one completed prefix)");
    let a = Ack {
        need: 3,
        snr_db: Some(-4),
        mode_hint: Some(2),
        credit_ms: 1500,
        completed: vec![[1, 2, 3, 4, 5, 6, 7, 8]],
    };
    println!("{}", hex(&a.to_vec().unwrap()));

    let me = Identity::from_secret([11; 32]);
    let club = Identity::from_secret([21; 32]);
    println!("\n## Binding record (secret 0x0b x 32, attested by Q0CLUB with secret 0x15 x 32)");
    println!("public key   {}", hex(&me.public().0));
    println!("attester key {}", hex(&club.public().0));
    let mut rec = BindingRecord::new(call("SA0KAM"), me.public(), 1, 1_790_000_000);
    rec.homes = Some(vec![call("SA0KAM-10"), call("SO5KM-10")]);
    rec.attestations = Some(vec![Attestation::make(
        call("Q0CLUB"),
        &club,
        call("SA0KAM"),
        &me.public(),
    )]);
    let signed = rec.seal(&me).unwrap();
    println!("id   {}", signed.id());
    println!("wire {}", hex(&signed.to_vec()));
    assert_eq!(signed.envelope().id(&BINDING), signed.id());

    let sender = Identity::from_secret([7; 32]);
    println!("\n## Chat bundle (secret 0x07 x 32)");
    println!("public key {}", hex(&sender.public().0));
    let chat = Bundle::new(call("SA0KAM"), Kind::Chat, 1_790_000_000, 3600)
        .to(Address::Station(call("SO5KM")))
        .with_text("73 de SA0KAM")
        .seal(&sender)
        .unwrap();
    println!("id   {}", chat.id());
    println!("wire {}", hex(&chat.to_vec()));
    println!("size {} bytes", chat.to_vec().len());

    println!("\n## Mail bundle (same key, priority, two recipients, subject)");
    let mail = Bundle::new(call("SA0KAM"), Kind::Mail, 1_790_000_100, 7 * 86_400)
        .to(Address::Station(call("SO5KM")))
        .to(Address::Email("qsl@example.org".into()))
        .with_precedence(Precedence::Priority)
        .with_subject("Sked")
        .with_text("40m 7.047 MHz at 19:00 UTC?")
        .seal(&sender)
        .unwrap();
    println!("id   {}", mail.id());
    println!("wire {}", hex(&mail.to_vec()));
    assert_eq!(mail.envelope().id(&BUNDLE), mail.id());
}
