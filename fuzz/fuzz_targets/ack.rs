#![no_main]
use hm_wire::Ack;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(a) = Ack::decode(data) {
        assert_eq!(a.to_vec().unwrap(), data);
    }
});
