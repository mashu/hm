//! RF admission: who may consume this station's airtime.
//!
//! Identity trust (`[[trust]]`) is the current RF-authorization allowlist.
//! Internet peer authentication remains separate (mutual TLS). Relays do not
//! need a direct trust entry for every destination on a path; they must still
//! authenticate the end-to-end origin before accepting custody, and this gate
//! re-checks before every RF or ARQ-modem transmission.

use crate::files::Trust;
use hm_wire::Callsign;

/// Whether a bundle whose end-to-end origin is `origin` may leave this station
/// over RF (packet radio or an ARQ modem).
///
/// Allowed when:
/// - the bundle was sealed by this station (`origin` is `me` or `key_call`), or
/// - the origin has a trusted key (explicit RF authorization for that station).
///
/// An Internet peer being linked does **not** by itself authorize RF for every
/// bundle that peer forwards.
pub fn may_transmit_rf(
    origin: Callsign,
    me: Callsign,
    key_call: Callsign,
    trust: &Trust,
) -> bool {
    origin == me || origin == key_call || trust.key_for(origin).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::KeyFile;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    #[test]
    fn local_origin_may_use_rf_without_a_trust_entry() {
        let me = call("SA0KAM-1");
        let key_call = call("SA0KAM");
        let trust = Trust::default();
        assert!(may_transmit_rf(me, me, key_call, &trust));
        assert!(may_transmit_rf(key_call, me, key_call, &trust));
        assert!(!may_transmit_rf(call("SO5KM"), me, key_call, &trust));
    }

    #[test]
    fn trusted_origin_may_use_rf() {
        let me = call("SM0R1");
        let peer = KeyFile::generate(call("SA0KAM")).unwrap();
        let mut trust = Trust::default();
        trust.insert(peer.call, peer.identity.public());
        assert!(may_transmit_rf(peer.call, me, me, &trust));
    }
}
