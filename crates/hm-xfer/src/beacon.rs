//! BEACON frames: signed presence and identification.
//!
//! A station broadcasts who it is, which key it signs with (by its id), what
//! it offers (mailbox, relay, internet) and which stations it has heard
//! lately. The signature binds
//! the frame's source callsign to the key: a beacon that verifies with the
//! key trusted for its callsign comes from that station. Beacons never teach
//! keys: one from a station with no trusted key cannot be checked, and one
//! whose key id is not the trusted key's is from another key.

use alloc::vec::Vec;

use hm_ident::{Identity, PublicKey};
use hm_wire::{Beacon, Callsign, Dest, FrameHeader, FrameType, Heard, Locator, WireError};

/// A beacon frame from `me`, signed now.
pub fn beacon_frame(
    identity: &Identity,
    me: Callsign,
    flags: u8,
    time: u32,
    locator: Option<Locator>,
    heard: Vec<Heard>,
) -> Result<Vec<u8>, WireError> {
    let mut b = Beacon {
        flags,
        key_id: identity.public().id(),
        time,
        locator,
        heard,
        signature: [0; 64],
    };
    b.signature = identity.sign(&b.signed_bytes(me)?);
    FrameHeader {
        ftype: FrameType::Beacon,
        src: me,
        dst: Dest::Broadcast,
        session: 0,
        index: 0,
    }
    .frame(&b.to_vec()?)
}

/// A beacon heard on the air, its signature not yet checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeardBeacon {
    pub from: Callsign,
    pub beacon: Beacon,
}

impl HeardBeacon {
    /// Whether it was signed with `key`: the key id names it, and the
    /// signature verifies with it.
    pub fn signed_by(&self, key: &PublicKey) -> bool {
        key.id() == self.beacon.key_id
            && self
                .beacon
                .signed_bytes(self.from)
                .is_ok_and(|statement| key.verify(&statement, &self.beacon.signature).is_ok())
    }
}

/// The beacon in `frame`, if it is one: laid out right, broadcast. Its
/// signature is for the caller to check, with the key it trusts for the
/// sender ([`HeardBeacon::signed_by`]).
pub fn read_beacon(frame: &[u8]) -> Option<HeardBeacon> {
    let (h, payload) = FrameHeader::decode(frame).ok()?;
    if h.ftype != FrameType::Beacon || h.dst != Dest::Broadcast {
        return None;
    }
    let beacon = Beacon::decode(payload).ok()?;
    Some(HeardBeacon { from: h.src, beacon })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_wire::FLAG_MAILBOX;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    #[test]
    fn signed_beacons_verify_with_their_key_and_altered_ones_do_not() {
        let id = Identity::from_secret([3; 32]);
        let heard = alloc::vec![Heard {
            call: call("SO5KM-1"),
            minutes: 4,
        }];
        let f = beacon_frame(
            &id,
            call("SA0KAM-10"),
            FLAG_MAILBOX,
            1_790_000_000,
            Locator::parse("JO89").ok(),
            heard.clone(),
        )
        .unwrap();
        let got = read_beacon(&f).expect("a beacon");
        assert!(got.signed_by(&id.public()));
        assert!(!got.signed_by(&Identity::from_secret([4; 32]).public()));
        assert_eq!(got.from, call("SA0KAM-10"));
        assert_eq!(got.beacon.key_id, id.public().id());
        assert_eq!((got.beacon.flags, got.beacon.time), (FLAG_MAILBOX, 1_790_000_000));
        assert_eq!(got.beacon.heard, heard);
        assert_eq!(got.beacon.locator.unwrap().to_string(), "JO89");

        // Every single-bit change is caught: header source, body or signature.
        for byte in 1..f.len() {
            if (13..18).contains(&byte) {
                continue; // session and index are not covered, and not used
            }
            let mut g = f.clone();
            g[byte] ^= 0x01;
            assert!(
                read_beacon(&g).is_none_or(|b| !b.signed_by(&id.public())),
                "flip in byte {byte} accepted"
            );
        }
        // Not a beacon, or not broadcast: ignored.
        let mut to_one = f.clone();
        to_one[7..13].copy_from_slice(&call("SO5KM").to_bytes());
        assert!(read_beacon(&to_one).is_none());
        assert!(read_beacon(&f[..40]).is_none());
    }

    #[test]
    fn mutated_frames_never_panic_and_verified_ones_re_encode() {
        use hm_core::DetRng;
        let id = Identity::from_secret([5; 32]);
        let heard = (0..4)
            .map(|i| Heard {
                call: call(&alloc::format!("SQ{i}XX")),
                minutes: i as u8,
            })
            .collect();
        let good = beacon_frame(&id, call("SA0KAM"), 0x07, 1_790_000_000, None, heard).unwrap();
        let mut g = DetRng::from_seed(9);
        let mut accepted = 0;
        for _ in 0..2_000 {
            let mut f = good.clone();
            match g.below(3) {
                0 => {
                    for _ in 0..1 + g.below(4) {
                        let i = g.below(f.len() as u64) as usize;
                        f[i] = g.next_u64() as u8;
                    }
                }
                1 => f.truncate(g.below(f.len() as u64) as usize),
                _ => f.extend((0..1 + g.below(20)).map(|_| g.next_u64() as u8)),
            }
            if let Some(h) = read_beacon(&f).filter(|h| h.signed_by(&id.public())) {
                accepted += 1;
                assert_eq!(h.beacon.to_vec().unwrap(), f[18..]);
            }
        }
        // Only changes to the unsigned session and index can still verify.
        assert!(accepted < 2_000 / 10, "{accepted}");
    }
}
