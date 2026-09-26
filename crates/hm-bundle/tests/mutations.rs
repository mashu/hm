//! Mutation testing of every decoder, on stable Rust.
//!
//! Valid wire objects are mutated (bit flips, truncation, insertion, deletion,
//! duplication, boundary bytes, splicing) and fed to every decoder. Properties:
//!
//! 1. No decoder panics.
//! 2. No forgery: a mutant that still verifies carries exactly the original
//!    signed bytes and signature (only the CBOR container may differ).
//! 3. Fixed-layout decoders round-trip whatever they accept.
//!
//! `HM_MUTATIONS=1000000 cargo test -p hm-bundle --release --test mutations` for the nightly run.

use hm_bundle::{Address, Bundle, Kind, Opened, Precedence};
use hm_core::DetRng;
use hm_ident::{Attestation, BindingRecord, Envelope, Identity, SignedBinding};
use hm_wire::{Ack, Callsign, Dest, FrameHeader, FrameType};

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

struct Samples {
    sender: Identity,
    wire: Vec<Vec<u8>>,
    bundles: Vec<Envelope>,
    binding: Envelope,
}

fn samples() -> Samples {
    let sender = Identity::from_secret([7; 32]);
    let chat = Bundle::new(call("SA0KAM"), Kind::Chat, 1_790_000_000, 3600)
        .to(Address::Station(call("SO5KM")))
        .with_text("73 de SA0KAM")
        .seal(&sender)
        .unwrap();
    let mail = Bundle::new(call("SA0KAM"), Kind::Mail, 1_790_000_100, 7 * 86_400)
        .to(Address::Station(call("SO5KM")))
        .to(Address::Email("qsl@example.org".into()))
        .to(Address::Group("SK-EMCOMM".into()))
        .with_precedence(Precedence::Flash)
        .with_subject("Sked")
        .with_text("40m 7.047 MHz at 19:00 UTC?")
        .seal(&sender)
        .unwrap();
    let me = Identity::from_secret([11; 32]);
    let club = Identity::from_secret([21; 32]);
    let mut rec = BindingRecord::new(call("SA0KAM"), me.public(), 1, 1_790_000_000);
    rec.homes = Some(vec![call("SA0KAM-10"), call("SO5KM-10")]);
    rec.attestations = Some(vec![Attestation::make(
        call("Q0CLUB"),
        &club,
        call("SA0KAM"),
        &me.public(),
    )]);
    let binding = rec.seal(&me).unwrap();
    let header = FrameHeader {
        ftype: FrameType::Data,
        src: call("SA0KAM"),
        dst: Dest::Station(call("SO5KM-1")),
        session: 0xBEEF,
        index: 0x012345,
    };
    let ack = Ack {
        need: 3,
        snr_db: Some(-4),
        mode_hint: Some(2),
        credit_ms: 1500,
        completed: vec![[1; 8], [2; 8]],
        receipt: None,
    };
    Samples {
        wire: vec![
            header.frame(&chat.to_vec()).unwrap(),
            ack.to_vec().unwrap(),
            chat.to_vec(),
            mail.to_vec(),
            binding.to_vec(),
        ],
        bundles: vec![chat.envelope().clone(), mail.envelope().clone()],
        binding: binding.envelope().clone(),
        sender,
    }
}

fn mutate(g: &mut DetRng, input: &[u8], other: &[u8]) -> Vec<u8> {
    let mut m = input.to_vec();
    let rounds = 1 + g.below(3);
    for _ in 0..rounds {
        let len = m.len() as u64;
        match g.below(7) {
            0 if len > 0 => {
                for _ in 0..1 + g.below(8) {
                    let bit = g.below(len * 8);
                    m[(bit / 8) as usize] ^= 1 << (bit % 8);
                }
            }
            1 => m.truncate(g.below(len + 1) as usize),
            2 => {
                let at = g.below(len + 1) as usize;
                for _ in 0..1 + g.below(4) {
                    m.insert(at, g.next_u64() as u8);
                }
            }
            3 if len > 0 => {
                let at = g.below(len) as usize;
                let n = (1 + g.below(4) as usize).min(m.len() - at);
                m.drain(at..at + n);
            }
            4 if len > 0 => {
                let a = g.below(len) as usize;
                let b = (a + 1 + g.below(16) as usize).min(m.len());
                let chunk = m[a..b].to_vec();
                let at = g.below(m.len() as u64 + 1) as usize;
                m.splice(at..at, chunk);
            }
            5 if len > 0 => {
                let at = g.below(len) as usize;
                m[at] = [0x00, 0xFF, 0x7F, 0x80, 0x1B, 0x5B, 0x9F, 0xBF][g.below(8) as usize];
            }
            6 if !other.is_empty() => {
                let cut = g.below(len + 1) as usize;
                let from = g.below(other.len() as u64) as usize;
                m.truncate(cut);
                m.extend_from_slice(&other[from..]);
            }
            _ => {}
        }
    }
    m
}

fn iterations() -> u64 {
    std::env::var("HM_MUTATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000)
}

#[test]
fn mutated_inputs_never_panic_or_forge() {
    let s = samples();
    let mut g = DetRng::from_seed(0xF022);
    let (mut decoded_bundles, mut verified_bundles, mut decoded_bindings, mut frames) =
        (0u64, 0u64, 0u64, 0u64);
    for i in 0..iterations() {
        let pick = g.below(s.wire.len() as u64) as usize;
        let other = &s.wire[g.below(s.wire.len() as u64) as usize];
        let m = mutate(&mut g, &s.wire[pick], other);

        if let Ok((h, payload)) = FrameHeader::decode(&m) {
            frames += 1;
            assert_eq!(h.frame(payload).unwrap(), m, "iteration {i}: frame round-trip");
        }
        if let Ok(a) = Ack::decode(&m) {
            assert_eq!(a.to_vec().unwrap(), m, "iteration {i}: ack round-trip");
        }
        for w in m.windows(6).take(4) {
            if let Ok(c) = Callsign::from_bytes(w.try_into().unwrap()) {
                assert_eq!(Callsign::parse(&c.to_string()).unwrap(), c);
            }
        }
        if let Ok(opened) = Opened::decode(&m) {
            decoded_bundles += 1;
            let env = opened.envelope.clone();
            if opened.verify(&s.sender.public()).is_ok() {
                verified_bundles += 1;
                assert!(
                    s.bundles.contains(&env),
                    "iteration {i}: FORGERY, mutated bundle verified: {m:02x?}"
                );
            }
        }
        if let Ok(b) = SignedBinding::decode(&m) {
            decoded_bindings += 1;
            assert_eq!(
                b.envelope(),
                &s.binding,
                "iteration {i}: FORGERY, mutated binding verified"
            );
        }
        // Frames carry bundles: the payload of a mutated frame must also be safe.
        if m.len() > hm_wire::HEADER_LEN {
            if let Ok(opened) = Opened::decode(&m[hm_wire::HEADER_LEN..]) {
                if opened
                    .envelope
                    .verify(&hm_ident::BUNDLE, &s.sender.public())
                    .is_ok()
                {
                    assert!(
                        s.bundles.contains(&opened.envelope),
                        "iteration {i}: FORGERY inside frame"
                    );
                }
            }
        }
    }
    eprintln!(
        "{} mutants: {frames} parsed as frames, {decoded_bundles} decoded as bundles \
         ({verified_bundles} verified, all identical to the originals), {decoded_bindings} bindings verified",
        iterations()
    );
    // The mutator must actually reach the interesting paths.
    assert!(decoded_bundles > 0 && frames > 0);
}
