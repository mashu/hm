//! The settings the node decides with, which may change while it runs.

use hm_store::RetryPolicy;
use hm_wire::Locator;
use serde::{Deserialize, Serialize};

use crate::Trust;

/// Cost of a delivery attempt on each bearer, in hundredths of a delivered
/// message's value (`[delivery] *_cost` in `station.toml`).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Costs {
    pub radio: f64,
    pub internet: f64,
    pub modem: f64,
}

impl Default for Costs {
    fn default() -> Self {
        Costs {
            radio: 1.0,
            internet: 2.0,
            modem: 1.5,
        }
    }
}

impl Costs {
    /// The costs in a delivered message's value, by [`hm_model::Bearer::index`],
    /// for route choice.
    pub fn attempt_cost(&self) -> [f64; 3] {
        [self.radio, self.internet, self.modem].map(|c| c / 100.0)
    }

    pub fn of(&self, bearer: hm_model::Bearer) -> f64 {
        self.attempt_cost()[bearer.index()]
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
