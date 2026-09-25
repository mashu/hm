use core::fmt;
use core::str::FromStr;

use minicbor::decode::{self, Decoder};
use minicbor::encode::{self, Encoder, Write};
use minicbor::{Decode, Encode};

use crate::WireError;

/// Maximum callsign length in characters (40^9 < 2^48).
pub const CALLSIGN_MAX_LEN: usize = 9;

/// Largest packed value that encodes a callsign: 40^9 - 1.
const MAX_PACKED: u64 = 262_143_999_999_999;

/// A station address: up to 9 characters from `A-Z 0-9 - / .`, packed base-40
/// into 48 bits. The first character is the least significant digit, and digit
/// 0 is reserved for "no character", so every valid value maps to exactly one
/// string. Input is normalised to upper case.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Callsign(u64);

fn digit(c: u8) -> Option<u64> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u64 + 1),
        b'a'..=b'z' => Some((c - b'a') as u64 + 1),
        b'0'..=b'9' => Some((c - b'0') as u64 + 27),
        b'-' => Some(37),
        b'/' => Some(38),
        b'.' => Some(39),
        _ => None,
    }
}

fn symbol(d: u64) -> u8 {
    match d {
        1..=26 => b'A' + (d - 1) as u8,
        27..=36 => b'0' + (d - 27) as u8,
        37 => b'-',
        38 => b'/',
        39 => b'.',
        _ => b'?',
    }
}

impl Callsign {
    /// Parse and normalise a callsign such as `SA0KAM` or `so5km-7`.
    pub fn parse(s: &str) -> Result<Callsign, WireError> {
        let bytes = s.as_bytes();
        if bytes.is_empty() || bytes.len() > CALLSIGN_MAX_LEN {
            return Err(WireError::BadCallsign);
        }
        let mut v: u64 = 0;
        for &c in bytes.iter().rev() {
            v = v * 40 + digit(c).ok_or(WireError::BadCallsign)?;
        }
        Ok(Callsign(v))
    }

    /// Rebuild from a packed 48-bit value, rejecting values that no string maps to.
    pub fn from_packed(v: u64) -> Result<Callsign, WireError> {
        if v == 0 || v > MAX_PACKED {
            return Err(WireError::BadCallsign);
        }
        // A zero digit is only allowed above the most significant character.
        let mut x = v;
        while x > 0 {
            if x.is_multiple_of(40) {
                return Err(WireError::BadCallsign);
            }
            x /= 40;
        }
        Ok(Callsign(v))
    }

    pub const fn packed(self) -> u64 {
        self.0
    }

    /// Big-endian 6-byte form used in frame headers and CBOR.
    pub fn to_bytes(self) -> [u8; 6] {
        let b = self.0.to_be_bytes();
        [b[2], b[3], b[4], b[5], b[6], b[7]]
    }

    pub fn from_bytes(b: [u8; 6]) -> Result<Callsign, WireError> {
        let v = u64::from_be_bytes([0, 0, b[0], b[1], b[2], b[3], b[4], b[5]]);
        Callsign::from_packed(v)
    }

    /// Write the characters into `buf`, returning how many were written.
    pub fn write_chars(self, buf: &mut [u8; CALLSIGN_MAX_LEN]) -> usize {
        let mut v = self.0;
        let mut n = 0;
        while v > 0 {
            buf[n] = symbol(v % 40);
            v /= 40;
            n += 1;
        }
        n
    }

    /// The callsign without any `-SSID` suffix, e.g. `SA0KAM` for `SA0KAM-7`.
    pub fn base(self) -> Callsign {
        let mut buf = [0u8; CALLSIGN_MAX_LEN];
        let n = self.write_chars(&mut buf);
        let end = buf[..n].iter().position(|&c| c == b'-').unwrap_or(n);
        if end == 0 {
            return self;
        }
        // Safe: characters come from the base-40 alphabet, which is ASCII.
        let s = core::str::from_utf8(&buf[..end]).unwrap_or("");
        Callsign::parse(s).unwrap_or(self)
    }
}

impl fmt::Display for Callsign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; CALLSIGN_MAX_LEN];
        let n = self.write_chars(&mut buf);
        f.write_str(core::str::from_utf8(&buf[..n]).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for Callsign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Callsign({self})")
    }
}

impl FromStr for Callsign {
    type Err = WireError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Callsign::parse(s)
    }
}

impl<C> Encode<C> for Callsign {
    fn encode<W: Write>(&self, e: &mut Encoder<W>, _: &mut C) -> Result<(), encode::Error<W::Error>> {
        e.bytes(&self.to_bytes())?;
        Ok(())
    }
}

impl<'b, C> Decode<'b, C> for Callsign {
    fn decode(d: &mut Decoder<'b>, _: &mut C) -> Result<Self, decode::Error> {
        let pos = d.position();
        let b = d.bytes()?;
        let arr: [u8; 6] = b
            .try_into()
            .map_err(|_| decode::Error::message("callsign must be 6 bytes").at(pos))?;
        Callsign::from_bytes(arr).map_err(|_| decode::Error::message("invalid callsign").at(pos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use proptest::prelude::*;

    #[test]
    fn roundtrip_known_callsigns() {
        for s in [
            "SA0KAM",
            "SO5KM",
            "SO5KM-7",
            "SP5ABC/P",
            "A",
            "123456789",
            "DL3KGS/MM",
        ] {
            let c = Callsign::parse(s).unwrap();
            assert_eq!(c.to_string(), s);
            assert_eq!(Callsign::from_bytes(c.to_bytes()).unwrap(), c);
        }
    }

    #[test]
    fn lower_case_is_normalised() {
        assert_eq!(
            Callsign::parse("sa0kam").unwrap(),
            Callsign::parse("SA0KAM").unwrap()
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Callsign::parse("").is_err());
        assert!(Callsign::parse("TOOLONGCALL").is_err());
        assert!(Callsign::parse("SA0 KAM").is_err());
        assert!(Callsign::parse("SA0KAMÄ").is_err());
        assert!(Callsign::from_packed(0).is_err());
        assert!(Callsign::from_packed(MAX_PACKED + 1).is_err());
        // "A", a zero digit, then "A": an interior gap that no string produces.
        assert!(Callsign::from_packed(1 + 40 * 40).is_err());
    }

    #[test]
    fn known_packing() {
        // "A" = 1; "AB" = 1 + 2*40.
        assert_eq!(Callsign::parse("A").unwrap().packed(), 1);
        assert_eq!(Callsign::parse("AB").unwrap().packed(), 81);
        // S=19 A=1 0=27 K=11 A=1 M=13:
        // 19 + 1*40 + 27*40^2 + 11*40^3 + 1*40^4 + 13*40^5 = 1_334_507_259 = 0x4F8AF6FB
        let c = Callsign::parse("SA0KAM").unwrap();
        assert_eq!(c.packed(), 1_334_507_259);
        assert_eq!(c.to_bytes(), [0x00, 0x00, 0x4F, 0x8A, 0xF6, 0xFB]);
    }

    #[test]
    fn base_strips_ssid() {
        let c = Callsign::parse("SO5KM-7").unwrap();
        assert_eq!(c.base().to_string(), "SO5KM");
        assert_eq!(Callsign::parse("SA0KAM").unwrap().base().to_string(), "SA0KAM");
    }

    proptest! {
        #[test]
        fn any_valid_string_roundtrips(s in "[A-Z0-9/.-]{1,9}") {
            let c = Callsign::parse(&s).unwrap();
            prop_assert_eq!(c.to_string(), s);
            prop_assert_eq!(Callsign::from_packed(c.packed()).unwrap(), c);
        }

        #[test]
        fn from_bytes_never_panics(b in any::<[u8; 6]>()) {
            if let Ok(c) = Callsign::from_bytes(b) {
                prop_assert_eq!(Callsign::parse(&c.to_string()).unwrap(), c);
            }
        }
    }
}
