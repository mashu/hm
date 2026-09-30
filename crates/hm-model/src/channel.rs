//! A shared radio channel: how many stations contend, how busy is it?
//!
//! Generative story:
//!
//! ```text
//! λ ~ Gamma(shape, rate)            stations active on the channel, drifting
//! N_window ~ Poisson(λ)             distinct stations heard in a window
//! β ~ Beta                          share of time the carrier is busy
//! ```
//!
//! Forgetting (an hour's half-life) lets both follow the day. The contender
//! posterior replaces a raw count of stations heard, which jumps with every
//! beacon, in the decisions that depend on it: how far apart beacons go to
//! keep the control plane in its share of the channel, and the p-persistence
//! of CSMA, whose throughput-optimal value for `N` saturated stations is
//! about `1/N`.

use minicbor::{Decode, Encode};

use crate::evidence::{Beta, Evidence, Prior};

/// Memory of channel activity.
pub const CHANNEL_HALF_LIFE: u64 = 3_600;
/// Gamma prior on the number of other active stations: about two, worth a
/// tenth of a window (so a few minutes of listening outweigh it).
const CONTENDER_PRIOR: (f64, f64) = (0.2, 0.1);
/// Busy share of the carrier before anything is measured, worth a minute.
const BUSY_PRIOR: Prior = Prior::new(QUIET_BUSY, 60_000.0);
/// Share of the time others keep a channel busy, believed before listening.
pub const QUIET_BUSY: f64 = 0.1;

/// Something seen on the channel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChannelObservation {
    /// Distinct other stations heard in the latest window.
    Active { stations: u32 },
    /// Others kept the carrier busy `busy_ms` of the last `total_ms`.
    Occupancy { busy_ms: u64, total_ms: u64 },
}

/// Belief about one channel.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct ChannelModel {
    /// Stations counted (yes) over windows (no), as Gamma-Poisson statistics.
    #[n(0)]
    contenders: Evidence,
    /// Busy (yes) and idle (no) milliseconds.
    #[n(1)]
    busy: Evidence,
}

impl ChannelModel {
    /// `stations` other stations heard within the latest window, looked at
    /// after `exposure` windows since the last look: counts taken more often
    /// than once a window overlap, and count for the share of a window that
    /// is new.
    pub fn observe_active(&mut self, at: u64, stations: u32, exposure: f64) {
        let exposure = exposure.clamp(0.0, 1.0);
        self.contenders
            .add(f64::from(stations) * exposure, exposure, at, CHANNEL_HALF_LIFE);
    }

    pub fn observe(&mut self, at: u64, observation: ChannelObservation) {
        match observation {
            ChannelObservation::Active { stations } => {
                self.contenders
                    .add(f64::from(stations), 1.0, at, CHANNEL_HALF_LIFE)
            }
            ChannelObservation::Occupancy { busy_ms, total_ms } => {
                let busy = busy_ms.min(total_ms);
                self.busy
                    .add(busy as f64, (total_ms - busy) as f64, at, CHANNEL_HALF_LIFE);
            }
        }
    }

    /// Expected number of other stations active on the channel.
    pub fn contenders(&self, now: u64) -> f64 {
        let (count, windows) = self.contenders.faded(now, CHANNEL_HALF_LIFE);
        (CONTENDER_PRIOR.0 + count) / (CONTENDER_PRIOR.1 + windows)
    }

    /// Stations sharing the channel, us included, rounded up: what budgets
    /// are divided by.
    pub fn sharing(&self, now: u64) -> usize {
        libm::ceil(1.0 + self.contenders(now)) as usize
    }

    pub fn occupancy(&self, now: u64) -> Beta {
        self.busy.posterior(BUSY_PRIOR, now, CHANNEL_HALF_LIFE)
    }

    /// Expected wait for a clear channel, in units of the others' typical
    /// transmission `busy_period`: arriving at a random moment the carrier is
    /// busy with chance β, and on a channel busy a share β of the time the
    /// wait for it to clear is about `β / (1 − β)` busy periods.
    pub fn access_wait(&self, now: u64, busy_period: f64) -> f64 {
        access_wait(self.occupancy(now).mean(), busy_period)
    }

    /// Share of the time others keep the carrier busy, as believed at `now`.
    pub fn busy(&self, now: u64) -> f64 {
        self.occupancy(now).mean()
    }

    /// KISS persistence byte for p-persistent CSMA: transmit with probability
    /// `(value + 1) / 256` per clear slot, about `1/N` for `N` stations.
    pub fn persistence(&self, now: u64) -> u8 {
        let p = 1.0 / (1.0 + self.contenders(now));
        (libm::round(p * 256.0) - 1.0).clamp(15.0, 255.0) as u8
    }
}

/// Expected wait for a clear channel that others keep busy a share `busy`
/// of the time, in units of their typical transmission `busy_period`:
/// arriving at a random moment the carrier is busy with chance `busy`, and
/// the wait for it to clear is about `busy / (1 − busy)` busy periods.
pub fn access_wait(busy: f64, busy_period: f64) -> f64 {
    let busy = busy.clamp(0.0, 0.95);
    busy / (1.0 - busy) * busy_period
}

/// Chance, under the contender belief, that a slot is clear of everyone else
/// who transmits with probability `p` per slot: `E[(1-p)^N]` for Poisson `N`.
pub fn clear_slot(contenders: f64, p: f64) -> f64 {
    libm::exp(-contenders * p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contenders_follow_the_counts_and_forget() {
        let mut c = ChannelModel::default();
        for t in 0..30 {
            c.observe(t * 60, ChannelObservation::Active { stations: 9 });
        }
        let now = 30 * 60;
        assert!((c.contenders(now) - 9.0).abs() < 0.5, "{}", c.contenders(now));
        assert_eq!(c.sharing(now), 10);
        // A day later the channel is believed as at first.
        assert!((c.contenders(now + 86_400) - 2.0).abs() < 0.1);
        // Looking every minute at an hour's count counts an hour once an hour.
        let mut looked = ChannelModel::default();
        for t in 0..60 {
            looked.observe_active(t * 60, 9, 1.0 / 60.0);
        }
        assert!(
            (looked.contenders(3_600) - 8.4).abs() < 0.5,
            "{}",
            looked.contenders(3_600)
        );
    }

    #[test]
    fn persistence_falls_as_contenders_grow() {
        let mut quiet = ChannelModel::default();
        let mut busy = ChannelModel::default();
        for t in 0..20 {
            quiet.observe(t, ChannelObservation::Active { stations: 1 });
            busy.observe(t, ChannelObservation::Active { stations: 20 });
        }
        assert!(quiet.persistence(20) > busy.persistence(20));
        assert!(busy.persistence(20) >= 15);
    }

    #[test]
    fn occupancy_is_a_share_of_time() {
        let mut c = ChannelModel::default();
        c.observe(
            0,
            ChannelObservation::Occupancy {
                busy_ms: 1_800_000,
                total_ms: 3_600_000,
            },
        );
        let busy = c.occupancy(0).mean();
        assert!(busy > 0.45 && busy < 0.5, "{busy}");
        assert!(c.access_wait(0, 10.0) > 8.0);
        assert!((clear_slot(0.0, 0.5) - 1.0).abs() < 1e-12);
    }
}
