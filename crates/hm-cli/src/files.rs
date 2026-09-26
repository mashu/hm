//! Station key files and trust files.
//!
//! Key file (keep private):
//! ```text
//! # hm-net station key. Keep this file private.
//! call SA0KAM
//! secret 3f0c...   (64 hex digits)
//! ```
//!
//! Trust file, one station per line, as printed by `hm whoami`:
//! ```text
//! SO5KM 8a1e...   (64 hex digits of the Ed25519 public key)
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use hm_ident::{Identity, PublicKey};
use hm_wire::Callsign;

use crate::hex;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// A station's callsign (base call, no SSID) and private key.
pub struct KeyFile {
    pub call: Callsign,
    pub identity: Identity,
}

impl KeyFile {
    /// A new key from the operating system's random number generator.
    pub fn generate(call: Callsign) -> io::Result<KeyFile> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| io::Error::other(format!("no system randomness: {e}")))?;
        Ok(KeyFile {
            call: call.base(),
            identity: Identity::from_secret(secret),
        })
    }

    pub fn to_text(&self) -> String {
        format!(
            "# hm-net station key. Keep this file private.\ncall {}\nsecret {}\n",
            self.call,
            hex::encode(&self.identity.secret())
        )
    }

    pub fn parse(text: &str) -> io::Result<KeyFile> {
        let (mut call, mut secret) = (None, None);
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            match line.split_once(char::is_whitespace) {
                Some(("call", v)) => {
                    call = Some(Callsign::parse(v.trim()).map_err(|e| invalid(format!("key file: {e}")))?)
                }
                Some(("secret", v)) => {
                    secret = Some(hex::decode_32(v).map_err(|e| invalid(format!("key file: {e}")))?)
                }
                _ => return Err(invalid(format!("key file: unexpected line {line:?}"))),
            }
        }
        let call = call.ok_or_else(|| invalid("key file: missing `call`"))?;
        let secret = secret.ok_or_else(|| invalid("key file: missing `secret`"))?;
        Ok(KeyFile {
            call: call.base(),
            identity: Identity::from_secret(secret),
        })
    }

    /// Write the key, readable by the owner only on Unix. Refuses to overwrite.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(path)?.write_all(self.to_text().as_bytes())
    }

    pub fn load(path: &Path) -> io::Result<KeyFile> {
        KeyFile::parse(&fs::read_to_string(path)?)
    }

    /// The line other stations put in their trust file.
    pub fn trust_line(&self) -> String {
        format!("{} {}", self.call, hex::encode(&self.identity.public().0))
    }
}

/// Public keys of stations whose messages we can verify.
#[derive(Clone, Debug, Default)]
pub struct Trust {
    keys: BTreeMap<Callsign, PublicKey>,
}

impl Trust {
    pub fn parse(text: &str) -> io::Result<Trust> {
        let mut t = Trust::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (call, key) = line
                .split_once(char::is_whitespace)
                .ok_or_else(|| invalid(format!("trust file line {}: expected `CALL PUBLICKEY`", n + 1)))?;
            let call =
                Callsign::parse(call).map_err(|e| invalid(format!("trust file line {}: {e}", n + 1)))?;
            let key = hex::decode_32(key).map_err(|e| invalid(format!("trust file line {}: {e}", n + 1)))?;
            t.keys.insert(call.base(), PublicKey(key));
        }
        Ok(t)
    }

    pub fn load(path: &Path) -> io::Result<Trust> {
        Trust::parse(&fs::read_to_string(path)?)
    }

    pub fn insert(&mut self, call: Callsign, key: PublicKey) {
        self.keys.insert(call.base(), key);
    }

    /// Every trusted station and its key.
    pub fn iter(&self) -> impl Iterator<Item = (Callsign, PublicKey)> + '_ {
        self.keys.iter().map(|(c, k)| (*c, *k))
    }

    /// The key for a station, ignoring any SSID.
    pub fn key_for(&self, call: Callsign) -> Option<PublicKey> {
        self.keys.get(&call.base()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_roundtrip_strips_ssid() {
        let k = KeyFile::generate(Callsign::parse("SA0KAM-7").unwrap()).unwrap();
        assert_eq!(k.call.to_string(), "SA0KAM");
        let back = KeyFile::parse(&k.to_text()).unwrap();
        assert_eq!(back.identity.public(), k.identity.public());
        assert!(KeyFile::parse("call SA0KAM\n").is_err());
        assert!(KeyFile::parse("secret 00\ncall SA0KAM").is_err());
    }

    #[test]
    fn save_refuses_to_overwrite() {
        let dir = std::env::temp_dir().join(format!("hm-key-test-{}", std::process::id()));
        let _ = fs::remove_file(&dir);
        let k = KeyFile::generate(Callsign::parse("SA0KAM").unwrap()).unwrap();
        k.save(&dir).unwrap();
        assert!(k.save(&dir).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(
            KeyFile::load(&dir).unwrap().identity.public(),
            k.identity.public()
        );
        fs::remove_file(&dir).unwrap();
    }

    #[test]
    fn trust_file_lookup_ignores_ssid() {
        let k = KeyFile::generate(Callsign::parse("SO5KM").unwrap()).unwrap();
        let t = Trust::parse(&format!("# friends\n\n{}\n", k.trust_line())).unwrap();
        assert_eq!(
            t.key_for(Callsign::parse("SO5KM-1").unwrap()),
            Some(k.identity.public())
        );
        assert_eq!(t.key_for(Callsign::parse("SA0KAM").unwrap()), None);
        assert!(Trust::parse("SO5KM").is_err());
        assert!(Trust::parse("SO5KM 1234").is_err());
    }
}
