#![no_main]
//! CTRL payloads: OFFER, OPEN and CLOSE decode without panicking, and what
//! decodes encodes back to the same bytes.
use hm_wire::{Close, Offer, Open};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(o) = Offer::decode(data) {
        assert_eq!(&o.to_bytes().unwrap()[..], data);
    }
    if let Ok(o) = Open::decode(data) {
        assert_eq!(&o.to_bytes().unwrap()[..], data);
    }
    if let Ok(c) = Close::decode(data) {
        assert_eq!(&c.to_bytes()[..], data);
    }
});
