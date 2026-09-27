use super::*;
use alloc::vec;
use hm_ident::Envelope;
use proptest::prelude::*;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn me() -> Identity {
    Identity::from_secret([7; 32])
}

fn chat() -> Bundle {
    Bundle::new(call("SA0KAM"), Kind::Chat, 1_790_000_000, 3600)
        .to(Address::Station(call("SO5KM")))
        .with_text("73 de SA0KAM")
}

#[test]
fn seal_open_verify_roundtrip() {
    let signed = chat().seal(&me()).unwrap();
    let wire = signed.to_vec();
    let opened = Opened::decode(&wire).unwrap();
    assert_eq!(opened.id, signed.id());
    assert_eq!(opened.bundle.to, vec![Address::Station(call("SO5KM"))]);
    let verified = opened.verify(&me().public()).unwrap();
    assert_eq!(
        verified.bundle().body.as_ref().unwrap().as_text().unwrap(),
        "73 de SA0KAM"
    );
}

#[test]
fn wrong_sender_key_fails() {
    let wire = chat().seal(&me()).unwrap().to_vec();
    let stranger = Identity::from_secret([8; 32]);
    let err = Opened::decode(&wire)
        .unwrap()
        .verify(&stranger.public())
        .unwrap_err();
    assert_eq!(err, BundleError::Ident(IdentError::BadSignature));
}

/// Guards the wire format: if this changes, the spec and every peer change too.
#[test]
fn golden_vector() {
    let signed = chat().seal(&me()).unwrap();
    let hex: alloc::string::String = signed
        .to_vec()
        .iter()
        .map(|b| alloc::format!("{b:02x}"))
        .collect();
    assert_eq!(signed.id().to_string(), GOLDEN_ID, "wire bytes: {hex}");
    assert_eq!(hex, GOLDEN_WIRE);
}

const GOLDEN_ID: &str = "d938aeaf37a378615c56aca3b385fcbc6e3d0eb724f6cce8901f260b3e2626f1";
/// Cross-checked with independent BLAKE3 (python `blake3`) and Ed25519 (PyNaCl).
const GOLDEN_WIRE: &str = concat!(
    "825832a70000014600004f8af6fb028182004600000207586b0301051a6ab13b80",
    "06190e100882004c3733206465205341304b414d5840ce3c7adc856375a2ce7cbb",
    "47011edcbbfa93ff43bf4346232367268be9e35cf86d01b31ab2408b6ba8954f50",
    "88b430c211eca59261c6d9805cd446f9773aa602",
);

#[test]
fn short_chat_overhead_is_small() {
    let wire = chat().seal(&me()).unwrap().to_vec();
    let text = "73 de SA0KAM".len();
    // Everything that is not the text itself: header fields, envelope, signature.
    let overhead = wire.len() - text;
    assert!(
        overhead <= 110,
        "overhead {overhead} bytes (total {})",
        wire.len()
    );
}

#[test]
fn newer_fields_survive_relay_and_verify() {
    // A future version adds field 42. An old node still decodes, routes and
    // verifies the bundle because the id and signature cover the raw bytes.
    #[derive(Encode)]
    #[cbor(map)]
    struct Future<'a> {
        #[n(0)]
        v: u8,
        #[n(1)]
        from: Callsign,
        #[n(2)]
        to: &'a [Address],
        #[n(3)]
        kind: Kind,
        #[n(5)]
        created: u64,
        #[n(6)]
        ttl: u32,
        #[n(42)]
        extra: &'a str,
    }
    let to = [
        Address::Station(call("SO5KM")),
        Address::Other {
            tag: 9,
            raw: vec![0x18, 0x2A],
        },
    ];
    let future = Future {
        v: 0,
        from: call("SA0KAM"),
        to: &to,
        kind: Kind::Other(77),
        created: 1,
        ttl: 60,
        extra: "new",
    };
    let raw = minicbor::to_vec(&future).unwrap();
    let env = Envelope::seal(&hm_ident::BUNDLE, raw, &me());
    let opened = Opened::decode(&env.to_vec()).unwrap();
    assert_eq!(opened.bundle.kind, Kind::Other(77));
    assert_eq!(
        opened.bundle.to[1],
        Address::Other {
            tag: 9,
            raw: vec![0x18, 0x2A]
        }
    );
    // Unknown address values re-encode verbatim.
    let reencoded = minicbor::to_vec(&opened.bundle.to[1]).unwrap();
    assert_eq!(reencoded, vec![0x82, 0x09, 0x18, 0x2A]);
    assert!(opened.verify(&me().public()).is_ok());
}

#[test]
fn validation_rules() {
    let base = || Bundle::new(call("SA0KAM"), Kind::Mail, 0, 60);
    assert!(base().seal(&me()).is_err(), "no recipients");
    assert!(base().to(Address::Group(String::new())).seal(&me()).is_err());
    assert!(base()
        .to(Address::Email("no-at-sign".into()))
        .seal(&me())
        .is_err());
    assert!(base().to(Address::Email("@x".into())).seal(&me()).is_err());
    assert!(base().to(Address::Email("a@b.se".into())).seal(&me()).is_ok());
    let mut b = base().to(Address::Station(call("SO5KM")));
    b.ttl = 0;
    assert!(b.seal(&me()).is_err());
    let mut b = base().to(Address::Station(call("SO5KM")));
    b.prec = Some(Precedence::Routine);
    assert!(b.seal(&me()).is_err(), "routine must be omitted");
    let receipt_without_ref =
        Bundle::new(call("SO5KM"), Kind::Receipt, 0, 60).to(Address::Station(call("SA0KAM")));
    assert!(receipt_without_ref.seal(&me()).is_err());
    let nonminimal_receipt =
        Bundle::receipt(call("SO5KM"), call("SA0KAM"), ObjectId([1; 32]), 0, 60).with_text("extra");
    assert!(nonminimal_receipt.seal(&me()).is_err());
    let multicast_receipt = Bundle::receipt(call("SO5KM"), call("SA0KAM"), ObjectId([1; 32]), 0, 60)
        .to(Address::Station(call("SP5AAA")));
    assert!(multicast_receipt.seal(&me()).is_err());
}

#[test]
fn receipt_and_precedence_helpers() {
    let original = chat().seal(&me()).unwrap();
    let r = Bundle::receipt(
        call("SO5KM"),
        call("SA0KAM"),
        original.id(),
        1_790_000_060,
        86_400,
    );
    assert_eq!(r.reply_to, Some(original.id()));
    assert!(r.clone().seal(&Identity::from_secret([9; 32])).is_ok());
    let flash = chat().with_precedence(Precedence::Flash);
    assert!(flash.precedence().rank() > chat().precedence().rank());
    assert_eq!(chat().with_precedence(Precedence::Routine).prec, None);
    assert_eq!(Precedence::Other(200).rank(), 0);
}

#[test]
fn expiry() {
    let b = chat();
    assert!(!b.is_expired(1_790_000_000 + 3599));
    assert!(b.is_expired(1_790_000_000 + 3600));
}

#[test]
fn max_hops_is_bounded_and_default_is_canonical() {
    assert_eq!(chat().max_hops(), DEFAULT_MAX_HOPS);
    assert_eq!(chat().with_max_hops(DEFAULT_MAX_HOPS).max_hops, None);
    let signed = chat().with_max_hops(3).seal(&me()).unwrap();
    let opened = Opened::decode(&signed.to_vec()).unwrap();
    assert_eq!(opened.bundle.max_hops(), 3);
    assert!(chat().with_max_hops(0).seal(&me()).is_err());
    assert!(chat().with_max_hops(MAX_HOPS + 1).seal(&me()).is_err());
    let mut noncanonical = chat();
    noncanonical.max_hops = Some(DEFAULT_MAX_HOPS);
    assert!(noncanonical.seal(&me()).is_err());
}

#[test]
fn codec_one_uses_the_pinned_dictionary_and_vector() {
    assert_eq!(
        blake3::hash(ZSTD_DICTIONARY).to_hex().as_str(),
        ZSTD_DICTIONARY_BLAKE3
    );
    let text = "Net control calling all stations. Check in with callsign, location, and traffic. \
        Net control calling all stations. Please acknowledge receipt.";
    let body = Body::text(text);
    assert_eq!(body.codec, Codec::Zstd);
    assert_eq!(
        body.data,
        [
            0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x8e, 0x75, 0x00, 0x00, 0x08, 0x20, 0x03, 0x00, 0x08, 0x25, 0x04,
            0x2a, 0x9c, 0xeb, 0x60, 0x74, 0x05, 0x01,
        ]
    );
    assert_eq!(body.as_text().unwrap(), text);
    assert_eq!(Body::text("73").codec, Codec::Plain);
}

#[test]
fn codec_one_rejects_invalid_and_oversized_output() {
    let malformed = Body {
        codec: Codec::Zstd,
        data: vec![1, 2, 3],
    };
    assert!(malformed.as_text().is_err());
    let too_large = "A".repeat(MAX_DECOMPRESSED_BODY + 1);
    let compressed = Body::text(&too_large);
    assert_eq!(compressed.codec, Codec::Zstd);
    assert!(compressed.as_text().is_err());
}

#[test]
fn attachments_are_references() {
    let part = PartRef {
        hash: ObjectId([5; 32]),
        size: 48_213,
        mime: "image/jpeg".into(),
        name: Some("map.jpg".into()),
        thumb: Some(vec![0xFF, 0xD8]),
    };
    let signed = chat().with_part(part.clone()).seal(&me()).unwrap();
    let back = Opened::decode(&signed.to_vec()).unwrap();
    assert_eq!(back.bundle.parts, Some(vec![part]));
}

proptest! {
    #[test]
    fn open_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        let _ = Opened::decode(&bytes);
    }

    #[test]
    fn arbitrary_text_roundtrips(text in "\\PC{0,200}", ttl in 1u32..1_000_000) {
        let b = Bundle::new(call("SA0KAM"), Kind::Mail, 5, ttl).to(Address::Station(call("SO5KM"))).with_text(&text);
        let signed = b.seal(&me()).unwrap();
        let opened = Opened::decode(&signed.to_vec()).unwrap();
        prop_assert_eq!(opened.bundle.body.as_ref().unwrap().as_text().unwrap(), text.as_str());
        prop_assert!(opened.verify(&me().public()).is_ok());
    }
}
