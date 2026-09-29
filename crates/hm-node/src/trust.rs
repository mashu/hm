//! The stations whose keys this station trusts.

use std::collections::BTreeMap;

use hm_ident::PublicKey;
use hm_wire::Callsign;

/// Public keys of stations whose messages we can verify.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trust {
    keys: BTreeMap<Callsign, PublicKey>,
}

impl Trust {
    /// A station's key as one line, `CALL KEY`, the way `hm whoami` prints it:
    /// `None` for a blank line or a comment.
    pub fn parse_line(line: &str) -> Result<Option<(Callsign, PublicKey)>, String> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }
        let (call, key) = line
            .split_once(char::is_whitespace)
            .ok_or("expected `CALL PUBLICKEY`")?;
        let call = Callsign::parse(call).map_err(|e| e.to_string())?;
        let key = decode_32(key.trim())?;
        Ok(Some((call, PublicKey(key))))
    }

    /// Stop trusting exactly `call`; true if it had an entry of its own.
    pub fn remove(&mut self, call: Callsign) -> bool {
        self.keys.remove(&call).is_some()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Trust `key` for `call`: that station only when `call` has an SSID,
    /// every SSID without a line of its own when it has none.
    pub fn insert(&mut self, call: Callsign, key: PublicKey) {
        self.keys.insert(call, key);
    }

    /// Every trusted station and its key.
    pub fn iter(&self) -> impl Iterator<Item = (Callsign, PublicKey)> + '_ {
        self.keys.iter().map(|(c, k)| (*c, *k))
    }

    /// The key for a station: its own line, else the line for its base callsign.
    pub fn key_for(&self, call: Callsign) -> Option<PublicKey> {
        self.keys
            .get(&call)
            .or_else(|| self.keys.get(&call.base()))
            .copied()
    }
}

/// 32 bytes from 64 hex digits.
fn decode_32(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return Err("expected 32 bytes (64 hex digits)".into());
    }
    let mut out = [0_u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("bad hex at position {}", 2 * i))?;
    }
    Ok(out)
}
