#![no_main]
//! The whole transfer engine, fed frames from the air. Frames are separated by
//! 0xC0 bytes; the byte after each separator advances the clock.
use hm_core::{DetRng, Input, Machine, Millis};
use hm_wire::Callsign;
use hm_xfer::{Config, Xfer};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut x = Xfer::new(
        Config::vhf_1200(Callsign::parse("SO5KM-1").unwrap()),
        hm_ident::Identity::from_secret([1; 32]),
        DetRng::from_seed(0),
    )
    .unwrap();
    let mut now = Millis(0);
    let mut out = Vec::new();
    for chunk in data.split(|&b| b == 0xC0) {
        if let Some((&dt, frame)) = chunk.split_first() {
            now += Millis(dt as u64 * 100);
            x.handle(
                now,
                Input::Frame {
                    port: 0,
                    data: frame.to_vec(),
                },
                &mut out,
            );
            if x.next_deadline().is_some_and(|t| t <= now) {
                x.on_deadline(now, &mut out);
            }
        }
        out.clear();
    }
});
