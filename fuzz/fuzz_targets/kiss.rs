#![no_main]
use hm_bearer::kiss;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Any byte stream, split at an arbitrary point: never panics, frames stay bounded,
    // and every decoded frame re-encodes to something that decodes to itself.
    let split = data.first().copied().unwrap_or(0) as usize % (data.len() + 1);
    let mut d = kiss::Decoder::new(512);
    let mut out = Vec::new();
    d.push(&data[..split], &mut out);
    d.push(&data[split..], &mut out);
    for f in out {
        assert!(f.data.len() < 512);
        let mut wire = Vec::new();
        kiss::encode(f.port, f.command, &f.data, &mut wire);
        let mut again = Vec::new();
        kiss::Decoder::new(512).push(&wire, &mut again);
        assert_eq!(again, vec![f]);
    }
});
