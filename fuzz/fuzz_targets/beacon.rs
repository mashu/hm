#![no_main]
use hm_wire::Beacon;
use hm_xfer::beacon::read_beacon;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The payload decoder re-encodes what it accepts byte for byte.
    if let Ok(b) = Beacon::decode(data) {
        assert_eq!(b.to_vec().unwrap(), data);
    }
    // Whole frames: never panics, and whatever verifies re-encodes to the same payload.
    if let Some(h) = read_beacon(data) {
        assert_eq!(h.beacon.to_vec().unwrap(), data[18..]);
    }
});
