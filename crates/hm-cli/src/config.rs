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

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hm_ident::PublicKey;
use hm_wire::{Callsign, Locator};
use serde::{Deserialize, Serialize};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use crate::files::Trust;
use crate::hex;
use crate::kiss_link::{KissTarget, TncParams};
use crate::station::LinkTiming;

pub const DEFAULT_PATH: &str = "station.toml";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub station: StationSettings,
    pub radio: RadioSettings,
    pub internet: InternetSettings,
    pub delivery: DeliverySettings,
    pub trust: Vec<TrustEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StationSettings {
    /// Key file, from `hm keygen`.
    pub key: PathBuf,
    /// SSID to run as, when the key file names a callsign without one.
    pub ssid: u8,
    /// Message store; the web access token is kept beside it.
    pub store: PathBuf,
    /// Address of the web page and API.
    pub http: String,
    /// Maidenhead grid locator sent in beacons (`JO89` or `JO89xi`).
    pub locator: Option<String>,
}

impl Default for StationSettings {
    fn default() -> Self {
        StationSettings {
            key: "station.key".into(),
            ssid: 0,
            store: "station.db".into(),
            http: "127.0.0.1:8080".into(),
            locator: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RadioSettings {
    /// false for a node without a radio (an internet server).
    pub enabled: bool,
    /// KISS TNC: `HOST:PORT` (Direwolf), or `serial:DEVICE[:BAUD]`.
    pub kiss: String,
    pub tnc_port: u8,
    /// Use the built-in modem on this sound card instead of a KISS TNC.
    pub audio: Option<String>,
    /// PTT with the built-in modem: `vox`, `rigctld[:HOST:PORT]`, `rts:DEV`, `dtr:DEV`, `cm108:HIDRAW[:GPIO]`.
    pub ptt: String,
    /// CSMA: transmit chance per clear slot is (persist + 1) / 256.
    pub persist: u8,
    pub slottime_ms: u64,
    pub bitrate: u32,
    pub txdelay_ms: u64,
    /// Slack around the predicted end of another station's transmission.
    pub guard_ms: u64,
    /// A signed beacon this often, in minutes; 0 for none.
    pub beacon_minutes: u64,
}

impl Default for RadioSettings {
    fn default() -> Self {
        RadioSettings {
            enabled: true,
            kiss: "127.0.0.1:8001".into(),
            tnc_port: 0,
            audio: None,
            ptt: "vox".into(),
            persist: 63,
            slottime_ms: 100,
            bitrate: 1200,
            txdelay_ms: 300,
            guard_ms: 1500,
            beacon_minutes: 10,
        }
    }
}

impl RadioSettings {
    /// Link timing for the transfer engine.
    pub fn timing(&self) -> LinkTiming {
        LinkTiming {
            bitrate_bps: self.bitrate,
            txdelay_ms: self.txdelay_ms,
            guard_ms: self.guard_ms,
            max_rounds: 12,
        }
    }

    /// Channel-access parameters a hardware TNC is given.
    pub fn tnc_params(&self) -> TncParams {
        TncParams {
            txdelay_ms: self.txdelay_ms as u32,
            persist: self.persist,
            slot_ms: self.slottime_ms as u32,
        }
    }

    /// The settings that take a new radio link to change: all but the beacon interval.
    pub fn link_settings(&self) -> RadioSettings {
        RadioSettings {
            beacon_minutes: 0,
            ..self.clone()
        }
    }

    /// Check what can be checked without opening anything.
    pub fn check(&self) -> Result<(), String> {
        KissTarget::parse(&self.kiss).map_err(|e| format!("radio.kiss: {e}"))?;
        if self.bitrate == 0 {
            return Err("radio.bitrate must be above 0".into());
        }
        if self.tnc_port > 15 {
            return Err("radio.tnc_port is 0 to 15".into());
        }
        if self.ptt.trim().is_empty() {
            return Err("radio.ptt is empty (use \"vox\" for none)".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InternetSettings {
    /// Accept links from trusted stations here, e.g. `0.0.0.0:4433`.
    pub listen: Option<String>,
    /// Stations to keep a link to.
    pub peers: Vec<PeerEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerEntry {
    pub station: String,
    /// `host:port`; looked up again at every dial, so a changed address is followed.
    pub address: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliverySettings {
    /// Relative cost of a delivery attempt by radio and over the internet.
    pub radio_cost: f64,
    pub internet_cost: f64,
    /// Retries: the first after this many seconds, doubling up to `retry_max_secs`.
    pub retry_first_secs: u64,
    pub retry_max_secs: u64,
    /// Give up after this many attempts.
    pub retry_attempts: u32,
}

impl Default for DeliverySettings {
    fn default() -> Self {
        DeliverySettings {
            radio_cost: 1.0,
            internet_cost: 2.0,
            retry_first_secs: 60,
            retry_max_secs: 3600,
            retry_attempts: 12,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustEntry {
    /// With an SSID, that station only; without, every SSID with no entry of its own.
    pub station: String,
    /// Ed25519 public key, 64 hex digits, as `hm whoami` prints it.
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
        c.locator()?;
        c.radio.check()?;
        Ok(c)
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

// ---- editing the file in place ---------------------------------------------

fn read_doc(path: &Path) -> io::Result<DocumentMut> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    // A file of only comments parses as trailing text, which would end up
    // after anything added; keep it at the top instead.
    if doc.as_table().is_empty() {
        let head = doc.trailing().as_str().unwrap_or_default().to_string();
        doc.set_trailing("");
        doc.as_table_mut().decor_mut().set_prefix(head);
    }
    Ok(doc)
}

/// Check the edited document still is a valid config, then replace the file atomically.
fn write_doc(path: &Path, doc: &DocumentMut) -> io::Result<()> {
    let text = doc.to_string();
    Config::parse(&text)
        .map_err(|e| invalid(format!("the change would make {} invalid: {e}", path.display())))?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

fn table<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut Table {
    let item = doc.entry(name).or_insert(Item::Table(Table::new()));
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("a table")
}

/// Set `[section] key = v` in the file, keeping everything else as it is.
pub fn set_value(path: &Path, section: &str, key: &str, v: impl Into<toml_edit::Value>) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let t = table(&mut doc, section);
    let mut new = v.into();
    // Keep the comment written after the old value.
    if let Some(old) = t.get(key).and_then(|i| i.as_value()) {
        *new.decor_mut() = old.decor().clone();
    }
    t[key] = Item::Value(new);
    write_doc(path, &doc)
}

/// Remove `[section] key` from the file, so the default applies.
pub fn unset_value(path: &Path, section: &str, key: &str) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    table(&mut doc, section).remove(key);
    write_doc(path, &doc)
}

/// Trust `key` for exactly `call`: its entry is updated in place (keeping its
/// note unless a new one is given), or a new entry goes at the end.
pub fn set_trust(path: &Path, call: Callsign, key: &PublicKey, note: Option<&str>) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    // The comment at the end of the file introduces the entries: the first
    // one goes below it, not above.
    let mut head = None;
    if doc
        .get("trust")
        .and_then(|t| t.as_array_of_tables())
        .is_none_or(|t| t.is_empty())
    {
        head = Some(doc.trailing().as_str().unwrap_or_default().to_string());
        doc.set_trailing("");
    }
    let entries = trust_entries(&mut doc);
    let station = call.to_string();
    let found = entries
        .iter_mut()
        .find(|t| t.get("station").and_then(|s| s.as_str()) == Some(station.as_str()));
    match found {
        Some(t) => {
            t["key"] = value(hex::encode(&key.0));
            if let Some(n) = note {
                t["note"] = value(n);
            }
        }
        None => {
            let mut t = Table::new();
            t["station"] = value(station);
            t["key"] = value(hex::encode(&key.0));
            if let Some(n) = note {
                t["note"] = value(n);
            }
            if let Some(h) = head {
                t.decor_mut().set_prefix(h);
            }
            entries.push(t);
        }
    }
    write_doc(path, &doc)
}

/// Stop trusting exactly `call`; false if it had no entry.
pub fn remove_trust(path: &Path, call: Callsign) -> io::Result<bool> {
    let mut doc = read_doc(path)?;
    let station = call.to_string();
    let entries = trust_entries(&mut doc);
    let Some(i) = entries
        .iter()
        .position(|t| t.get("station").and_then(|s| s.as_str()) == Some(station.as_str()))
    else {
        return Ok(false);
    };
    // Comments above the entry stay: above the next entry, or at the end.
    let prefix = entries
        .get(i)
        .and_then(|t| t.decor().prefix())
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    entries.remove(i);
    if !prefix.trim().is_empty() {
        let last = entries.is_empty();
        if let Some(next) = entries.get_mut(i) {
            let old = next
                .decor()
                .prefix()
                .and_then(|p| p.as_str())
                .unwrap_or_default()
                .to_string();
            next.decor_mut().set_prefix(format!("{prefix}{old}"));
        } else if last {
            doc.set_trailing(prefix);
        }
    }
    write_doc(path, &doc)?;
    Ok(true)
}

fn trust_entries(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    let item = doc
        .entry("trust")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    if !item.is_array_of_tables() {
        *item = Item::ArrayOfTables(ArrayOfTables::new());
    }
    item.as_array_of_tables_mut().expect("an array of tables")
}

/// Write the `[radio]` settings of `new` that differ from `old` (the
/// beacon interval aside), in one checked edit.
pub fn set_radio(path: &Path, old: &RadioSettings, new: &RadioSettings) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let t = table(&mut doc, "radio");
    let mut put = |key: &str, v: toml_edit::Value| {
        let mut v = v;
        if let Some(o) = t.get(key).and_then(|i| i.as_value()) {
            *v.decor_mut() = o.decor().clone();
        }
        t[key] = Item::Value(v);
    };
    if old.enabled != new.enabled {
        put("enabled", new.enabled.into());
    }
    if old.kiss != new.kiss {
        put("kiss", new.kiss.as_str().into());
    }
    if old.tnc_port != new.tnc_port {
        put("tnc_port", i64::from(new.tnc_port).into());
    }
    if old.ptt != new.ptt {
        put("ptt", new.ptt.as_str().into());
    }
    if old.persist != new.persist {
        put("persist", i64::from(new.persist).into());
    }
    for (key, o, n) in [
        ("slottime_ms", old.slottime_ms, new.slottime_ms),
        ("bitrate", u64::from(old.bitrate), u64::from(new.bitrate)),
        ("txdelay_ms", old.txdelay_ms, new.txdelay_ms),
        ("guard_ms", old.guard_ms, new.guard_ms),
    ] {
        if o != n {
            put(key, (n as i64).into());
        }
    }
    if old.audio != new.audio {
        match &new.audio {
            Some(a) => put("audio", a.as_str().into()),
            None => {
                t.remove("audio");
            }
        }
    }
    write_doc(path, &doc)
}

/// Replace the internet peers.
pub fn set_peers(path: &Path, peers: &[(Callsign, String)]) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let mut list = ArrayOfTables::new();
    for (call, address) in peers {
        let mut t = Table::new();
        t["station"] = value(call.to_string());
        t["address"] = value(address.as_str());
        list.push(t);
    }
    table(&mut doc, "internet")["peers"] = Item::ArrayOfTables(list);
    write_doc(path, &doc)
}

/// A commented starting file for a new station.
pub fn starter(call: Callsign, key: &Path) -> String {
    format!(
        r#"# hm-net station {call}. Every setting has a default; see README.md.
# The web page edits the delivery settings, the internet peers and the
# trusted stations, and keeps these comments.

# ssid = 1                  when the key file names a callsign without an SSID
# store = "station.db"
# http = "127.0.0.1:8080"   the web page; keep it on localhost unless behind HTTPS
# locator = "JO89xi"        your grid square, sent in beacons
[station]
key = {key:?}

# enabled = false           an internet-only node
# kiss = "127.0.0.1:8001"   Direwolf, or "serial:/dev/ttyUSB0:9600" for a hardware TNC
# audio = "default"         the built-in modem on a sound card instead
# ptt = "vox"               or "rts:/dev/ttyUSB0", "cm108:/dev/hidraw0"
[radio]
beacon_minutes = 10         # 0 turns the beacon off

# listen = "0.0.0.0:4433"   accept links from other stations over the internet
# Stations this node dials, one entry each:
#   [[internet.peers]]
#   station = "SO5KM"
#   address = "hm.example.org:4433"
[internet]

# The cheaper way that reaches a station is tried first; a failed delivery
# is retried after first_secs, doubling up to max_secs, attempts times.
[delivery]
radio_cost = 1.0
internet_cost = 2.0
retry_first_secs = 60
retry_max_secs = 3600
retry_attempts = 12

# Stations whose messages you can verify. Add one with
#   hm trust add "SO5KM-1 8a1e…"   (the line `hm whoami` prints on their side)
# or on the web page.
"#,
        key = key.display().to_string()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hm-config-{name}-{}.toml", std::process::id()))
    }

    #[test]
    fn defaults_and_partial_files() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let c = Config::parse("[radio]\nenabled = false\n[delivery]\ninternet_cost = 0.5\n").unwrap();
        assert!(!c.radio.enabled);
        assert_eq!(c.delivery.internet_cost, 0.5);
        assert_eq!(c.delivery.radio_cost, 1.0);
        // A typo is an error, not a silently ignored setting.
        let e = Config::parse("[radio]\nbecaon_minutes = 5\n").unwrap_err();
        assert!(e.contains("becaon_minutes"), "{e}");
        assert!(Config::parse("[[trust]]\nstation = \"SO5KM\"\nkey = \"xyz\"\n").is_err());
        assert!(Config::parse("[[internet.peers]]\nstation = \"SO5KM\"\naddress = \"nohost\"\n").is_err());
    }

    #[test]
    fn starter_file_is_valid_and_mostly_comments() {
        let text = starter(call("SA0KAM-1"), Path::new("station.key"));
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.station.key, PathBuf::from("station.key"));
        assert!(c.trust.is_empty());
    }

    #[test]
    fn edits_keep_comments_and_check_the_result() {
        let path = tmp("edit");
        fs::write(
            &path,
            "# my station\n[radio]\nkiss = \"127.0.0.1:8001\" # direwolf\n\n# friends\n[[trust]]\nstation = \"SO5KM\"\nkey = \"0101010101010101010101010101010101010101010101010101010101010101\"\nnote = \"Jan\"\n",
        )
        .unwrap();
        set_trust(&path, call("SO5KM"), &PublicKey([2; 32]), None).unwrap();
        set_trust(&path, call("SA0KAM-2"), &PublicKey([3; 32]), Some("my server")).unwrap();
        set_value(&path, "radio", "beacon_minutes", 5i64).unwrap();
        set_peers(&path, &[(call("SA0KAM-2"), "server.example.org:4433".into())]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# my station\n[radio]\nkiss = \"127.0.0.1:8001\" # direwolf\n"),
            "{text}"
        );
        assert!(text.contains("# friends"), "{text}");
        let c = Config::load(&path).unwrap();
        assert_eq!(c.radio.beacon_minutes, 5);
        assert_eq!(c.trust[0].note.as_deref(), Some("Jan"), "note kept on update");
        let t = c.trust().unwrap();
        assert_eq!(t.key_for(call("SO5KM-4")), Some(PublicKey([2; 32])));
        assert_eq!(t.key_for(call("SA0KAM-2")), Some(PublicKey([3; 32])));
        assert_eq!(
            c.peers().unwrap(),
            vec![(call("SA0KAM-2"), "server.example.org:4433".to_string())]
        );

        assert!(remove_trust(&path, call("SO5KM")).unwrap());
        assert!(!remove_trust(&path, call("SO5KM")).unwrap());
        unset_value(&path, "radio", "beacon_minutes").unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!((c.trust.len(), c.radio.beacon_minutes), (1, 10));
        // An edit that would leave the file invalid is refused and changes nothing.
        let before = fs::read_to_string(&path).unwrap();
        assert!(set_value(&path, "radio", "persist", "lots").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        fs::remove_file(&path).unwrap();
        // A file that does not exist yet is created by the first edit.
        set_trust(&path, call("SO5KM"), &PublicKey([1; 32]), None).unwrap();
        assert_eq!(Config::load(&path).unwrap().trust.len(), 1);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn edits_to_the_starter_file_land_under_their_comments() {
        let path = tmp("starter");
        fs::write(&path, starter(call("SA0KAM-1"), Path::new("station.key"))).unwrap();
        set_value(&path, "delivery", "internet_cost", 0.5).unwrap();
        set_value(&path, "radio", "beacon_minutes", 5i64).unwrap();
        set_peers(&path, &[(call("SO5KM"), "hub.example.org:4433".into())]).unwrap();
        set_trust(&path, call("SO5KM-1"), &PublicKey([1; 32]), Some("Jan")).unwrap();
        set_trust(&path, call("SP5AAA"), &PublicKey([2; 32]), None).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("beacon_minutes = 5         # 0 turns the beacon off"),
            "{text}"
        );
        let (intro, peer, delivery, trust) = (
            text.find("[internet]").unwrap(),
            text.find("[[internet.peers]]\nstation = \"SO5KM\"").unwrap(),
            text.find("[delivery]").unwrap(),
            text.find("# Stations whose messages").unwrap(),
        );
        assert!(intro < peer && peer < delivery && delivery < trust, "{text}");
        assert!(text.find("station = \"SO5KM-1\"").unwrap() > trust, "{text}");
        assert!(text.find("station = \"SP5AAA\"").unwrap() > trust, "{text}");

        // Removing every entry leaves the comment in place for the next one.
        assert!(remove_trust(&path, call("SO5KM-1")).unwrap());
        assert!(remove_trust(&path, call("SP5AAA")).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Stations whose messages"), "{text}");
        set_trust(&path, call("SP5AAA"), &PublicKey([2; 32]), None).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("# Stations whose messages").count(), 1, "{text}");
        assert!(
            text.find("station = \"SP5AAA\"").unwrap() > text.find("# Stations whose").unwrap(),
            "{text}"
        );
        fs::remove_file(&path).unwrap();
    }
}
