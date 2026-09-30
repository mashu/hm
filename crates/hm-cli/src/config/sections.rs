//! The sections of `station.toml`, each with its defaults.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::kiss_link::{KissTarget, TncParams};
use crate::station::LinkTiming;

use super::public_hub;

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
    /// Built-in modem: "ax25", "il2p" (Reed–Solomon FEC; NinoTNC and Direwolf
    /// 1.7 decode it) or "auto" (IL2P to stations that say they decode it).
    pub framing: String,
    /// Transfer-engine rounds before giving up a radio handoff.
    pub max_rounds: u32,
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
            framing: "ax25".into(),
            max_rounds: 12,
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
            max_rounds: self.max_rounds.max(1).min(u32::from(u8::MAX)) as u8,
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
        crate::sound_link::Framing::parse(&self.framing).map_err(|e| format!("radio.{e}"))?;
        if self.ptt.trim().is_empty() {
            return Err("radio.ptt is empty (use \"vox\" for none)".into());
        }
        if self.max_rounds == 0 {
            return Err("radio.max_rounds must be positive".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InternetSettings {
    /// Accept links from trusted stations here, e.g. `0.0.0.0:4433`.
    pub listen: Option<String>,
    /// Stations to keep a link to.
    pub peers: Vec<PeerEntry>,
    /// Accept inbound internet links from any station with a valid certificate
    /// (public core hub). Dialling out still requires a trust entry.
    pub open_hub: bool,
}

impl Default for InternetSettings {
    fn default() -> Self {
        InternetSettings {
            listen: None,
            peers: vec![public_hub::peer()],
            open_hub: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerEntry {
    pub station: String,
    /// `host:port`; looked up again at every dial, so a changed address is followed.
    pub address: String,
}

/// An ARQ modem program (VARA, Mercury, ARDOP) as a bearer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModemSettings {
    pub enabled: bool,
    /// "vara" (also for Mercury, which speaks VARA's interface) or "ardop".
    pub kind: String,
    pub host: String,
    /// Command port; data is on the next port. VARA 8300, ARDOP 8515.
    pub port: u16,
    /// Bandwidth to ask for, Hz; 0 leaves the modem's own setting.
    pub bandwidth: u32,
    /// "none" when the modem keys the radio itself; otherwise how to key it
    /// when the modem asks (the same forms as `radio.ptt`).
    pub ptt: String,
}

impl Default for ModemSettings {
    fn default() -> Self {
        ModemSettings {
            enabled: false,
            kind: "vara".into(),
            host: "127.0.0.1".into(),
            port: 8300,
            bandwidth: 0,
            ptt: "none".into(),
        }
    }
}

impl ModemSettings {
    pub fn check(&self) -> Result<(), String> {
        crate::node::arq::Kind::parse(&self.kind).map_err(|e| format!("modem.{e}"))?;
        if self.port == 0 || self.port == u16::MAX {
            return Err("modem.port must be 1 to 65534 (data is on the next port)".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContactEntry {
    pub from: String,
    pub to: String,
    /// `radio`, `internet` or `modem`.
    pub bearer: String,
    /// Unix seconds.
    pub start: u64,
    /// Unix seconds.
    pub end: u64,
    pub rate_bps: u32,
    pub capacity_bytes: u64,
    /// Initial probability hint in `[0, 1]`.
    #[serde(default)]
    pub success: Option<f64>,
    #[serde(default)]
    pub flags: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliverySettings {
    /// What sending costs, in hundredths of a delivered message's value:
    /// what route choice weighs against a route's chance and speed. A minute
    /// of radio airtime on a quiet channel (dearer as others keep it busy),
    /// an attempt over the internet, a minute of ARQ modem airtime.
    pub radio_cost: f64,
    pub internet_cost: f64,
    pub modem_cost: f64,
    /// Retries: the first after this many seconds, doubling up to `retry_max_secs`.
    pub retry_first_secs: u64,
    pub retry_max_secs: u64,
    /// Give up after this many attempts.
    pub retry_attempts: u32,
    /// Retain a shadow copy after hop handoff for holdings pull / reclaim.
    pub custody_grace_secs: u64,
    /// Reclaim in-transit custody when no e2e receipt arrives within this time.
    pub custody_suspect_secs: u64,
    /// Retry budget for destination-signed end-to-end receipts.
    pub receipt_retry_attempts: u32,
    /// Retired: what is learned about links keeps its own memory. Accepted
    /// and ignored, so that older station files still load.
    #[serde(skip_serializing)]
    pub evidence_half_life_secs: Option<u64>,
}

impl Default for DeliverySettings {
    fn default() -> Self {
        let costs = hm_node::Costs::default();
        DeliverySettings {
            radio_cost: costs.radio,
            internet_cost: costs.internet,
            modem_cost: costs.modem,
            retry_first_secs: 60,
            retry_max_secs: 3600,
            retry_attempts: 12,
            custody_grace_secs: 6 * 3600,
            custody_suspect_secs: 24 * 3600,
            receipt_retry_attempts: 24,
            evidence_half_life_secs: None,
        }
    }
}

impl DeliverySettings {
    pub fn check(&self) -> Result<(), String> {
        for (name, cost) in [
            ("radio_cost", self.radio_cost),
            ("internet_cost", self.internet_cost),
            ("modem_cost", self.modem_cost),
        ] {
            if !(cost.is_finite() && cost > 0.0) {
                return Err(format!("delivery.{name} must be a positive number"));
            }
        }
        if self.retry_first_secs == 0 || self.retry_max_secs == 0 || self.retry_attempts == 0 {
            return Err("delivery retries need positive first/max delay and attempts".into());
        }
        if self.retry_max_secs < self.retry_first_secs {
            return Err("delivery.retry_max_secs must be >= retry_first_secs".into());
        }
        if self.custody_grace_secs == 0 || self.custody_suspect_secs == 0 {
            return Err("delivery custody_grace_secs and custody_suspect_secs must be positive".into());
        }
        if self.receipt_retry_attempts == 0 {
            return Err("delivery.receipt_retry_attempts must be positive".into());
        }
        Ok(())
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
