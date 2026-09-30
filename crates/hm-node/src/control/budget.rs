//! How much of a shared radio channel the control plane may take: the
//! beacon interval, the budget the stations on the channel share, and when
//! to pull holdings from a station heard.

use std::collections::VecDeque;

use hm_wire::FLAG_HOLDING;

pub const CONTROL_BUDGET_WINDOW_MS: u64 = 60 * 60 * 1_000;

/// Share of a radio channel, in parts per 10,000, that the control traffic of
/// all the stations sharing it may take together (2%).
pub const CONTROL_BUDGET_PERMYRIAD: u16 = 200;

/// Share of a radio channel all stations' beacons together may take (2%).
pub const BEACON_CHANNEL_PERMYRIAD: u64 = 200;

/// Pull holdings from a station heard on the radio at most this often.
pub const RADIO_PULL_SECS: u64 = 30 * 60;

/// A beacon older than this never counts as a live contact, whatever the
/// beacon interval.
const MIN_LIVE_WINDOW_SECS: u64 = 20 * 60;

/// How often to beacon: the configured interval, or longer while the
/// `stations` sharing the channel (ourselves included) would otherwise spend
/// more than [`BEACON_CHANNEL_PERMYRIAD`] of it on beacons of
/// `beacon_airtime_ms` each. APRS smart beaconing and Meshtastic's interval
/// scaling answer the same problem the same way.
pub fn beacon_interval_ms(configured_ms: u64, stations: usize, beacon_airtime_ms: u64) -> u64 {
    let needed = (stations.max(1) as u64)
        .saturating_mul(beacon_airtime_ms)
        .saturating_mul(10_000)
        / BEACON_CHANNEL_PERMYRIAD;
    configured_ms.max(needed)
}

/// How long a station counts as in reach after its last beacon: its beacons
/// may be as far apart as ours (`beacon_interval_secs`), and one may be lost.
pub fn live_window_secs(beacon_interval_secs: u64) -> u64 {
    beacon_interval_secs
        .saturating_mul(5)
        .div_ceil(2)
        .max(MIN_LIVE_WINDOW_SECS)
}

/// One station's part of the channel's control budget: the `stations`
/// sharing the channel (ourselves included) split `channel_permyriad`
/// evenly, so together they never spend more than that.
pub fn control_share_permyriad(channel_permyriad: u16, stations: usize) -> u16 {
    let share = usize::from(channel_permyriad) / stations.max(1);
    u16::try_from(share.max(1)).unwrap_or(channel_permyriad)
}

/// Whether to pull holdings over the radio from a station whose latest
/// beacon had `flags`: only if it says it holds something, and not more
/// often than [`RADIO_PULL_SECS`]. A station holding mail for us sends it
/// when it hears us anyway; pulling covers bulletins and missed pushes.
pub fn radio_pull_due(flags: u8, last_pull: Option<u64>, now: u64) -> bool {
    flags & FLAG_HOLDING != 0 && last_pull.is_none_or(|at| now.saturating_sub(at) >= RADIO_PULL_SECS)
}

/// Exact sliding-window budget for SYNC radio airtime.
///
/// A frame longer than the whole allowance (a small share on a slow channel
/// shared by many stations) is still sent now and then: once nothing else is
/// in the window and the last send is long enough ago that, averaged over the
/// quiet time since, the share holds.
pub struct ControlBudget {
    window_ms: u64,
    permyriad: u16,
    allowance_ms: u64,
    used_ms: u64,
    transmissions: VecDeque<(u64, u64)>,
    /// After a frame longer than the allowance, nothing until then.
    quiet_until_ms: u64,
}

impl Default for ControlBudget {
    fn default() -> Self {
        Self::new(CONTROL_BUDGET_WINDOW_MS, CONTROL_BUDGET_PERMYRIAD)
            .expect("control-plane constants are valid")
    }
}

impl ControlBudget {
    pub fn new(window_ms: u64, permyriad: u16) -> Result<Self, &'static str> {
        if window_ms == 0 || permyriad > 10_000 {
            return Err("invalid control-airtime budget");
        }
        let allowance_ms = window_ms.saturating_mul(u64::from(permyriad)).div_ceil(10_000);
        Ok(Self {
            window_ms,
            permyriad,
            allowance_ms,
            used_ms: 0,
            transmissions: VecDeque::new(),
            quiet_until_ms: 0,
        })
    }

    /// Change the share of the window this budget allows, keeping what was
    /// already spent in it.
    pub fn set_permyriad(&mut self, permyriad: u16) {
        self.permyriad = permyriad.min(10_000);
        self.allowance_ms = self
            .window_ms
            .saturating_mul(u64::from(self.permyriad))
            .div_ceil(10_000);
    }

    pub fn admit(&mut self, now_ms: u64, airtime_ms: u64) -> bool {
        self.prune(now_ms);
        if now_ms < self.quiet_until_ms {
            return false;
        }
        let oversized = airtime_ms > self.allowance_ms;
        let fits = if oversized {
            // Too long for any window: send it into an empty window, then
            // stay quiet until the share has covered it.
            self.used_ms == 0
        } else {
            airtime_ms <= self.allowance_ms.saturating_sub(self.used_ms)
        };
        if !fits {
            return false;
        }
        if oversized {
            self.quiet_until_ms =
                now_ms.saturating_add(airtime_ms.saturating_mul(10_000) / u64::from(self.permyriad.max(1)));
        }
        self.used_ms = self.used_ms.saturating_add(airtime_ms);
        self.transmissions.push_back((now_ms, airtime_ms));
        true
    }

    #[cfg(test)]
    pub fn used_ms(&mut self, now_ms: u64) -> u64 {
        self.prune(now_ms);
        self.used_ms
    }

    fn prune(&mut self, now_ms: u64) {
        while self
            .transmissions
            .front()
            .is_some_and(|(at, _)| now_ms.saturating_sub(*at) >= self.window_ms)
        {
            if let Some((_, airtime)) = self.transmissions.pop_front() {
                self.used_ms = self.used_ms.saturating_sub(airtime);
            }
        }
    }
}
