#![no_main]
use hm_bearer::ax25;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(ui) = ax25::parse_ui(data) {
        assert!(ui.info.len() <= data.len());
    }
    let _ = ax25::unwrap(data);
});
