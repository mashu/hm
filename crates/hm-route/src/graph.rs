//! The contact plan: which links exist when, how fast, with how much room.
//!
//! The graph holds only what is known about contacts as facts and claims:
//! schedules configured by operators, links seen live (a beacon heard, an
//! internet or modem session up), links that could be tried (a modem can call
//! anyone), and contacts other stations advertise. How likely each is to work
//! is not the graph's business: that is the station's beliefs
//! (`hm_model::Beliefs`), which the planner asks through
//! [`hm_model::Estimate`].

use std::collections::BTreeMap;

use hm_wire::{Callsign, ContactAdvert, Heard};

pub use hm_model::Bearer;

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
    /// Seen in a beacon, directly or in its list of stations heard.
    Beacon,
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
    pub queue_delay_secs: u32,
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

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GraphConfig {
    /// How long a contact seen live (or one that could be tried) stays in
    /// the plan after it was last seen.
    pub live_contact_secs: u64,
    /// Longest an advertised contact is kept after it was received.
    pub advert_max_age_secs: u64,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            live_contact_secs: 20 * 60,
            advert_max_age_secs: 24 * 3600,
        }
    }
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Merge {
    Inserted,
    Updated,
    Duplicate,
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    InvalidContact(&'static str),
    UnknownContact,
    Capacity,
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidContact(message) => write!(formatter, "invalid contact: {message}"),
            Self::UnknownContact => formatter.write_str("unknown contact"),
            Self::Capacity => formatter.write_str("contact has insufficient residual capacity"),
        }
    }
}

impl std::error::Error for GraphError {}

pub struct ContactGraph {
    config: GraphConfig,
    contacts: BTreeMap<ContactKey, Contact>,
    latest_sequence: BTreeMap<ContactKey, u32>,
    node_flags: BTreeMap<Callsign, (u8, u64)>,
}

impl ContactGraph {
    pub fn new(config: GraphConfig) -> Result<Self, GraphError> {
        if config.live_contact_secs == 0 || config.advert_max_age_secs == 0 {
            return Err(GraphError::InvalidContact("invalid graph configuration"));
        }
        Ok(Self {
            config,
            contacts: BTreeMap::new(),
            latest_sequence: BTreeMap::new(),
            node_flags: BTreeMap::new(),
        })
    }

    pub fn add_schedule(&mut self, scheduled: ScheduledContact) -> Result<ContactKey, GraphError> {
        validate_fields(
            scheduled.from,
            scheduled.to,
            scheduled.start,
            scheduled.end,
            scheduled.rate_bps,
            scheduled.capacity_bytes,
            scheduled.success_permyriad,
        )?;
        let key = ContactKey {
            from: scheduled.from,
            to: scheduled.to,
            bearer: scheduled.bearer,
            epoch: scheduled.start,
        };
        self.contacts.insert(
            key,
            Contact {
                key,
                start: scheduled.start,
                end: scheduled.end,
                rate_bps: scheduled.rate_bps,
                capacity_bytes: scheduled.capacity_bytes,
                reserved_bytes: 0,
                queue_delay_secs: 0,
                success_permyriad: scheduled.success_permyriad,
                flags: scheduled.flags,
                fresh_until: scheduled.end,
                sequence: None,
                source: ContactSource::Schedule,
            },
        );
        Ok(key)
    }

    /// A session up now (an internet link).
    pub fn observe_live_link(&mut self, live: LiveContact) -> Result<ContactKey, GraphError> {
        self.put_live(live, ContactSource::LiveLink)
    }

    /// A link that could be tried now though it was never seen open.
    pub fn add_potential(&mut self, live: LiveContact) -> Result<ContactKey, GraphError> {
        self.put_live(live, ContactSource::Potential)
    }

    /// Contacts shown by a verified beacon: from its origin to the receiver,
    /// and from every station it lists to its origin, at the time it heard it.
    pub fn observe_beacon(&mut self, beacon: BeaconObservation<'_>) -> Result<(), GraphError> {
        self.put_live(
            LiveContact {
                from: beacon.origin,
                to: beacon.receiver,
                bearer: Bearer::Radio,
                rate_bps: beacon.rate_bps,
                capacity_bytes: beacon.capacity_bytes,
                flags: beacon.flags,
                observed_at: beacon.observed_at,
            },
            ContactSource::Beacon,
        )?;
        self.node_flags.insert(
            beacon.origin,
            (
                beacon.flags,
                beacon.observed_at.saturating_add(self.config.live_contact_secs),
            ),
        );
        for (station, at) in beacon.hearings() {
            self.put_live(
                LiveContact {
                    from: station,
                    to: beacon.origin,
                    bearer: Bearer::Radio,
                    rate_bps: beacon.rate_bps,
                    capacity_bytes: beacon.capacity_bytes,
                    flags: 0,
                    observed_at: at,
                },
                ContactSource::Beacon,
            )?;
        }
        Ok(())
    }

    pub fn merge_advert(&mut self, advert: &ContactAdvert, received_at: u64) -> Result<Merge, GraphError> {
        let start = u64::from(advert.start);
        let end = u64::from(advert.end);
        validate_fields(
            advert.origin,
            advert.peer,
            start,
            end,
            advert.rate_bps,
            u64::from(advert.capacity_bytes),
            Some(advert.success_permyriad),
        )?;
        let key = ContactKey {
            from: advert.origin,
            to: advert.peer,
            bearer: advert.bearer.into(),
            epoch: start,
        };
        if let Some(current) = self.latest_sequence.get(&key).copied() {
            if advert.sequence == current {
                return Ok(Merge::Duplicate);
            }
            if !newer_serial(advert.sequence, current) {
                return Ok(Merge::Stale);
            }
        }
        let reserved_bytes = self.contacts.get(&key).map_or(0, |contact| {
            contact.reserved_bytes.min(u64::from(advert.capacity_bytes))
        });
        let contact = Contact {
            key,
            start,
            end,
            rate_bps: advert.rate_bps,
            capacity_bytes: u64::from(advert.capacity_bytes),
            reserved_bytes,
            queue_delay_secs: 0,
            success_permyriad: Some(advert.success_permyriad),
            flags: advert.flags,
            fresh_until: end.min(received_at.saturating_add(self.config.advert_max_age_secs)),
            sequence: Some(advert.sequence),
            source: ContactSource::Advert,
        };
        let merge = if self.contacts.insert(key, contact).is_some() {
            Merge::Updated
        } else {
            Merge::Inserted
        };
        self.latest_sequence.insert(key, advert.sequence);
        self.node_flags.insert(
            advert.origin,
            (
                advert.flags,
                received_at.saturating_add(self.config.advert_max_age_secs),
            ),
        );
        Ok(merge)
    }

    pub fn contacts(&self, now: u64) -> impl Iterator<Item = &Contact> {
        self.contacts
            .values()
            .filter(move |contact| contact.is_usable_at(now))
    }

    pub fn outgoing(&self, station: Callsign, now: u64) -> impl Iterator<Item = &Contact> {
        self.contacts(now)
            .filter(move |contact| contact.key.from == station)
    }

    pub fn contact(&self, key: ContactKey) -> Option<&Contact> {
        self.contacts.get(&key)
    }

    pub fn reserve(&mut self, key: ContactKey, bytes: u64) -> Result<(), GraphError> {
        let contact = self.contacts.get_mut(&key).ok_or(GraphError::UnknownContact)?;
        if contact.residual_capacity() < bytes {
            return Err(GraphError::Capacity);
        }
        contact.reserved_bytes += bytes;
        Ok(())
    }

    pub fn reserve_many(&mut self, keys: &[ContactKey], bytes: u64) -> Result<(), GraphError> {
        let mut totals = BTreeMap::<ContactKey, u64>::new();
        for key in keys {
            let total = totals.entry(*key).or_default();
            *total = total.saturating_add(bytes);
        }
        for (key, total) in &totals {
            let contact = self.contacts.get(key).ok_or(GraphError::UnknownContact)?;
            if contact.residual_capacity() < *total {
                return Err(GraphError::Capacity);
            }
        }
        for (key, total) in totals {
            self.contacts
                .get_mut(&key)
                .expect("validated contact")
                .reserved_bytes += total;
        }
        Ok(())
    }

    pub fn release(&mut self, key: ContactKey, bytes: u64) -> Result<(), GraphError> {
        let contact = self.contacts.get_mut(&key).ok_or(GraphError::UnknownContact)?;
        contact.reserved_bytes = contact.reserved_bytes.saturating_sub(bytes);
        Ok(())
    }

    pub fn release_many(&mut self, keys: &[ContactKey], bytes: u64) {
        for key in keys {
            if let Some(contact) = self.contacts.get_mut(key) {
                contact.reserved_bytes = contact.reserved_bytes.saturating_sub(bytes);
            }
        }
    }

    pub fn consume(&mut self, key: ContactKey, bytes: u64) -> Result<(), GraphError> {
        let contact = self.contacts.get_mut(&key).ok_or(GraphError::UnknownContact)?;
        if contact.reserved_bytes < bytes || contact.capacity_bytes < bytes {
            return Err(GraphError::Capacity);
        }
        contact.reserved_bytes -= bytes;
        contact.capacity_bytes -= bytes;
        Ok(())
    }

    pub fn set_queue_delay(&mut self, key: ContactKey, seconds: u32) -> Result<(), GraphError> {
        let contact = self.contacts.get_mut(&key).ok_or(GraphError::UnknownContact)?;
        contact.queue_delay_secs = seconds;
        Ok(())
    }

    /// How long a live contact seen in a beacon or on a link stays in the
    /// plan. Follows the beacon interval, which grows on a busy channel.
    pub fn set_live_contact_secs(&mut self, secs: u64) {
        self.config.live_contact_secs = secs.max(1);
    }

    pub fn live_contact_secs(&self) -> u64 {
        self.config.live_contact_secs
    }

    /// Stations whose latest beacon or advert carried every bit of `mask`.
    pub fn stations_flagged(&self, mask: u8, now: u64) -> impl Iterator<Item = Callsign> + '_ {
        self.node_flags
            .iter()
            .filter(move |(_, (flags, fresh_until))| *fresh_until > now && flags & mask == mask)
            .map(|(station, _)| *station)
    }

    pub fn flags(&self, station: Callsign, now: u64) -> Option<u8> {
        self.node_flags
            .get(&station)
            .filter(|(_, fresh_until)| *fresh_until > now)
            .map(|(flags, _)| *flags)
    }

    pub fn prune(&mut self, now: u64) {
        self.contacts
            .retain(|_, contact| contact.end > now && contact.fresh_until > now);
        self.node_flags.retain(|_, (_, fresh_until)| *fresh_until > now);
    }

    fn put_live(&mut self, live: LiveContact, source: ContactSource) -> Result<ContactKey, GraphError> {
        let end = live.observed_at.saturating_add(self.config.live_contact_secs);
        validate_fields(
            live.from,
            live.to,
            live.observed_at,
            end,
            live.rate_bps,
            live.capacity_bytes,
            None,
        )?;
        let key = ContactKey {
            from: live.from,
            to: live.to,
            bearer: live.bearer,
            epoch: 0,
        };
        if let Some(held) = self.contacts.get(&key) {
            // An older observation never replaces a newer one, and a link
            // that could be tried never replaces one seen open.
            let weaker = source == ContactSource::Potential && held.source != ContactSource::Potential;
            if held.start > live.observed_at || (weaker && held.end > live.observed_at) {
                return Ok(key);
            }
        }
        let reserved_bytes = self
            .contacts
            .get(&key)
            .map_or(0, |contact| contact.reserved_bytes.min(live.capacity_bytes));
        self.contacts.insert(
            key,
            Contact {
                key,
                start: live.observed_at,
                end,
                rate_bps: live.rate_bps,
                capacity_bytes: live.capacity_bytes,
                reserved_bytes,
                queue_delay_secs: 0,
                success_permyriad: None,
                flags: live.flags,
                fresh_until: end,
                sequence: None,
                source,
            },
        );
        Ok(key)
    }
}

fn validate_fields(
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

fn newer_serial(candidate: u32, current: u32) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1_u32 << 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_wire::ContactBearer;

    fn call(value: &str) -> Callsign {
        value.parse().unwrap()
    }

    fn graph() -> ContactGraph {
        ContactGraph::new(GraphConfig {
            live_contact_secs: 60,
            advert_max_age_secs: 300,
        })
        .unwrap()
    }

    #[test]
    fn beacon_builds_observed_directed_edges() {
        let (a, b, c) = (call("M0AAA"), call("M0BBB"), call("M0CCC"));
        let mut graph = graph();
        let beacon = BeaconObservation {
            origin: b,
            receiver: a,
            heard: &[Heard { call: a, minutes: 0 }, Heard { call: c, minutes: 2 }],
            rate_bps: 1_200,
            capacity_bytes: 4_096,
            flags: 3,
            observed_at: 1_000,
        };
        graph.observe_beacon(beacon).unwrap();
        let edges = |at: u64| -> Vec<(Callsign, Callsign)> {
            graph
                .contacts(at)
                .map(|contact| (contact.key.from, contact.key.to))
                .collect()
        };
        assert!(edges(1_000).contains(&(b, a)));
        assert!(edges(1_000).contains(&(a, b)));
        // B heard C two minutes before its beacon: a live contact from then,
        // over by now with this graph's one-minute window.
        assert!(edges(900).contains(&(c, b)));
        assert!(!edges(1_000).contains(&(c, b)));
        assert_eq!(graph.flags(b, 1_000), Some(3));
        assert_eq!(beacon.hearings().collect::<Vec<_>>(), vec![(a, 1_000), (c, 880)]);
        assert!(graph.contacts(1_000).all(|c| c.stated().is_none()));
    }

    /// A link that could be tried never hides one seen open.
    #[test]
    fn a_potential_link_does_not_replace_a_live_one() {
        let (a, b) = (call("M0AAA"), call("M0BBB"));
        let mut graph = graph();
        let live = |at| LiveContact {
            from: a,
            to: b,
            bearer: Bearer::Radio,
            rate_bps: 1_200,
            capacity_bytes: 1_000,
            flags: 0,
            observed_at: at,
        };
        let key = graph.observe_live_link(live(100)).unwrap();
        graph.add_potential(live(120)).unwrap();
        assert_eq!(graph.contact(key).unwrap().source, ContactSource::LiveLink);
        graph.add_potential(live(200)).unwrap();
        assert_eq!(graph.contact(key).unwrap().source, ContactSource::Potential);
    }

    #[test]
    fn signed_adverts_use_serial_order_and_expire() {
        let mut graph = graph();
        let mut advert = ContactAdvert {
            origin: call("M0AAA"),
            sequence: u32::MAX,
            start: 100,
            end: 500,
            peer: call("M0BBB"),
            bearer: ContactBearer::Radio,
            success_permyriad: 8_000,
            rate_bps: 1_200,
            capacity_bytes: 2_000,
            flags: 1,
            signature: [0; 64],
        };
        assert_eq!(graph.merge_advert(&advert, 100).unwrap(), Merge::Inserted);
        assert_eq!(graph.merge_advert(&advert, 100).unwrap(), Merge::Duplicate);
        let mut other_contact = advert.clone();
        other_contact.peer = call("M0CCC");
        assert_eq!(
            graph.merge_advert(&other_contact, 100).unwrap(),
            Merge::Inserted,
            "sequence numbers are per contact claim, not per origin"
        );
        advert.sequence = 0;
        assert_eq!(graph.merge_advert(&advert, 101).unwrap(), Merge::Updated);
        advert.sequence = u32::MAX - 1;
        assert_eq!(graph.merge_advert(&advert, 102).unwrap(), Merge::Stale);
        graph.prune(401);
        assert_eq!(graph.contacts(401).count(), 0, "advert freshness is bounded");
    }

    #[test]
    fn capacity_reservations_are_atomic_and_bounded() {
        let (a, b) = (call("M0AAA"), call("M0BBB"));
        let mut graph = graph();
        let key = graph
            .add_schedule(ScheduledContact {
                from: a,
                to: b,
                bearer: Bearer::Radio,
                start: 10,
                end: 100,
                rate_bps: 1_200,
                capacity_bytes: 100,
                success_permyriad: None,
                flags: 0,
            })
            .unwrap();
        graph.reserve(key, 80).unwrap();
        assert_eq!(graph.contact(key).unwrap().residual_capacity(), 20);
        assert_eq!(graph.reserve(key, 21), Err(GraphError::Capacity));
        graph.consume(key, 50).unwrap();
        assert_eq!(
            (
                graph.contact(key).unwrap().capacity_bytes,
                graph.contact(key).unwrap().reserved_bytes,
            ),
            (50, 30)
        );
        graph.release(key, 30).unwrap();
        assert_eq!(graph.contact(key).unwrap().residual_capacity(), 50);
    }
}
