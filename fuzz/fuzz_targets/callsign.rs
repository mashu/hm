#![no_main]
use hm_wire::Callsign;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = core::str::from_utf8(data) {
        if let Ok(c) = Callsign::parse(s) {
            assert_eq!(c.to_string(), s.to_ascii_uppercase());
        }
    }
    if data.len() >= 6 {
        if let Ok(c) = Callsign::from_bytes(data[..6].try_into().unwrap()) {
            assert_eq!(Callsign::parse(&c.to_string()).unwrap(), c);
        }
    }
});
