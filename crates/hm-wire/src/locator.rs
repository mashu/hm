use core::fmt;

use crate::WireError;

/// A Maidenhead grid locator of 4 or 6 characters: `JO89` or `JO89ab`.
///
/// On the wire: 6 ASCII bytes, upper case, a 4-character locator followed by
/// two zero bytes; six zero bytes mean no locator.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Locator([u8; 6]);

impl Locator {
    /// Read a locator as people write it, in any case.
    pub fn parse(s: &str) -> Result<Locator, WireError> {
        let b = s.trim().as_bytes();
        if (b.len() != 4 && b.len() != 6) || !b.iter().all(u8::is_ascii_alphanumeric) {
            return Err(WireError::BadLocator);
        }
        let mut out = [0u8; 6];
        for (i, c) in b.iter().enumerate() {
            out[i] = c.to_ascii_uppercase();
        }
        Locator::from_bytes(out)?.ok_or(WireError::BadLocator)
    }

    /// The wire form, or `None` for "no locator".
    pub fn from_bytes(b: [u8; 6]) -> Result<Option<Locator>, WireError> {
        if b == [0; 6] {
            return Ok(None);
        }
        let field = |c: u8| (b'A'..=b'R').contains(&c);
        let square = |c: u8| c.is_ascii_digit();
        let sub = |c: u8| (b'A'..=b'X').contains(&c);
        let ok = field(b[0])
            && field(b[1])
            && square(b[2])
            && square(b[3])
            && ((sub(b[4]) && sub(b[5])) || (b[4] == 0 && b[5] == 0));
        if ok {
            Ok(Some(Locator(b)))
        } else {
            Err(WireError::BadLocator)
        }
    }

    pub fn to_bytes(self) -> [u8; 6] {
        self.0
    }

    /// The wire form of an optional locator.
    pub fn option_bytes(l: Option<Locator>) -> [u8; 6] {
        l.map_or([0; 6], Locator::to_bytes)
    }

    fn len(&self) -> usize {
        if self.0[4] == 0 {
            4
        } else {
            6
        }
    }

    /// The centre of the square, in degrees: (latitude, longitude).
    pub fn centre(&self) -> (f64, f64) {
        let b = self.0;
        let mut lon = -180.0 + f64::from(b[0] - b'A') * 20.0 + f64::from(b[2] - b'0') * 2.0;
        let mut lat = -90.0 + f64::from(b[1] - b'A') * 10.0 + f64::from(b[3] - b'0');
        if self.len() == 6 {
            lon += f64::from(b[4] - b'A') * (2.0 / 24.0) + 1.0 / 24.0;
            lat += f64::from(b[5] - b'A') * (1.0 / 24.0) + 0.5 / 24.0;
        } else {
            lon += 1.0;
            lat += 0.5;
        }
        (lat, lon)
    }
}

impl fmt::Display for Locator {
    /// Written the usual way: field and square upper case, subsquare lower.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, &c) in self.0[..self.len()].iter().enumerate() {
            let c = if i >= 4 { c.to_ascii_lowercase() } else { c };
            write!(f, "{}", c as char)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Locator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Locator({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_and_wire() {
        let l = Locator::parse("jo89AB").unwrap();
        assert_eq!(l.to_string(), "JO89ab");
        assert_eq!(&l.to_bytes(), b"JO89AB");
        let four = Locator::parse("KP20").unwrap();
        assert_eq!(four.to_bytes(), *b"KP20\0\0");
        assert_eq!(Locator::from_bytes(four.to_bytes()), Ok(Some(four)));
        assert_eq!(Locator::from_bytes([0; 6]), Ok(None));
        for bad in [
            "", "JO8", "JO89a", "SO89", "JOA9", "JO89ay", "JO89abc", "JO89\0\0",
        ] {
            assert!(Locator::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Locator::from_bytes(*b"JO89A\0").is_err());
    }

    #[test]
    fn centres() {
        let (lat, lon) = Locator::parse("JO89").unwrap().centre();
        assert!((lat - 59.5).abs() < 1e-9 && (lon - 17.0).abs() < 1e-9);
        // Stockholm, JO89xi: 59.35 N, 18.0 E within a subsquare.
        let (lat, lon) = Locator::parse("JO89xi").unwrap().centre();
        assert!(
            (lat - 59.354).abs() < 0.03 && (lon - 17.958).abs() < 0.05,
            "{lat} {lon}"
        );
    }
}
