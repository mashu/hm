//! Lower-case hex, for keys and ids in text files.

pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("bad hex at position {i}")))
        .collect()
}

pub fn decode_32(s: &str) -> Result<[u8; 32], String> {
    decode(s)?
        .try_into()
        .map_err(|_| "expected 32 bytes (64 hex digits)".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip_and_errors() {
        assert_eq!(super::encode(&[0x00, 0xAB, 0xFF]), "00abff");
        assert_eq!(super::decode("00AbfF").unwrap(), vec![0x00, 0xAB, 0xFF]);
        assert!(super::decode("abc").is_err());
        assert!(super::decode("zz").is_err());
        assert!(super::decode_32("00").is_err());
    }
}
