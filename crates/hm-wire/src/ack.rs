use alloc::vec::Vec;

use crate::WireError;

/// Most completed-object prefixes one ACK may carry.
pub const MAX_ACK_COMPLETED: usize = 16;

/// `need` value meaning "I have symbols for this session but missed the OFFER;
/// send it again".
pub const NEED_OFFER: u16 = 0xFFFF;

/// Bytes of the optional receipt signature at the end of an ACK.
pub const RECEIPT_LEN: usize = 64;

/// Acknowledgement payload of a [`crate::FrameType::Ack`] frame.
///
/// Binary layout, big-endian, 7 + 8n bytes, plus 64 when a receipt is present:
///
/// ```text
/// need      u16   symbols still needed for the current object
///                 (0 = done, 0xFFFF = send the OFFER again)
/// snr       i8    received SNR in dB, -128 = unknown
/// mode      u8    suggested modem mode for the peer, 255 = no suggestion
/// credit    u16   airtime the peer may use before the next ACK, in ms
/// n         u8    number of completed object prefixes that follow (<= 16)
/// prefix[n] 8 B   first 8 bytes of each completed object id
/// receipt   64 B  optional: the receiver's Ed25519 signature proving completion
/// ```
///
/// The receipt is present exactly when the payload is 64 bytes longer than
/// the prefixes require.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ack {
    pub need: u16,
    /// -127..=127 when known.
    pub snr_db: Option<i8>,
    /// 0..=254 when present.
    pub mode_hint: Option<u8>,
    pub credit_ms: u16,
    pub completed: Vec<[u8; 8]>,
    pub receipt: Option<[u8; RECEIPT_LEN]>,
}

impl Ack {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        if self.completed.len() > MAX_ACK_COMPLETED
            || self.snr_db == Some(i8::MIN)
            || self.mode_hint == Some(u8::MAX)
        {
            return Err(WireError::OutOfRange);
        }
        out.extend_from_slice(&self.need.to_be_bytes());
        out.push(self.snr_db.unwrap_or(i8::MIN) as u8);
        out.push(self.mode_hint.unwrap_or(u8::MAX));
        out.extend_from_slice(&self.credit_ms.to_be_bytes());
        out.push(self.completed.len() as u8);
        for p in &self.completed {
            out.extend_from_slice(p);
        }
        if let Some(r) = &self.receipt {
            out.extend_from_slice(r);
        }
        Ok(())
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, WireError> {
        let mut v = Vec::with_capacity(7 + 8 * self.completed.len() + RECEIPT_LEN);
        self.encode(&mut v)?;
        Ok(v)
    }

    pub fn decode(b: &[u8]) -> Result<Ack, WireError> {
        if b.len() < 7 {
            return Err(WireError::TooShort);
        }
        let n = b[6] as usize;
        if n > MAX_ACK_COMPLETED {
            return Err(WireError::OutOfRange);
        }
        let expected = 7 + 8 * n;
        if b.len() < expected {
            return Err(WireError::TooShort);
        }
        let receipt = match b.len() - expected {
            0 => None,
            RECEIPT_LEN => {
                let mut r = [0u8; RECEIPT_LEN];
                r.copy_from_slice(&b[expected..]);
                Some(r)
            }
            _ => return Err(WireError::Trailing),
        };
        let snr = b[2] as i8;
        let completed = b[7..expected].as_chunks::<8>().0.to_vec();
        Ok(Ack {
            need: u16::from_be_bytes([b[0], b[1]]),
            snr_db: if snr == i8::MIN { None } else { Some(snr) },
            mode_hint: if b[3] == u8::MAX { None } else { Some(b[3]) },
            credit_ms: u16::from_be_bytes([b[4], b[5]]),
            completed,
            receipt,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use proptest::prelude::*;

    #[test]
    fn roundtrip_and_layout() {
        let a = Ack {
            need: 3,
            snr_db: Some(-4),
            mode_hint: Some(2),
            credit_ms: 1500,
            completed: vec![[1, 2, 3, 4, 5, 6, 7, 8]],
            receipt: None,
        };
        let b = a.to_vec().unwrap();
        assert_eq!(b, [0, 3, 0xFC, 2, 0x05, 0xDC, 1, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(Ack::decode(&b).unwrap(), a);
    }

    #[test]
    fn receipt_is_the_trailing_64_bytes() {
        let a = Ack {
            completed: vec![[9; 8]],
            receipt: Some([0xAB; 64]),
            ..Ack::default()
        };
        let b = a.to_vec().unwrap();
        assert_eq!(b.len(), 7 + 8 + 64);
        assert_eq!(Ack::decode(&b).unwrap(), a);
        assert_eq!(Ack::decode(&b[..b.len() - 1]), Err(WireError::Trailing));
    }

    #[test]
    fn unknown_values_roundtrip_as_none() {
        let a = Ack::default();
        let b = a.to_vec().unwrap();
        assert_eq!(b, [0, 0, 0x80, 0xFF, 0, 0, 0]);
        assert_eq!(Ack::decode(&b).unwrap(), a);
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(Ack {
            snr_db: Some(i8::MIN),
            ..Ack::default()
        }
        .to_vec()
        .is_err());
        assert!(Ack {
            mode_hint: Some(255),
            ..Ack::default()
        }
        .to_vec()
        .is_err());
        assert!(Ack {
            completed: vec![[0; 8]; 17],
            ..Ack::default()
        }
        .to_vec()
        .is_err());
        assert_eq!(Ack::decode(&[0, 0, 0, 0, 0, 0, 1]), Err(WireError::TooShort));
        assert_eq!(Ack::decode(&[0, 0, 0, 0, 0, 0, 0, 9]), Err(WireError::Trailing));
        assert_eq!(Ack::decode(&[0, 0, 0, 0, 0, 0, 17]), Err(WireError::OutOfRange));
    }

    proptest! {
        #[test]
        fn decode_never_panics_and_roundtrips(bytes in proptest::collection::vec(any::<u8>(), 0..160)) {
            if let Ok(a) = Ack::decode(&bytes) {
                prop_assert_eq!(a.to_vec().unwrap(), bytes);
            }
        }
    }
}
