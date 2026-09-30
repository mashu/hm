//! `station.toml`: a station's settings and the stations it trusts.
//!
//! One file, meant to be read and edited by people as well as by the node:
//!
//! ```toml
//! [station]
//! key = "station.key"
//!
//! [radio]
//! kiss = "127.0.0.1:8001"
//!
//! [[trust]]
//! station = "SO5KM-1"
//! key = "8a1e…"
//! note = "Jan, club station"
//! ```
//!
//! Every setting has a default, so a file needs only what differs. Unknown
//! keys are an error, so a typo does not silently do nothing. Relative paths
//! are relative to the file's directory. The node writes changes made on the
//! web page back with `toml_edit`, which keeps comments and layout.

mod edit;
mod sections;
mod starter;

use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hm_ident::PublicKey;
use hm_route::{Bearer as RouteBearer, ScheduledContact};
use hm_wire::{Callsign, Locator};
use serde::{Deserialize, Serialize};

use crate::files::Trust;
use crate::hex;

pub use edit::{remove_trust, set_modem, set_peers, set_radio, set_relay, set_trust, set_value, unset_value};
pub use hm_node::RelaySettings;
pub use sections::{
    ContactEntry, DeliverySettings, InternetSettings, ModemSettings, PeerEntry, RadioSettings,
    StationSettings, TrustEntry,
};
pub use starter::starter;

pub const DEFAULT_PATH: &str = "station.toml";

/// Public core hub shipped with hm. Home stations trust and dial it by default.
pub mod public_hub {
    use super::{PeerEntry, TrustEntry};

    pub const CALL: &str = "SA0KAM-0";
    pub const KEY_HEX: &str = "3893d7fd50f143c9489552363ce63714c80ea00eff96763896f09af34b53b9af";
    pub const ADDRESS: &str = "34.51.161.47:4433";

    pub fn peer() -> PeerEntry {
        PeerEntry {
            station: CALL.into(),
            address: ADDRESS.into(),
        }
    }

    pub fn trust() -> TrustEntry {
        TrustEntry {
            station: CALL.into(),
            key: KEY_HEX.into(),
            note: Some("public core hub".into()),
        }
    }

    pub fn trust_line() -> String {
        format!("{CALL} {KEY_HEX}")
    }

    /// Drop dial/trust entries for `call` so a station does not peer with itself.
    pub fn omit_self(config: &mut super::Config, call: hm_wire::Callsign) {
        let name = call.to_string();
        config.internet.peers.retain(|p| p.station != name);
        config.trust.retain(|t| t.station != name);
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub station: StationSettings,
    pub radio: RadioSettings,
    pub internet: InternetSettings,
    pub modem: ModemSettings,
    pub delivery: DeliverySettings,
    pub relay: RelaySettings,
    pub contact: Vec<ContactEntry>,
    pub trust: Vec<TrustEntry>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            station: StationSettings::default(),
            radio: RadioSettings::default(),
            internet: InternetSettings::default(),
            modem: ModemSettings::default(),
            delivery: DeliverySettings::default(),
            relay: RelaySettings::default(),
            contact: Vec::new(),
            trust: vec![public_hub::trust()],
        }
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Config {
    /// Read `path`; a file that does not exist gives the defaults.
    pub fn load(path: &Path) -> io::Result<Config> {
        match fs::read_to_string(path) {
            Ok(text) => Config::parse(&text).map_err(|e| invalid(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        }
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let c: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        c.trust()?;
        c.peers()?;
        c.contacts()?;
        c.locator()?;
        c.radio.check()?;
        c.modem.check()?;
        c.delivery.check()?;
        c.relay.check()?;
        Ok(c)
    }

    /// Write a complete, checked configuration and refuse to replace a file.
    pub fn save_new(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(|e| invalid(format!("configuration: {e}")))?;
        Config::parse(&text).map_err(|e| invalid(format!("configuration: {e}")))?;
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        if let Err(error) = file.write_all(text.as_bytes()) {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(error);
        }
        Ok(())
    }

    /// The trusted stations, checked.
    pub fn trust(&self) -> Result<Trust, String> {
        let mut t = Trust::default();
        for (i, e) in self.trust.iter().enumerate() {
            let at = |m: String| format!("trust entry {} ({}): {m}", i + 1, e.station);
            let call = Callsign::parse(&e.station).map_err(|x| at(x.to_string()))?;
            let key = hex::decode_32(&e.key).map_err(at)?;
            t.insert(call, PublicKey(key));
        }
        Ok(t)
    }

    /// The station's grid locator, checked.
    pub fn locator(&self) -> Result<Option<Locator>, String> {
        self.station
            .locator
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .map(|l| Locator::parse(l).map_err(|e| format!("station.locator {l:?}: {e}")))
            .transpose()
    }

    /// The internet peers, callsigns checked (addresses are looked up when dialled).
    pub fn peers(&self) -> Result<Vec<(Callsign, String)>, String> {
        self.internet
            .peers
            .iter()
            .map(|p| {
                let call =
                    Callsign::parse(&p.station).map_err(|e| format!("internet peer {}: {e}", p.station))?;
                if !p.address.contains(':') {
                    return Err(format!(
                        "internet peer {}: address {:?} needs a port",
                        p.station, p.address
                    ));
                }
                Ok((call, p.address.clone()))
            })
            .collect()
    }

    pub fn contacts(&self) -> Result<Vec<ScheduledContact>, String> {
        self.contact
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let at = |message: String| format!("contact entry {}: {message}", index + 1);
                let from = Callsign::parse(&entry.from).map_err(|error| at(error.to_string()))?;
                let to = Callsign::parse(&entry.to).map_err(|error| at(error.to_string()))?;
                let bearer = match entry.bearer.as_str() {
                    "radio" => RouteBearer::Radio,
                    "internet" => RouteBearer::Internet,
                    "modem" => RouteBearer::Modem,
                    other => return Err(at(format!("unknown bearer {other:?}"))),
                };
                let success_permyriad = entry
                    .success
                    .map(|success| {
                        if !success.is_finite() || !(0.0..=1.0).contains(&success) {
                            return Err(at("success must be between 0 and 1".into()));
                        }
                        Ok((success * 10_000.0).round() as u16)
                    })
                    .transpose()?;
                if from == to || entry.start >= entry.end || entry.rate_bps == 0 || entry.capacity_bytes == 0
                {
                    return Err(at("invalid endpoints, window, rate, or capacity".into()));
                }
                Ok(ScheduledContact {
                    from,
                    to,
                    bearer,
                    start: entry.start,
                    end: entry.end,
                    rate_bps: entry.rate_bps,
                    capacity_bytes: entry.capacity_bytes,
                    success_permyriad,
                    flags: entry.flags,
                })
            })
            .collect()
    }

    pub fn http(&self) -> Result<SocketAddr, String> {
        self.station
            .http
            .parse()
            .map_err(|e| format!("station.http {:?}: {e}", self.station.http))
    }

    pub fn listen(&self) -> Result<Option<SocketAddr>, String> {
        self.internet
            .listen
            .as_deref()
            .map(|l| l.parse().map_err(|e| format!("internet.listen {l:?}: {e}")))
            .transpose()
    }

    /// Make relative paths relative to `dir`, the config file's directory.
    pub fn resolve_paths(&mut self, dir: &Path) {
        for p in [&mut self.station.key, &mut self.station.store] {
            if p.is_relative() {
                *p = dir.join(&*p);
            }
        }
    }
}

/// The directory relative paths in `path` are taken from.
pub fn dir_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests;
