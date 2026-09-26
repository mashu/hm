#![no_main]
use hm_wire::FrameHeader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((h, payload)) = FrameHeader::decode(data) {
        assert_eq!(h.frame(payload).unwrap(), data);
    }
});
