#![no_main]
//! Messages on internet and ARQ modem streams (`hm_wire::stream`), which any
//! linked or connected peer can send: parsing never panics, a message reads
//! back from its encoding, no prefix of it reads as a message, and a stream
//! of messages reads as a whole.
use hm_wire::stream::{next_message, StreamLimits};
use libfuzzer_sys::fuzz_target;

const LIMITS: StreamLimits = StreamLimits {
    max_object: 1 << 20,
    max_control: 4096,
};

fuzz_target!(|data: &[u8]| {
    let mut rest = data;
    while let Ok(Some((message, used))) = next_message(rest, LIMITS) {
        assert!(used > 0 && used <= rest.len());
        let bytes = message.encode();
        // Reasons longer than an answer carries are cut when encoding; every
        // other message encodes to exactly the bytes it was read from.
        if bytes.len() == used {
            assert_eq!(&bytes[..], &rest[..used]);
            for cut in [0, used / 2, used - 1] {
                assert_eq!(next_message(&rest[..cut], LIMITS), Ok(None));
            }
        }
        rest = &rest[used..];
    }
});
