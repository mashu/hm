#![no_main]
//! The relay routing wrapper (hop count and visited path around a bundle):
//! it unwraps without panicking, wraps back to the same bytes, and taking it
//! one hop further keeps the path free of loops and within the hop limit.
use hm_wire::{unwrap_routed, wrap_routed, Callsign, MAX_ROUTE_HOPS};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(Some(routed)) = unwrap_routed(data) else {
        return;
    };
    assert_eq!(
        wrap_routed(routed.hop_count, &routed.visited, routed.bundle).unwrap(),
        data
    );
    let relay = Callsign::parse("N0CALL").unwrap();
    if let Ok(next) = routed.forwarded_by(relay, MAX_ROUTE_HOPS) {
        let next = unwrap_routed(&next).unwrap().unwrap();
        assert_eq!(next.hop_count, routed.hop_count + 1);
        assert!(next.hop_count <= MAX_ROUTE_HOPS);
        assert_eq!(next.visited.last(), Some(&relay));
        assert_eq!(next.bundle, routed.bundle);
    }
});
