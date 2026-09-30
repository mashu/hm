use alloc::vec::Vec;

use crate::{Callsign, Locator, WireError};

/// Signature domain of a beacon.
pub const BEACON_SIG_PREFIX: &[u8] = b"hm/beacon-sig/v0";
/// Most stations one beacon lists as heard.
pub const MAX_HEARD: usize = 16;
/// Bytes of a key id: enough to tell keys apart, not to learn one.
pub const KEY_ID_LEN: usize = 8;
/// Bytes of a beacon payload before the heard list.
const FIXED: usize = 20;
const SIG_LEN: usize = 64;

/// Flag: the station holds mail for other stations (a mailbox node).
pub const FLAG_MAILBOX: u8 = 0x01;
/// Flag: the station passes mail on for others (Phase 2).
pub const FLAG_RELAY: u8 = 0x02;
/// Flag: the station has internet links.
pub const FLAG_INTERNET: u8 = 0x04;
/// Flag: the station holds bundles others may pull with holdings SYNC (mail
/// waiting for a station, relayable holdings, its own bulletins). Stations
/// ask only those that set it.
pub const FLAG_HOLDING: u8 = 0x08;

/// A station heard recently by the beaconing station.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Heard {
    pub call: Callsign,
    /// Minutes since last heard; 255 means 255 or more.
    pub minutes: u8,
}

/// Presence and identification: BEACON frame payload, broadcast.
///
/// ```text
/// flags     u8        FLAG_MAILBOX | FLAG_RELAY | FLAG_INTERNET | FLAG_HOLDING;
///                     other bits 0, and ignored by receivers
/// key id    [u8; 8]   which key signed it (hm_ident::PublicKey::id)
/// time      u32       Unix seconds when sent
/// locator   [u8; 6]   Maidenhead locator, upper case ASCII, 4 characters
///                     followed by two zero bytes, or all zero for none
/// n         u8        stations heard, at most 16
/// heard     n x (callsign [u8; 6], minutes u8)
/// signature [u8; 64]  Ed25519 over
///                     "hm/beacon-sig/v0" || header source callsign || all bytes before it
/// ```
///
/// Decoding checks the layout only; the signature is checked by whoever holds
/// the Ed25519 implementation and the trusted key, over
/// [`Beacon::signed_bytes`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Beacon {
    pub flags: u8,
    pub key_id: [u8; KEY_ID_LEN],
    pub time: u32,
    /// Where the station is, as precisely as it chose to say.
    pub locator: Option<Locator>,
    pub heard: Vec<Heard>,
    pub signature: [u8; SIG_LEN],
}

impl Beacon {
    /// The payload without its signature: what the signature covers after
    /// the prefix and the source callsign.
    pub fn body(&self) -> Result<Vec<u8>, WireError> {
        if self.heard.len() > MAX_HEARD {
            return Err(WireError::OutOfRange);
        }
        let mut b = Vec::with_capacity(FIXED + 7 * self.heard.len() + SIG_LEN);
        b.push(self.flags);
        b.extend_from_slice(&self.key_id);
        b.extend_from_slice(&self.time.to_be_bytes());
        b.extend_from_slice(&Locator::option_bytes(self.locator));
        b.push(self.heard.len() as u8);
        for h in &self.heard {
            b.extend_from_slice(&h.call.to_bytes());
            b.push(h.minutes);
        }
        Ok(b)
    }

    /// The statement the signature is made over, for a beacon from `src`
    /// (the frame header's source callsign, exactly as sent).
    pub fn signed_bytes(&self, src: Callsign) -> Result<Vec<u8>, WireError> {
        let body = self.body()?;
        let mut s = Vec::with_capacity(BEACON_SIG_PREFIX.len() + 6 + body.len());
        s.extend_from_slice(BEACON_SIG_PREFIX);
        s.extend_from_slice(&src.to_bytes());
        s.extend_from_slice(&body);
        Ok(s)
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, WireError> {
        let mut b = self.body()?;
        b.extend_from_slice(&self.signature);
        Ok(b)
    }

    pub fn decode(p: &[u8]) -> Result<Beacon, WireError> {
        if p.len() < FIXED + SIG_LEN {
            return Err(WireError::TooShort);
        }
        let n = p[FIXED - 1] as usize;
        if n > MAX_HEARD {
            return Err(WireError::OutOfRange);
        }
        let len = FIXED + 7 * n + SIG_LEN;
        if p.len() < len {
            return Err(WireError::TooShort);
        }
        if p.len() > len {
            return Err(WireError::Trailing);
        }
        let mut heard = Vec::with_capacity(n);
        for i in 0..n {
            let at = FIXED + 7 * i;
            heard.push(Heard {
                call: Callsign::from_bytes(p[at..at + 6].try_into().expect("6 bytes"))?,
                minutes: p[at + 6],
            });
        }
        Ok(Beacon {
            flags: p[0],
            key_id: p[1..9].try_into().expect("8 bytes"),
            time: u32::from_be_bytes(p[9..13].try_into().expect("4 bytes")),
            locator: Locator::from_bytes(p[13..19].try_into().expect("6 bytes"))?,
            heard,
            signature: p[len - SIG_LEN..].try_into().expect("64 bytes"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Beacon {
        Beacon {
            flags: FLAG_MAILBOX | FLAG_INTERNET,
            key_id: [7; 8],
            time: 1_790_000_000,
            locator: Some(Locator::parse("JO89ab").unwrap()),
            heard: vec![
                Heard {
                    call: Callsign::parse("SO5KM-1").unwrap(),
                    minutes: 3,
                },
                Heard {
                    call: Callsign::parse("SP5AAA").unwrap(),
                    minutes: 255,
                },
            ],
            signature: [9; 64],
        }
    }

    #[test]
    fn roundtrip_and_layout() {
        let b = sample();
        let v = b.to_vec().unwrap();
        assert_eq!(v.len(), 20 + 14 + 64);
        assert_eq!(v[0], 0x05);
        assert_eq!(&v[1..9], &[7; 8]);
        assert_eq!(&v[9..13], &1_790_000_000u32.to_be_bytes());
        assert_eq!(&v[13..19], b"JO89AB");
        assert_eq!(v[19], 2);
        assert_eq!(Beacon::decode(&v).unwrap(), b);
        let s = b.signed_bytes(Callsign::parse("SA0KAM").unwrap()).unwrap();
        assert_eq!(&s[..16], BEACON_SIG_PREFIX);
        assert_eq!(&s[16..22], &Callsign::parse("SA0KAM").unwrap().to_bytes());
        assert_eq!(&s[22..], &v[..v.len() - 64]);
    }

    #[test]
    fn bad_lengths_and_counts_are_rejected() {
        let v = sample().to_vec().unwrap();
        assert_eq!(Beacon::decode(&v[..v.len() - 1]), Err(WireError::TooShort));
        let mut long = v.clone();
        long.push(0);
        assert_eq!(Beacon::decode(&long), Err(WireError::Trailing));
        let mut many = v.clone();
        many[19] = 17;
        assert_eq!(Beacon::decode(&many), Err(WireError::OutOfRange));
        let mut too_many = sample();
        too_many.heard = vec![too_many.heard[0]; 17];
        assert_eq!(too_many.to_vec(), Err(WireError::OutOfRange));
        let mut zero_call = v.clone();
        zero_call[20..26].fill(0);
        assert!(Beacon::decode(&zero_call).is_err());
        let mut bad_grid = v.clone();
        bad_grid[13] = b'Z';
        assert_eq!(Beacon::decode(&bad_grid), Err(WireError::BadLocator));
        let mut none = sample();
        none.locator = None;
        let w = none.to_vec().unwrap();
        assert_eq!(&w[13..19], &[0; 6]);
        assert_eq!(Beacon::decode(&w).unwrap(), none);
    }
}
