use alloc::vec::Vec;

use crate::{Callsign, WireError};

/// Version carried in the high nibble of every frame header. 0 = draft.
pub const WIRE_VERSION: u8 = 0;

/// Header length in bytes.
pub const HEADER_LEN: usize = 18;

/// Largest value of the 24-bit `index` field.
pub const MAX_INDEX: u32 = 0x00FF_FFFF;

const BROADCAST: [u8; 6] = [0xFF; 6];

/// Frame type, low nibble of byte 0.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameType {
    /// One RaptorQ symbol of an object being transferred.
    Data = 0,
    /// Acknowledgement and channel feedback, payload is [`crate::Ack`].
    Ack = 1,
    /// Set-reconciliation message.
    Sync = 2,
    /// Session open/close and feature negotiation.
    Ctrl = 3,
    /// Node presence and station identification.
    Beacon = 4,
}

impl FrameType {
    fn from_nibble(n: u8) -> Result<FrameType, WireError> {
        Ok(match n {
            0 => FrameType::Data,
            1 => FrameType::Ack,
            2 => FrameType::Sync,
            3 => FrameType::Ctrl,
            4 => FrameType::Beacon,
            other => return Err(WireError::UnknownFrameType(other)),
        })
    }
}

/// Frame destination.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dest {
    Station(Callsign),
    /// All stations on the channel (`FF FF FF FF FF FF`).
    Broadcast,
}

/// The header carried in clear by every radio frame.
///
/// ```text
/// byte 0      version (high nibble) | frame type (low nibble)
/// bytes 1-6   source callsign, base-40, big-endian
/// bytes 7-12  destination callsign, or FF..FF for broadcast
/// bytes 13-14 session id, big-endian
/// bytes 15-17 index (symbol or sequence number), big-endian
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameHeader {
    pub ftype: FrameType,
    pub src: Callsign,
    pub dst: Dest,
    pub session: u16,
    /// 24-bit value; must not exceed [`MAX_INDEX`].
    pub index: u32,
}

impl FrameHeader {
    /// Append the 18 header bytes to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        if self.index > MAX_INDEX {
            return Err(WireError::OutOfRange);
        }
        out.push((WIRE_VERSION << 4) | self.ftype as u8);
        out.extend_from_slice(&self.src.to_bytes());
        match self.dst {
            Dest::Station(c) => out.extend_from_slice(&c.to_bytes()),
            Dest::Broadcast => out.extend_from_slice(&BROADCAST),
        }
        out.extend_from_slice(&self.session.to_be_bytes());
        let i = self.index.to_be_bytes();
        out.extend_from_slice(&i[1..4]);
        Ok(())
    }

    /// Build a complete frame: header followed by `payload`.
    pub fn frame(&self, payload: &[u8]) -> Result<Vec<u8>, WireError> {
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        self.encode(&mut out)?;
        out.extend_from_slice(payload);
        Ok(out)
    }

    /// Split a received frame into its header and payload.
    pub fn decode(frame: &[u8]) -> Result<(FrameHeader, &[u8]), WireError> {
        if frame.len() < HEADER_LEN {
            return Err(WireError::TooShort);
        }
        let version = frame[0] >> 4;
        if version != WIRE_VERSION {
            return Err(WireError::BadVersion(version));
        }
        let ftype = FrameType::from_nibble(frame[0] & 0x0F)?;
        let src = Callsign::from_bytes(six(&frame[1..7]))?;
        let dst_bytes = six(&frame[7..13]);
        let dst = if dst_bytes == BROADCAST {
            Dest::Broadcast
        } else {
            Dest::Station(Callsign::from_bytes(dst_bytes)?)
        };
        let session = u16::from_be_bytes([frame[13], frame[14]]);
        let index = u32::from_be_bytes([0, frame[15], frame[16], frame[17]]);
        Ok((
            FrameHeader {
                ftype,
                src,
                dst,
                session,
                index,
            },
            &frame[HEADER_LEN..],
        ))
    }
}

fn six(b: &[u8]) -> [u8; 6] {
    let mut a = [0u8; 6];
    a.copy_from_slice(b);
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample() -> FrameHeader {
        FrameHeader {
            ftype: FrameType::Data,
            src: Callsign::parse("SA0KAM").unwrap(),
            dst: Dest::Station(Callsign::parse("SO5KM-1").unwrap()),
            session: 0xBEEF,
            index: 0x012345,
        }
    }

    #[test]
    fn roundtrip_and_layout() {
        let h = sample();
        let f = h.frame(b"hello").unwrap();
        assert_eq!(f.len(), HEADER_LEN + 5);
        assert_eq!(f[0], 0x00);
        assert_eq!(&f[13..15], &[0xBE, 0xEF]);
        assert_eq!(&f[15..18], &[0x01, 0x23, 0x45]);
        let (d, payload) = FrameHeader::decode(&f).unwrap();
        assert_eq!(d, h);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn broadcast_roundtrip() {
        let mut h = sample();
        h.dst = Dest::Broadcast;
        h.ftype = FrameType::Beacon;
        let f = h.frame(&[]).unwrap();
        assert_eq!(&f[7..13], &[0xFF; 6]);
        assert_eq!(FrameHeader::decode(&f).unwrap().0, h);
    }

    #[test]
    fn rejects_bad_frames() {
        let f = sample().frame(&[]).unwrap();
        assert_eq!(FrameHeader::decode(&f[..17]), Err(WireError::TooShort));
        let mut v = f.clone();
        v[0] = 0x10;
        assert_eq!(FrameHeader::decode(&v), Err(WireError::BadVersion(1)));
        let mut t = f.clone();
        t[0] = 0x0F;
        assert_eq!(FrameHeader::decode(&t), Err(WireError::UnknownFrameType(15)));
        let mut h = sample();
        h.index = MAX_INDEX + 1;
        assert_eq!(h.frame(&[]), Err(WireError::OutOfRange));
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            if let Ok((h, payload)) = FrameHeader::decode(&bytes) {
                let again = h.frame(payload).unwrap();
                prop_assert_eq!(again, bytes);
            }
        }
    }
}
