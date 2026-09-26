//! CRC-16/X.25, the HDLC and AX.25 frame check sequence.
//! Reflected polynomial 0x8408, initial value 0xFFFF, final XOR 0xFFFF,
//! sent least significant byte first.

pub fn crc16_x25(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x8408
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// True if the last two bytes of `frame` are the correct FCS of the rest.
pub fn fcs_ok(frame: &[u8]) -> bool {
    if frame.len() < 3 {
        return false;
    }
    let (data, fcs) = frame.split_at(frame.len() - 2);
    crc16_x25(data) == u16::from_le_bytes([fcs[0], fcs[1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_value() {
        // Standard check value for CRC-16/X-25 over the ASCII digits 1-9.
        assert_eq!(crc16_x25(b"123456789"), 0x906E);
    }

    #[test]
    fn appended_fcs_verifies() {
        let mut f = b"hello".to_vec();
        f.extend_from_slice(&crc16_x25(b"hello").to_le_bytes());
        assert!(fcs_ok(&f));
        f[0] ^= 1;
        assert!(!fcs_ok(&f));
    }
}
