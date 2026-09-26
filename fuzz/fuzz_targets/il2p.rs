#![no_main]
//! The IL2P receiver fed arbitrary bits never panics, and anything it
//! decodes encodes to a frame it decodes again to the same bytes.
use hm_modem_afsk::il2p::{encode_bits, Receiver};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut r = Receiver::new();
    for &byte in data {
        for k in (0..8).rev() {
            if let Some(frame) = r.push((byte >> k) & 1) {
                let mut again = Receiver::new();
                let back: Vec<Vec<u8>> = encode_bits(&[&frame], 1, true)
                    .into_iter()
                    .filter_map(|b| again.push(b))
                    .collect();
                assert_eq!(back, vec![frame]);
            }
        }
    }
});
