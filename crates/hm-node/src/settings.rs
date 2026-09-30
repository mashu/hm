//! The settings the node decides with, which may change while it runs.

use hm_model::PerBearer;
use hm_store::RetryPolicy;
use hm_wire::Locator;
use serde::{Deserialize, Serialize};

use crate::Trust;

/// What sending costs, in hundredths of a delivered message's value
/// (`[delivery] *_cost` in `station.toml`): a minute of airtime on the radio
/// and through the ARQ modem, an attempt over the internet.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Costs {
    /// A minute of radio airtime on a quiet channel.
    pub radio: f64,
    /// An attempt over the internet.
    pub internet: f64,
    /// A minute of ARQ modem airtime.
    pub modem: f64,
}

impl Default for Costs {
    fn default() -> Self {
        Costs {
            radio: 5.0,
            internet: 2.0,
            modem: 5.0,
        }
    }
}

impl Costs {
    /// What an attempt costs besides its airtime, in a delivered message's
    /// value.
    pub fn attempt(&self) -> PerBearer<f64> {
        PerBearer([0.0, self.internet / 100.0, 0.0])
    }

    /// What a second of airtime costs, in a delivered message's value, when
    /// others keep the radio channel busy a share `busy` of the time: the
    /// airtime is theirs to want too, so it is dearer the busier they keep
    /// it, by `1 / (1 − busy)`, the factor by which a queue's delay grows
    /// with its load.
    pub fn airtime(&self, busy: f64) -> PerBearer<f64> {
        let congestion = 1.0 / (1.0 - busy.clamp(0.0, 0.95));
        PerBearer([self.radio / 6_000.0 * congestion, 0.0, self.modem / 6_000.0])
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelaySettings {
    /// Accept custody for traffic whose final recipient is another station.
    pub enabled: bool,
    /// Hold traffic until its final recipient contacts this node.
    pub mailbox: bool,
    pub max_holdings: usize,
    pub max_bytes: u64,
    pub max_hops: u8,
    /// Per-bundle radio airtime ceiling.
    pub airtime_budget_secs: u64,
    /// Retired: whether urgent traffic goes two ways at once is decided by
    /// expected utility. Accepted and ignored, so that older station files
    /// still load.
    #[serde(skip_serializing)]
    pub urgent_min_gain: Option<f64>,
    /// Fraction of rolling radio airtime reserved for control.
    pub control_airtime_fraction: f64,
}

impl Default for RelaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mailbox: false,
            max_holdings: 256,
            max_bytes: 16 * 1024 * 1024,
            max_hops: 8,
            airtime_budget_secs: 300,
            urgent_min_gain: None,
            control_airtime_fraction: 0.02,
        }
    }
}

impl RelaySettings {
    pub fn check(&self) -> Result<(), String> {
        if self.max_holdings == 0 || self.max_bytes == 0 {
            return Err("relay max_holdings and max_bytes must be positive".into());
        }
        if !(1..=16).contains(&self.max_hops) {
            return Err("relay.max_hops must be between 1 and 16".into());
        }
        if self.airtime_budget_secs == 0 {
            return Err("relay.airtime_budget_secs must be positive".into());
        }
        if !self.control_airtime_fraction.is_finite() || !(0.0..=1.0).contains(&self.control_airtime_fraction)
        {
            return Err("relay.control_airtime_fraction must be between 0 and 1".into());
        }
        Ok(())
    }
}

/// What the node decides with that may change while it runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub trust: Trust,
    pub costs: Costs,
    pub retry: RetryPolicy,
    pub receipt_retry: RetryPolicy,
    /// A copy is kept this long after custody is handed over.
    pub custody_grace_secs: u64,
    /// Longest wait for an end-to-end receipt before custody is reclaimed.
    pub custody_suspect_secs: u64,
    pub relay: RelaySettings,
    /// Seconds between our beacons; 0 sends none.
    pub beacon_secs: u64,
    /// Rate of the radio link.
    pub radio_bitrate: u32,
    /// Grid locator our beacons carry.
    pub locator: Option<Locator>,
}
