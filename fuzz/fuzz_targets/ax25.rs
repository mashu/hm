#![no_main]
//! AX.25 UI frames from the air: parsing never panics, and a compact hm
//! frame unwraps to a full frame whose header decodes, from the AX.25 source.
use hm_bearer::ax25;
use hm_wire::FrameHeader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(ui) = ax25::parse_ui(data) {
        assert!(ui.info.len() <= data.len());
        if let Some(frame) = ax25::unwrap_frame(data) {
            if ui.info.first().is_some_and(|b| b >> 4 == ax25::COMPACT_VERSION) {
                let (h, _) = FrameHeader::decode(&frame).expect("rebuilt headers decode");
                assert_eq!(Some(h.src), ui.src.to_callsign());
            }
        }
    }
    let _ = ax25::unwrap(data);
});
