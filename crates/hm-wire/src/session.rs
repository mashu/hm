use crate::WireError;

/// CTRL message type byte for [`Open`].
pub const CTRL_OPEN: u8 = 0x02;
/// CTRL message type byte for [`Close`].
pub const CTRL_CLOSE: u8 = 0x03;
/// Bytes of an encoded [`Open`] (type byte included).
pub const OPEN_LEN: usize = 12;
/// Bytes of an encoded [`Close`] (type byte included).
pub const CLOSE_LEN: usize = 4;

/// Feature: holds mail for other stations (a mailbox node).
pub const FEATURE_MAILBOX: u32 = 0x0000_0001;
/// Feature: accepts bundles addressed to other stations and passes them on.
pub const FEATURE_RELAY: u32 = 0x0000_0002;
/// Feature: decodes IL2P-framed frames on this link as well as AX.25.
pub const FEATURE_IL2P: u32 = 0x0000_0004;
/// Feature: reads the compact form of frames on this link: a UI frame to the
/// station's own AX.25 address with a 6-byte header (see `hm-bearer`).
pub const FEATURE_COMPACT: u32 = 0x0000_0008;

/// Flag in [`Open::flags`]: this OPEN answers one from the peer.
pub const OPEN_REPLY: u8 = 0x01;

/// What a station offers and accepts, told to a peer before transfers.
/// CTRL frame payload, 12 bytes:
///
/// ```text
/// type          u8   0x02
/// flags         u8   0x01 reply (answers the peer's OPEN); other bits 0
/// features      u32  FEATURE_* bits; unknown bits are ignored
/// max_object    u24  largest object this station accepts
/// max_symbol    u16  largest symbol size it accepts (a multiple of 8)
/// max_parallel  u8   transfers it takes at once from this peer
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Open {
    pub flags: u8,
    pub features: u32,
    pub max_object: u32,
    pub max_symbol: u16,
    pub max_parallel: u8,
}

impl Open {
    pub fn is_reply(&self) -> bool {
        self.flags & OPEN_REPLY != 0
    }

    pub fn to_bytes(&self) -> Result<[u8; OPEN_LEN], WireError> {
        if self.max_object > crate::MAX_OBJECT_LEN {
            return Err(WireError::OutOfRange);
        }
        let mut b = [0u8; OPEN_LEN];
        b[0] = CTRL_OPEN;
        b[1] = self.flags;
        b[2..6].copy_from_slice(&self.features.to_be_bytes());
        b[6..9].copy_from_slice(&self.max_object.to_be_bytes()[1..4]);
        b[9..11].copy_from_slice(&self.max_symbol.to_be_bytes());
        b[11] = self.max_parallel;
        Ok(b)
    }

    pub fn decode(payload: &[u8]) -> Result<Open, WireError> {
        if payload.len() < OPEN_LEN {
            return Err(WireError::TooShort);
        }
        if payload.len() > OPEN_LEN {
            return Err(WireError::Trailing);
        }
        if payload[0] != CTRL_OPEN {
            return Err(WireError::OutOfRange);
        }
        Ok(Open {
            flags: payload[1],
            features: u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]),
            max_object: u32::from_be_bytes([0, payload[6], payload[7], payload[8]]),
            max_symbol: u16::from_be_bytes([payload[9], payload[10]]),
            max_parallel: payload[11],
        })
    }
}

/// Why a station ends a transfer, or all transfers from a peer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// Nothing more to send for now; the peer may forget our transfers.
    Done,
    /// Too busy: try again after `retry_after` seconds.
    Busy,
    /// Not accepted from this station, now or later.
    Refused,
    /// The object is larger than the receiver accepts.
    TooLarge,
    /// A reason this implementation does not know; treat as refused.
    Other(u8),
}

impl CloseReason {
    pub fn to_u8(self) -> u8 {
        match self {
            CloseReason::Done => 0,
            CloseReason::Busy => 1,
            CloseReason::Refused => 2,
            CloseReason::TooLarge => 3,
            CloseReason::Other(v) => v,
        }
    }

    pub fn from_u8(v: u8) -> CloseReason {
        match v {
            0 => CloseReason::Done,
            1 => CloseReason::Busy,
            2 => CloseReason::Refused,
            3 => CloseReason::TooLarge,
            v => CloseReason::Other(v),
        }
    }
}

/// Ends a transfer (the frame header's session), or every transfer between
/// the two stations (session 0). CTRL frame payload, 4 bytes:
///
/// ```text
/// type         u8   0x03
/// reason       u8   0 done, 1 busy, 2 refused, 3 too large
/// retry_after  u16  seconds before trying again (busy); 0 otherwise
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Close {
    pub reason: CloseReason,
    pub retry_after: u16,
}

impl Close {
    pub fn to_bytes(&self) -> [u8; CLOSE_LEN] {
        let r = self.retry_after.to_be_bytes();
        [CTRL_CLOSE, self.reason.to_u8(), r[0], r[1]]
    }

    pub fn decode(payload: &[u8]) -> Result<Close, WireError> {
        if payload.len() < CLOSE_LEN {
            return Err(WireError::TooShort);
        }
        if payload.len() > CLOSE_LEN {
            return Err(WireError::Trailing);
        }
        if payload[0] != CTRL_CLOSE {
            return Err(WireError::OutOfRange);
        }
        Ok(Close {
            reason: CloseReason::from_u8(payload[1]),
            retry_after: u16::from_be_bytes([payload[2], payload[3]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_layout() {
        let o = Open {
            flags: OPEN_REPLY,
            features: FEATURE_MAILBOX | FEATURE_IL2P,
            max_object: 256 * 1024,
            max_symbol: 256,
            max_parallel: 2,
        };
        let b = o.to_bytes().unwrap();
        assert_eq!(b, [0x02, 0x01, 0, 0, 0, 0x05, 0x04, 0x00, 0x00, 0x01, 0x00, 2]);
        assert_eq!(Open::decode(&b).unwrap(), o);
        assert!(o.is_reply());
        assert_eq!(Open::decode(&b[..11]), Err(WireError::TooShort));
        assert_eq!(Open::decode(&[&b[..], &[0]].concat()), Err(WireError::Trailing));
        let mut other = b;
        other[0] = CTRL_CLOSE;
        assert_eq!(Open::decode(&other), Err(WireError::OutOfRange));
        assert!(Open {
            max_object: crate::MAX_OBJECT_LEN + 1,
            ..o
        }
        .to_bytes()
        .is_err());
    }

    #[test]
    fn close_layout_and_unknown_reasons() {
        let c = Close {
            reason: CloseReason::Busy,
            retry_after: 90,
        };
        assert_eq!(c.to_bytes(), [0x03, 1, 0, 90]);
        assert_eq!(Close::decode(&c.to_bytes()).unwrap(), c);
        let odd = Close::decode(&[0x03, 200, 0, 0]).unwrap();
        assert_eq!(odd.reason, CloseReason::Other(200));
        assert_eq!(odd.to_bytes(), [0x03, 200, 0, 0]);
        assert_eq!(Close::decode(&[0x03, 1, 0]), Err(WireError::TooShort));
        assert_eq!(Close::decode(&[0x01, 1, 0, 0]), Err(WireError::OutOfRange));
    }
}
