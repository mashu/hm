#![no_main]
use hm_bundle::Opened;
use hm_ident::Identity;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(opened) = Opened::decode(data) {
        let _ = opened.bundle.validate();
        if let Some(body) = &opened.bundle.body {
            let _ = body.as_text();
        }
        let _ = opened.bundle.is_expired(u64::MAX);
        // Without the private key, nothing the fuzzer builds may verify.
        let key = Identity::from_secret([7; 32]).public();
        let _ = opened.verify(&key);
    }
});
