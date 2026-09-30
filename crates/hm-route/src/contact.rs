//! What the contact plan is made of: contacts (windows in which a link is
//! stated or seen to carry bytes), the links a station has seen exist, and
//! the observations that tell of them.

use hm_wire::{Callsign, Heard};

pub use hm_model::Bearer;

use crate::GraphError;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContactKey {
    pub from: Callsign,
    pub to: Callsign,
    pub bearer: Bearer,
    /// Planned start, or zero for a rolling live contact.
    pub epoch: u64,
}

impl ContactKey {
    /// The link the contact is on.
    pub fn link(&self) -> hm_model::LinkKey {
        hm_model::LinkKey {
            from: self.from,
            to: self.to,
            bearer: self.bearer,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContactSource {
    /// Configured by the operator.
    Schedule,
    /// A session up now (an internet link).
    LiveLink,
    /// A link that could be tried now, never seen open: a modem can call any
    /// station, a radio can reach one it has not heard. Its chance comes from
    /// the beliefs about the link, which start from its bearer's population.
    Potential,
    /// Claimed by another station.
    Advert,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Contact {
    pub key: ContactKey,
    pub start: u64,
    pub end: u64,
    pub rate_bps: u32,
    pub capacity_bytes: u64,
    pub reserved_bytes: u64,
    /// A probability someone stated for the contact (an operator's schedule,
    /// a peer's advert), weighed against the station's own evidence.
    pub success_permyriad: Option<u16>,
    pub flags: u8,
    pub fresh_until: u64,
    pub sequence: Option<u32>,
    pub source: ContactSource,
}

impl Contact {
    pub fn residual_capacity(&self) -> u64 {
        self.capacity_bytes.saturating_sub(self.reserved_bytes)
    }

    pub fn is_usable_at(&self, now: u64) -> bool {
        self.end > now && self.fresh_until > now && self.residual_capacity() > 0
    }

    /// The stated probability, if any.
    pub fn stated(&self) -> Option<f64> {
        self.success_permyriad.map(|p| f64::from(p) / 10_000.0)
    }
}

/// A link the station has seen exist (in a beacon, or as a session): when
/// it will be open is the beliefs' forecast, not a window.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KnownLink {
    pub rate_bps: u32,
    /// Bytes one opening is taken to carry.
    pub capacity_bytes: u64,
    /// When it was last seen.
    pub seen_at: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ScheduledContact {
    pub from: Callsign,
    pub to: Callsign,
    pub bearer: Bearer,
    pub start: u64,
    pub end: u64,
    pub rate_bps: u32,
    pub capacity_bytes: u64,
    pub success_permyriad: Option<u16>,
    pub flags: u8,
}

/// A link seen open (or one that could be tried) at `observed_at`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiveContact {
    pub from: Callsign,
    pub to: Callsign,
    pub bearer: Bearer,
    pub rate_bps: u32,
    pub capacity_bytes: u64,
    pub flags: u8,
    pub observed_at: u64,
}

/// A verified beacon from `origin`, heard by `receiver` at `observed_at`.
#[derive(Copy, Clone, Debug)]
pub struct BeaconObservation<'a> {
    pub origin: Callsign,
    pub receiver: Callsign,
    pub heard: &'a [Heard],
    pub rate_bps: u32,
    pub capacity_bytes: u64,
    pub flags: u8,
    pub observed_at: u64,
}

impl BeaconObservation<'_> {
    /// When `origin` last heard each station it lists: links `(station → origin)`
    /// open at those times.
    pub fn hearings(&self) -> impl Iterator<Item = (Callsign, u64)> + '_ {
        self.heard
            .iter()
            .filter(|h| h.call != self.origin)
            .map(|h| (h.call, self.observed_at.saturating_sub(u64::from(h.minutes) * 60)))
    }
}

pub(crate) fn validate_fields(
    from: Callsign,
    to: Callsign,
    start: u64,
    end: u64,
    rate_bps: u32,
    capacity_bytes: u64,
    success_permyriad: Option<u16>,
) -> Result<(), GraphError> {
    if from == to {
        return Err(GraphError::InvalidContact("self edge"));
    }
    if start >= end {
        return Err(GraphError::InvalidContact("empty time window"));
    }
    if rate_bps == 0 || capacity_bytes == 0 {
        return Err(GraphError::InvalidContact("zero rate or capacity"));
    }
    if success_permyriad.is_some_and(|probability| probability > 10_000) {
        return Err(GraphError::InvalidContact("success probability above one"));
    }
    Ok(())
}
