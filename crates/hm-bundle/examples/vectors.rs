//! Prints the test vectors published in SPEC.md.
//!
//! `cargo run -p hm-bundle --example vectors`
//!
//! Keys come from fixed secrets so the output is reproducible; Ed25519
//! signatures are deterministic.

use hm_bearer::{ax25, kiss};
use hm_bundle::{Address, Bundle, Kind, Precedence};
use hm_core::{DetRng, Input, Machine, Millis, Output};
use hm_ident::{Attestation, BindingRecord, Identity, BINDING, BUNDLE};
use hm_wire::{Ack, Callsign, Dest, FrameHeader, FrameType};
use hm_xfer::{object_id, Command, Config, Xfer};

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
        receipt: None,
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

    println!("\n## AX.25 UI and KISS (the frame header vector above, from SA0KAM)");
    let hm_frame = h.frame(b"hello").unwrap();
    let ui = ax25::wrap(call("SA0KAM"), &hm_frame).unwrap();
    println!("ax25 {}", hex(&ui));
    println!("kiss {}", hex(&kiss::data_frame(0, &ui)));

    println!("\n## Transfer of the chat bundle (SA0KAM -> SO5KM-1, symbol size 200, first over)");
    let object = chat.to_vec();
    println!("object_id {}", object_id(&object));
    let mut cfg = Config::vhf_1200(call("SA0KAM"));
    cfg.duty_cycle_permille = 1000;
    let mut x = Xfer::new(cfg, Identity::from_secret([7; 32]), DetRng::from_seed(0)).unwrap();
    let mut out = Vec::new();
    x.handle(
        Millis(0),
        Input::Command(Command::Send {
            to: call("SO5KM-1"),
            object,
            precedence: 0,
        }),
        &mut out,
    );
    let mut over = Vec::new();
    for o in out {
        if let Output::Transmit { data, .. } = o {
            let kind = match FrameHeader::decode(&data).unwrap().0.ftype {
                FrameType::Ctrl => "offer",
                FrameType::Data => "data ",
                _ => "other",
            };
            println!("{kind} {}", hex(&data));
            over.push(data);
        }
    }

    println!("\n## Receipt ACK from SO5KM-1 (secret 0x0c x 32) for that transfer");
    let receiver = Identity::from_secret([12; 32]);
    println!("public key {}", hex(&receiver.public().0));
    let mut rcfg = Config::vhf_1200(call("SO5KM-1"));
    rcfg.duty_cycle_permille = 1000;
    let mut r = Xfer::new(rcfg, receiver, DetRng::from_seed(0)).unwrap();
    let mut rout = Vec::new();
    for f in over {
        r.handle(Millis(0), Input::Frame { port: 0, data: f }, &mut rout);
    }
    let t = r.next_deadline().unwrap();
    r.on_deadline(t, &mut rout);
    for o in rout {
        if let Output::Transmit { data, .. } = o {
            println!("ack   {}", hex(&data));
        }
    }
}
