//! The parts of `station.toml` a running node applies without a restart:
//! trusted stations, delivery costs and retries, the beacon interval, and the
//! internet peers to keep a link to. They change through the API (and are
//! saved to the file) or by editing the file, which is read again on change.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::SystemTime;

use hm_ident::PublicKey;
use hm_store::RetryPolicy;
use hm_wire::{Callsign, Locator};

use super::choose::Costs;
use crate::config::{self, Config, RadioSettings, RelaySettings};
use crate::files::Trust;

#[derive(Clone, Debug, PartialEq)]
pub struct Live {
    pub trust: Trust,
    /// Notes beside trusted stations, by station.
    pub notes: Vec<(Callsign, String)>,
    pub costs: Costs,
    pub retry: RetryPolicy,
    pub receipt_retry: RetryPolicy,
    pub custody_grace_secs: u64,
    pub custody_suspect_secs: u64,
    pub evidence_half_life_secs: u64,
    pub relay: RelaySettings,
    /// Seconds between beacons; 0 sends none. (Minutes in station.toml.)
    pub beacon_secs: u64,
    pub peers: Vec<(Callsign, String)>,
    /// Grid locator sent in beacons.
    pub locator: Option<Locator>,
    /// `[radio]`: a change opens a new radio link (see `node::RadioBuilder`).
    pub radio: RadioSettings,
}

impl Live {
    pub fn from_config(c: &Config) -> Result<Live, String> {
        let notes = c
            .trust
            .iter()
            .filter_map(|e| Some((Callsign::parse(&e.station).ok()?, e.note.clone()?)))
            .collect();
        Ok(Live {
            trust: c.trust()?,
            notes,
            costs: Costs {
                radio: c.delivery.radio_cost,
                internet: c.delivery.internet_cost,
                modem: c.delivery.modem_cost,
            },
            retry: RetryPolicy {
                first_delay_secs: c.delivery.retry_first_secs,
                max_delay_secs: c.delivery.retry_max_secs,
                max_attempts: c.delivery.retry_attempts,
            },
            receipt_retry: RetryPolicy {
                first_delay_secs: c.delivery.retry_first_secs,
                max_delay_secs: c.delivery.retry_max_secs,
                max_attempts: c.delivery.receipt_retry_attempts,
            },
            custody_grace_secs: c.delivery.custody_grace_secs,
            custody_suspect_secs: c.delivery.custody_suspect_secs,
            evidence_half_life_secs: c.delivery.evidence_half_life_secs,
            relay: c.relay.clone(),
            beacon_secs: c.radio.beacon_minutes.saturating_mul(60),
            peers: c.peers()?,
            locator: c.locator()?,
            radio: c.radio.clone(),
        })
    }

    pub fn note(&self, call: Callsign) -> Option<&str> {
        self.notes
            .iter()
            .find(|(c, _)| *c == call)
            .map(|(_, n)| n.as_str())
    }
}

/// A change to the live settings; `None` leaves a setting as it is.
#[derive(Clone, Debug, Default)]
pub struct Change {
    pub beacon_minutes: Option<u64>,
    pub radio_cost: Option<f64>,
    pub internet_cost: Option<f64>,
    pub modem_cost: Option<f64>,
    pub retry_first_secs: Option<u64>,
    pub retry_max_secs: Option<u64>,
    pub retry_attempts: Option<u32>,
    pub custody_grace_secs: Option<u64>,
    pub custody_suspect_secs: Option<u64>,
    pub receipt_retry_attempts: Option<u32>,
    pub evidence_half_life_secs: Option<u64>,
    pub relay: Option<RelaySettings>,
    pub peers: Option<Vec<(Callsign, String)>>,
    /// `Some(None)` removes the locator.
    pub locator: Option<Option<Locator>>,
    /// The whole `[radio]` section as it should be (beacon interval aside).
    pub radio: Option<RadioSettings>,
}

/// Command-line settings for this run, applied over the file each time it is read.
pub type Overrides = std::sync::Arc<dyn Fn(&mut Config) + Send + Sync>;

pub struct LiveConfig {
    /// The settings in use and a version bumped on every change.
    current: RwLock<(Live, u64)>,
    file: Option<PathBuf>,
    /// The file's modification time and size when last read (the size too,
    /// because some file systems keep times to the second only). Also held
    /// while editing, so two edits cannot lose each other's change.
    seen: Mutex<Option<(SystemTime, u64)>>,
    overrides: Option<Overrides>,
}

fn fingerprint(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn invalid(e: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

impl LiveConfig {
    /// Start from `live`, as read from `file` (if the node has one).
    pub fn new(live: Live, file: Option<PathBuf>) -> LiveConfig {
        let seen = file.as_deref().and_then(fingerprint);
        LiveConfig {
            current: RwLock::new((live, 0)),
            file,
            seen: Mutex::new(seen),
            overrides: None,
        }
    }

    /// Apply `o` over the file every time it is read.
    pub fn with_overrides(mut self, o: Option<Overrides>) -> LiveConfig {
        self.overrides = o;
        self
    }

    pub fn get(&self) -> Live {
        self.current.read().expect("lock").0.clone()
    }

    pub fn version(&self) -> u64 {
        self.current.read().expect("lock").1
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Use `live` from now on; true if it differs from what was in use.
    fn replace(&self, live: Live) -> bool {
        let mut cur = self.current.write().expect("lock");
        if cur.0 == live {
            return false;
        }
        cur.0 = live;
        cur.1 += 1;
        true
    }

    fn read_file(&self, path: &Path) -> Result<Live, String> {
        let mut c = Config::load(path).map_err(|e| e.to_string())?;
        if let Some(o) = &self.overrides {
            o(&mut c);
        }
        Live::from_config(&c).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Read the file again if it changed on disk since last read. `None` when
    /// nothing changed; `Some(Ok(()))` after a reload that changed something;
    /// `Some(Err)` when the file is not valid, and what is in use stays.
    pub fn reload_if_changed(&self) -> Option<Result<(), String>> {
        let path = self.file.as_deref()?;
        let now = fingerprint(path);
        {
            let mut seen = self.seen.lock().expect("lock");
            if now == *seen {
                return None;
            }
            *seen = now;
        }
        match self.read_file(path) {
            Ok(live) => self.replace(live).then_some(Ok(())),
            Err(e) => Some(Err(e)),
        }
    }

    /// Edit the file with `edit` (then take everything from it), or without a
    /// file change the settings in memory with `apply`.
    fn edit(
        &self,
        edit: impl FnOnce(&Path) -> io::Result<()>,
        apply: impl FnOnce(&mut Live),
    ) -> io::Result<()> {
        let mut seen = self.seen.lock().expect("lock");
        match &self.file {
            Some(path) => {
                edit(path)?;
                *seen = fingerprint(path);
                self.replace(self.read_file(path).map_err(invalid)?);
            }
            None => {
                let mut live = self.get();
                apply(&mut live);
                self.replace(live);
            }
        }
        Ok(())
    }

    /// Trust `key` for exactly `call`.
    pub fn add_trust(&self, call: Callsign, key: PublicKey, note: Option<&str>) -> io::Result<()> {
        self.edit(
            |p| config::set_trust(p, call, &key, note),
            |l| {
                l.trust.insert(call, key);
                if let Some(n) = note {
                    l.notes.retain(|(c, _)| *c != call);
                    l.notes.push((call, n.to_string()));
                }
            },
        )
    }

    /// Stop trusting exactly `call`; false if it had no entry of its own.
    pub fn remove_trust(&self, call: Callsign) -> io::Result<bool> {
        if !self.get().trust.iter().any(|(c, _)| c == call) {
            return Ok(false);
        }
        self.edit(
            |p| config::remove_trust(p, call).map(|_| ()),
            |l| {
                l.trust.remove(call);
                l.notes.retain(|(c, _)| *c != call);
            },
        )?;
        Ok(true)
    }

    /// Apply `c`, checking it first; nothing changes if any part is invalid.
    pub fn change(&self, c: Change) -> io::Result<()> {
        let bad = |m: &str| Err(io::Error::new(io::ErrorKind::InvalidInput, m.to_string()));
        for cost in [c.radio_cost, c.internet_cost, c.modem_cost]
            .into_iter()
            .flatten()
        {
            if !(cost.is_finite() && cost > 0.0) {
                return bad("costs must be positive numbers");
            }
        }
        if c.retry_attempts == Some(0) || c.retry_first_secs == Some(0) {
            return bad("retries need at least one attempt and a delay of at least a second");
        }
        if c.receipt_retry_attempts == Some(0) {
            return bad("receipt_retry_attempts must be positive");
        }
        if c.custody_grace_secs == Some(0) || c.custody_suspect_secs == Some(0) {
            return bad("custody_grace_secs and custody_suspect_secs must be positive");
        }
        if c.evidence_half_life_secs == Some(0) {
            return bad("evidence_half_life_secs must be positive");
        }
        if let Some(r) = &c.radio {
            if let Err(e) = r.check() {
                return bad(&e);
            }
        }
        if let Some(r) = &c.relay {
            if let Err(e) = r.check() {
                return bad(&e);
            }
        }
        let radio_now = self.get().radio;
        let relay_now = self.get().relay;
        let edit = |p: &Path| -> io::Result<()> {
            if let Some(v) = c.beacon_minutes {
                config::set_value(p, "radio", "beacon_minutes", v as i64)?;
            }
            for (k, v) in [
                ("radio_cost", c.radio_cost),
                ("internet_cost", c.internet_cost),
                ("modem_cost", c.modem_cost),
            ] {
                if let Some(v) = v {
                    config::set_value(p, "delivery", k, v)?;
                }
            }
            for (k, v) in [
                ("retry_first_secs", c.retry_first_secs),
                ("retry_max_secs", c.retry_max_secs),
                ("retry_attempts", c.retry_attempts.map(u64::from)),
                ("custody_grace_secs", c.custody_grace_secs),
                ("custody_suspect_secs", c.custody_suspect_secs),
                ("receipt_retry_attempts", c.receipt_retry_attempts.map(u64::from)),
                ("evidence_half_life_secs", c.evidence_half_life_secs),
            ] {
                if let Some(v) = v {
                    config::set_value(p, "delivery", k, v as i64)?;
                }
            }
            if let Some(relay) = &c.relay {
                config::set_relay(p, &relay_now, relay)?;
            }
            if let Some(peers) = &c.peers {
                config::set_peers(p, peers)?;
            }
            if let Some(r) = &c.radio {
                config::set_radio(p, &radio_now, r)?;
            }
            match c.locator {
                Some(Some(l)) => config::set_value(p, "station", "locator", l.to_string())?,
                Some(None) => config::unset_value(p, "station", "locator")?,
                None => {}
            }
            Ok(())
        };
        let c2 = c.clone();
        self.edit(edit, move |l| {
            if let Some(v) = c2.beacon_minutes {
                l.beacon_secs = v.saturating_mul(60);
            }
            if let Some(v) = c2.radio_cost {
                l.costs.radio = v;
            }
            if let Some(v) = c2.internet_cost {
                l.costs.internet = v;
            }
            if let Some(v) = c2.modem_cost {
                l.costs.modem = v;
            }
            if let Some(v) = c2.retry_first_secs {
                l.retry.first_delay_secs = v;
                l.receipt_retry.first_delay_secs = v;
            }
            if let Some(v) = c2.retry_max_secs {
                l.retry.max_delay_secs = v;
                l.receipt_retry.max_delay_secs = v;
            }
            if let Some(v) = c2.retry_attempts {
                l.retry.max_attempts = v;
            }
            if let Some(v) = c2.receipt_retry_attempts {
                l.receipt_retry.max_attempts = v;
            }
            if let Some(v) = c2.custody_grace_secs {
                l.custody_grace_secs = v;
            }
            if let Some(v) = c2.custody_suspect_secs {
                l.custody_suspect_secs = v;
            }
            if let Some(v) = c2.evidence_half_life_secs {
                l.evidence_half_life_secs = v;
            }
            if let Some(r) = c2.relay {
                l.relay = r;
            }
            if let Some(p) = c2.peers {
                l.peers = p;
            }
            if let Some(g) = c2.locator {
                l.locator = g;
            }
            if let Some(r) = c2.radio {
                l.radio = RadioSettings {
                    beacon_minutes: l.radio.beacon_minutes,
                    ..r
                };
            }
        })
    }

    /// Load the settings file with CLI overrides applied (for restart-bound fields).
    pub fn file_config(&self) -> Option<Result<Config, String>> {
        let path = self.file.as_deref()?;
        Some((|| {
            let mut c = Config::load(path).map_err(|e| e.to_string())?;
            if let Some(o) = &self.overrides {
                o(&mut c);
            }
            Ok(c)
        })())
    }

    /// Write restart-bound settings to the file only; they take effect on the next start.
    pub fn save_restart(&self, patch: RestartChange) -> io::Result<()> {
        let bad = |m: &str| Err(io::Error::new(io::ErrorKind::InvalidInput, m.to_string()));
        let path = self
            .file
            .as_deref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no settings file to save to"))?;
        if let Some(m) = &patch.modem {
            m.check().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        }
        if let Some(http) = &patch.http {
            if http.trim().is_empty() {
                return bad("station.http must not be empty");
            }
        }
        if let Some(store) = &patch.store {
            if store.trim().is_empty() {
                return bad("station.store must not be empty");
            }
        }
        let mut seen = self.seen.lock().expect("lock");
        let before = Config::load(path).map_err(|e| invalid(e.to_string()))?;
        if let Some(listen) = &patch.internet_listen {
            let trimmed = listen.trim();
            if trimmed.is_empty() {
                config::unset_value(path, "internet", "listen")?;
            } else {
                config::set_value(path, "internet", "listen", trimmed.to_string())?;
            }
        }
        if let Some(v) = patch.open_hub {
            config::set_value(path, "internet", "open_hub", v)?;
        }
        if let Some(m) = &patch.modem {
            config::set_modem(path, &before.modem, m)?;
        }
        if let Some(http) = &patch.http {
            config::set_value(path, "station", "http", http.trim().to_string())?;
        }
        if let Some(store) = &patch.store {
            config::set_value(path, "station", "store", store.trim().to_string())?;
        }
        *seen = fingerprint(path);
        Ok(())
    }
}

/// Restart-bound settings written to the file; applied on the next `hm node` start.
#[derive(Clone, Debug, Default)]
pub struct RestartChange {
    /// `Some("")` clears `internet.listen`.
    pub internet_listen: Option<String>,
    pub open_hub: Option<bool>,
    pub modem: Option<config::ModemSettings>,
    pub http: Option<String>,
    pub store: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    #[test]
    fn edits_are_saved_and_outside_edits_are_picked_up() {
        let path = std::env::temp_dir().join(format!("hm-live-{}.toml", std::process::id()));
        std::fs::write(&path, "# my station\n[radio]\nbeacon_minutes = 10 # every ten\n").unwrap();
        let live = LiveConfig::new(
            Live::from_config(&Config::load(&path).unwrap()).unwrap(),
            Some(path.clone()),
        );
        assert_eq!((live.get().trust.len(), live.version()), (1, 0)); // public hub default

        live.add_trust(call("SO5KM-1"), PublicKey([1; 32]), Some("Jan"))
            .unwrap();
        live.change(Change {
            beacon_minutes: Some(3),
            internet_cost: Some(0.5),
            peers: Some(vec![(call("SO5KM"), "hub.example.org:4433".into())]),
            ..Change::default()
        })
        .unwrap();
        let l = live.get();
        assert_eq!(l.trust.key_for(call("SO5KM-1")), Some(PublicKey([1; 32])));
        assert_eq!(l.note(call("SO5KM-1")), Some("Jan"));
        assert_eq!((l.beacon_secs, l.costs.internet), (180, 0.5));
        assert_eq!(l.peers.len(), 1);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# my station\n[radio]\nbeacon_minutes = 3 # every ten\n"),
            "{text}"
        );
        assert_eq!(live.reload_if_changed(), None, "our own writes are not changes");

        // Refused changes change nothing.
        assert!(live
            .change(Change {
                radio_cost: Some(-1.0),
                ..Change::default()
            })
            .is_err());
        assert!(live
            .change(Change {
                retry_attempts: Some(0),
                ..Change::default()
            })
            .is_err());
        assert_eq!(live.get(), l);

        // Someone edits the file by hand: picked up on the next check.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "\n[[trust]]\nstation = \"SP5AAA\"\nkey = \"{}\"\n",
            "02".repeat(32)
        ));
        std::fs::write(&path, text).unwrap();
        assert_eq!(live.reload_if_changed(), Some(Ok(())));
        // File trusts only (defaults apply when the [[trust]] table is absent).
        assert_eq!(live.get().trust.len(), 2);

        // A broken file is reported and what is in use stays.
        std::fs::write(&path, "[radio]\nbeacon_minutes = \"soon\"\n").unwrap();
        assert!(matches!(live.reload_if_changed(), Some(Err(_))));
        assert_eq!(live.get().trust.len(), 2);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn without_a_file_changes_last_in_memory() {
        let live = LiveConfig::new(Live::from_config(&Config::default()).unwrap(), None);
        live.add_trust(call("SO5KM"), PublicKey([1; 32]), None).unwrap();
        live.change(Change {
            beacon_minutes: Some(0),
            ..Change::default()
        })
        .unwrap();
        assert_eq!(
            (live.get().trust.len(), live.get().beacon_secs, live.version()),
            (2, 0, 2)
        );
        assert!(live.remove_trust(call("SO5KM")).unwrap());
        assert_eq!(live.reload_if_changed(), None);
    }

    #[test]
    fn command_line_overrides_outlast_edits_to_the_file() {
        let path = std::env::temp_dir().join(format!("hm-live-o-{}.toml", std::process::id()));
        std::fs::write(&path, "[radio]\nbeacon_minutes = 10\nkiss = \"127.0.0.1:8001\"\n").unwrap();
        let o: Overrides = std::sync::Arc::new(|c: &mut Config| c.radio.kiss = "127.0.0.1:9001".into());
        let mut c = Config::load(&path).unwrap();
        o(&mut c);
        let live =
            LiveConfig::new(Live::from_config(&c).unwrap(), Some(path.clone())).with_overrides(Some(o));
        live.add_trust(call("SO5KM"), PublicKey([1; 32]), None).unwrap();
        live.change(Change {
            beacon_minutes: Some(5),
            ..Change::default()
        })
        .unwrap();
        let l = live.get();
        assert_eq!((l.radio.kiss.as_str(), l.beacon_secs), ("127.0.0.1:9001", 300));
        // The file keeps its own value.
        assert!(std::fs::read_to_string(&path).unwrap().contains("127.0.0.1:8001"));
        std::fs::remove_file(&path).unwrap();
    }
}
