#![no_main]
use hm_ident::SignedBinding;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(b) = SignedBinding::decode(data) {
        let _ = b.valid_attesters();
        assert_eq!(SignedBinding::decode(&b.to_vec()).unwrap(), b);
    }
});
