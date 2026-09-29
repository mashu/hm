#![no_main]
//! SYNC control messages (contact adverts, filters, wants, offers) arrive
//! from any linked station: they decode without panicking, and what decodes
//! encodes to something that decodes to the same message.
use hm_wire::SyncMessage;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = SyncMessage::decode(data) {
        let bytes = message.encode().expect("a decoded message encodes");
        assert_eq!(SyncMessage::decode(&bytes).unwrap(), message);
    }
});
