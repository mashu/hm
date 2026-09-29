//! Station key files, and the list of trusted stations in memory.
//!
//! Key file (keep private), TOML:
//! ```toml
//! # hm-net station key. Keep this file private.
//! call = "SA0KAM-2"
//! secret = "3f0c..."   # 64 hex digits
//! ```
//!
//! A station shares its public key as one line, `CALL KEY`, which `hm whoami`
//! prints and `hm trust add` (or the web page) takes. Trusted stations are
//! kept in `station.toml` (see [`crate::config`]).
//!
//! Every SSID is a station of its own and may have its own key: SA0KAM-1 and
//! SA0KAM-2 can be two nodes. An entry with an SSID names exactly that station;
//! one without covers all SSIDs of the callsign that have no entry of their own.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use hm_ident::Identity;
use hm_wire::Callsign;

use crate::hex;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// A station's callsign and private key. A key file without an SSID lets the
/// station choose one when it starts (`--ssid`); one with an SSID is that
/// station only.
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
            call,
            identity: Identity::from_secret(secret),
        })
    }

    pub fn to_text(&self) -> String {
        format!(
            "# hm-net station key. Keep this file private.\ncall = \"{}\"\nsecret = \"{}\"\n",
            self.call,
            hex::encode(&self.identity.secret())
        )
    }

    pub fn parse(text: &str) -> io::Result<KeyFile> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            call: String,
            secret: String,
        }
        let raw: Raw = toml::from_str(text).map_err(|e| invalid(format!("key file: {e}")))?;
        let call = Callsign::parse(&raw.call).map_err(|e| invalid(format!("key file: call: {e}")))?;
        let secret = hex::decode_32(&raw.secret).map_err(|e| invalid(format!("key file: secret: {e}")))?;
        Ok(KeyFile {
            call,
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

    /// The line other stations give `hm trust add`.
    pub fn trust_line(&self) -> String {
        format!("{} {}", self.call, hex::encode(&self.identity.public().0))
    }
}

pub use hm_node::Trust;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_roundtrip_keeps_the_ssid() {
        let k = KeyFile::generate(Callsign::parse("SA0KAM-7").unwrap()).unwrap();
        assert_eq!(k.call.to_string(), "SA0KAM-7");
        assert!(k.trust_line().starts_with("SA0KAM-7 "));
        let back = KeyFile::parse(&k.to_text()).unwrap();
        assert_eq!((back.call, back.identity.public()), (k.call, k.identity.public()));
        assert!(k.to_text().contains("call = \"SA0KAM-7\""));
        assert!(KeyFile::parse("call = \"SA0KAM\"\n").is_err());
        assert!(KeyFile::parse("call = \"SA0KAM\"\nsecret = \"00\"\n").is_err());
        assert!(
            KeyFile::parse("call SA0KAM\nsecret 00\n").is_err(),
            "the old format is gone"
        );
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
    fn trust_entries_name_one_station_or_every_ssid() {
        let call = |s: &str| Callsign::parse(s).unwrap();
        let base = KeyFile::generate(call("SO5KM")).unwrap();
        let one = KeyFile::generate(call("SO5KM-2")).unwrap();
        let entry = |l: &str| Trust::parse_line(l).unwrap().unwrap();
        let mut t = Trust::default();
        for (c, k) in [entry(&base.trust_line()), entry(&one.trust_line())] {
            t.insert(c, k);
        }
        // An entry without an SSID covers every SSID with no entry of its own...
        assert_eq!(t.key_for(call("SO5KM")), Some(base.identity.public()));
        assert_eq!(t.key_for(call("SO5KM-1")), Some(base.identity.public()));
        // ...and an entry with one names exactly that station.
        assert_eq!(t.key_for(call("SO5KM-2")), Some(one.identity.public()));
        assert_eq!(t.key_for(call("SA0KAM")), None);
        let mut only_two = Trust::default();
        let (c, k) = entry(&one.trust_line());
        only_two.insert(c, k);
        assert_eq!(only_two.key_for(call("SO5KM-1")), None);
        assert_eq!(only_two.key_for(call("SO5KM")), None);
        assert_eq!(Trust::parse_line("  # a comment"), Ok(None));
        assert!(Trust::parse_line("SO5KM").is_err());
        assert!(Trust::parse_line("SO5KM 1234").is_err());
    }
}
