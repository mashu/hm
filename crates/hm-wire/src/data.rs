use crate::WireError;

/// Bytes of [`DataPreamble`] at the start of every DATA frame payload.
pub const DATA_PREAMBLE_LEN: usize = 4;

/// Largest object a transfer can carry (24-bit length).
pub const MAX_OBJECT_LEN: u32 = 0x00FF_FFFF;

/// Start of a DATA frame payload; the RaptorQ symbol follows.
///
/// ```text
/// object_len  u24  length of the whole object being transferred
/// remaining   u8   DATA frames still to come in this over after this one
/// ```
///
/// The frame header's `index` is the symbol's encoding symbol id (ESI) and its
/// `session` names the transfer. The symbol size is the payload length minus 4.
/// `object_len` in every frame lets a receiver that missed the OFFER start
/// collecting symbols anyway; `remaining` tells it when the sender's over ends,
/// so it knows when to answer on a half-duplex channel.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DataPreamble {
    pub object_len: u32,
    pub remaining: u8,
}

impl DataPreamble {
    pub fn to_bytes(&self) -> Result<[u8; DATA_PREAMBLE_LEN], WireError> {
        if self.object_len > MAX_OBJECT_LEN {
            return Err(WireError::OutOfRange);
        }
        let l = self.object_len.to_be_bytes();
        Ok([l[1], l[2], l[3], self.remaining])
    }

    /// Split a DATA payload into preamble and symbol.
    pub fn decode(payload: &[u8]) -> Result<(DataPreamble, &[u8]), WireError> {
        if payload.len() < DATA_PREAMBLE_LEN {
            return Err(WireError::TooShort);
        }
        let object_len = u32::from_be_bytes([0, payload[0], payload[1], payload[2]]);
        Ok((
            DataPreamble {
                object_len,
                remaining: payload[3],
            },
            &payload[DATA_PREAMBLE_LEN..],
        ))
    }
}

/// CTRL message type byte for [`Offer`].
pub const CTRL_OFFER: u8 = 0x01;

/// Bytes of an encoded [`Offer`] (type byte included).
pub const OFFER_LEN: usize = 40;

/// Announces a transfer. Sent as the first frame of the first over, and again
/// when the receiver's ACK asks for it. CTRL frame payload, 40 bytes:
///
/// ```text
/// type         u8    0x01
/// hash         32 B  BLAKE3 derive_key("hm-net 2026-09 xfer v0", object)
/// object_len   u24
/// symbol_size  u16   multiple of 8
/// precedence   u8    as in bundles: 0 routine .. 3 flash
/// remaining    u8    DATA frames following in this over
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Offer {
    pub hash: [u8; 32],
    pub object_len: u32,
    pub symbol_size: u16,
    pub precedence: u8,
    pub remaining: u8,
}

impl Offer {
    pub fn to_bytes(&self) -> Result<[u8; OFFER_LEN], WireError> {
        if self.object_len > MAX_OBJECT_LEN {
            return Err(WireError::OutOfRange);
        }
        let mut b = [0u8; OFFER_LEN];
        b[0] = CTRL_OFFER;
        b[1..33].copy_from_slice(&self.hash);
        b[33..36].copy_from_slice(&self.object_len.to_be_bytes()[1..4]);
        b[36..38].copy_from_slice(&self.symbol_size.to_be_bytes());
        b[38] = self.precedence;
        b[39] = self.remaining;
        Ok(b)
    }

    pub fn decode(payload: &[u8]) -> Result<Offer, WireError> {
        if payload.len() < OFFER_LEN {
            return Err(WireError::TooShort);
        }
        if payload.len() > OFFER_LEN {
            return Err(WireError::Trailing);
        }
        if payload[0] != CTRL_OFFER {
            return Err(WireError::OutOfRange);
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&payload[1..33]);
        Ok(Offer {
            hash,
            object_len: u32::from_be_bytes([0, payload[33], payload[34], payload[35]]),
            symbol_size: u16::from_be_bytes([payload[36], payload[37]]),
            precedence: payload[38],
            remaining: payload[39],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn preamble_layout() {
        let p = DataPreamble {
            object_len: 0x012345,
            remaining: 7,
        };
        assert_eq!(p.to_bytes().unwrap(), [0x01, 0x23, 0x45, 7]);
        let mut payload = p.to_bytes().unwrap().to_vec();
        payload.extend_from_slice(b"symbol");
        assert_eq!(DataPreamble::decode(&payload).unwrap(), (p, &b"symbol"[..]));
        assert!(DataPreamble {
            object_len: MAX_OBJECT_LEN + 1,
            remaining: 0
        }
        .to_bytes()
        .is_err());
        assert_eq!(DataPreamble::decode(&[1, 2, 3]), Err(WireError::TooShort));
    }

    #[test]
    fn offer_layout() {
        let o = Offer {
            hash: [0xAB; 32],
            object_len: 5000,
            symbol_size: 200,
            precedence: 3,
            remaining: 16,
        };
        let b = o.to_bytes().unwrap();
        assert_eq!(b[0], CTRL_OFFER);
        assert_eq!(&b[33..], &[0x00, 0x13, 0x88, 0x00, 0xC8, 3, 16]);
        assert_eq!(Offer::decode(&b).unwrap(), o);
        assert_eq!(Offer::decode(&b[..39]), Err(WireError::TooShort));
        let mut long = b.to_vec();
        long.push(0);
        assert_eq!(Offer::decode(&long), Err(WireError::Trailing));
        let mut wrong = b;
        wrong[0] = 9;
        assert_eq!(Offer::decode(&wrong), Err(WireError::OutOfRange));
    }

    proptest! {
        #[test]
        fn decoders_never_panic_and_roundtrip(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            if let Ok((p, sym)) = DataPreamble::decode(&bytes) {
                let mut again = p.to_bytes().unwrap().to_vec();
                again.extend_from_slice(sym);
                prop_assert_eq!(again, bytes.clone());
            }
            if let Ok(o) = Offer::decode(&bytes) {
                prop_assert_eq!(o.to_bytes().unwrap().to_vec(), bytes);
            }
        }
    }
}
