#![no_main]
use hm_ident::Envelope;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(env) = Envelope::decode(data) {
        // Re-encoding may normalise the CBOR container, never the signed content.
        assert_eq!(Envelope::decode(&env.to_vec()).unwrap(), env);
    }
});
