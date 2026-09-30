//! The contact plan: which links exist, and the windows in which some are
//! stated or seen to carry bytes.
//!
//! The graph holds what is known as facts and claims. Contacts are windows:
//! schedules configured by operators, sessions up now (an internet link),
//! links that could be tried now (a modem can call anyone), and contacts
//! other stations advertise. Known links are the topology the station has
//! seen for itself, in beacons and sessions: when one of them will next be
//! open is not a window but a forecast, which the planner asks of the
//! station's beliefs (`hm_model::Beliefs`, through [`hm_model::Estimate`]),
//! as it asks how likely every contact is to work.

use std::collections::BTreeMap;

use hm_wire::{Callsign, ContactAdvert};

use crate::contact::validate_fields;
use crate::{
    BeaconObservation, Bearer, Contact, ContactKey, ContactSource, KnownLink, LiveContact, ScheduledContact,
};

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GraphConfig {
    /// How long a session seen up (or a link that could be tried) stays in
    /// the plan after it was last seen, and how long a station's flags are
    /// believed after its last beacon.
    pub live_contact_secs: u64,
    /// Longest an advertised contact is kept after it was received.
    pub advert_max_age_secs: u64,
    /// A link not seen for this long is forgotten.
    pub link_memory_secs: u64,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            live_contact_secs: 20 * 60,
            advert_max_age_secs: 24 * 3600,
            link_memory_secs: 14 * 24 * 3600,
        }
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
    /// Contacts by the station they leave from.
    contacts: BTreeMap<Callsign, BTreeMap<ContactKey, Contact>>,
    /// Known links by the station they leave from, then by where they go.
    links: BTreeMap<Callsign, BTreeMap<(Callsign, Bearer), KnownLink>>,
    latest_sequence: BTreeMap<ContactKey, u32>,
    node_flags: BTreeMap<Callsign, (u8, u64)>,
}

impl ContactGraph {
    pub fn new(config: GraphConfig) -> Result<Self, GraphError> {
        if config.live_contact_secs == 0 || config.advert_max_age_secs == 0 || config.link_memory_secs == 0 {
            return Err(GraphError::InvalidContact("invalid graph configuration"));
        }
        Ok(Self {
            config,
            contacts: BTreeMap::new(),
            links: BTreeMap::new(),
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
        self.insert(Contact {
            key,
            start: scheduled.start,
            end: scheduled.end,
            rate_bps: scheduled.rate_bps,
            capacity_bytes: scheduled.capacity_bytes,
            reserved_bytes: 0,
            success_permyriad: scheduled.success_permyriad,
            flags: scheduled.flags,
            fresh_until: scheduled.end,
            sequence: None,
            source: ContactSource::Schedule,
        });
        Ok(key)
    }

    /// A session up now (an internet link): a contact while it lasts, and a
    /// link known to exist.
    pub fn observe_live_link(&mut self, live: LiveContact) -> Result<ContactKey, GraphError> {
        let key = self.put_live(live, ContactSource::LiveLink)?;
        self.know(live.from, live.to, live.bearer, live);
        Ok(key)
    }

    /// A link that could be tried now though it was never seen open.
    pub fn add_potential(&mut self, live: LiveContact) -> Result<ContactKey, GraphError> {
        self.put_live(live, ContactSource::Potential)
    }

    /// A beacon shows radio paths open: from its origin to us when it was
    /// heard, and from each station it lists to its origin when that station
    /// was last heard there. A radio path open one way is open both ways
    /// (propagation is reciprocal; how likely a handoff over it is to
    /// complete is the beliefs' business), so each is known as a link in
    /// both directions: without them, a station could not route through a
    /// neighbour to the stations only the neighbour hears.
    pub fn observe_beacon(&mut self, beacon: BeaconObservation<'_>) -> Result<(), GraphError> {
        let seen = |at| LiveContact {
            from: beacon.origin,
            to: beacon.receiver,
            bearer: Bearer::Radio,
            rate_bps: beacon.rate_bps,
            capacity_bytes: beacon.capacity_bytes,
            flags: 0,
            observed_at: at,
        };
        validate_fields(
            beacon.origin,
            beacon.receiver,
            0,
            1,
            beacon.rate_bps,
            beacon.capacity_bytes,
            None,
        )?;
        self.know_path(beacon.origin, beacon.receiver, seen(beacon.observed_at));
        for (station, at) in beacon.hearings() {
            self.know_path(beacon.origin, station, seen(at));
        }
        self.node_flags.insert(
            beacon.origin,
            (
                beacon.flags,
                beacon.observed_at.saturating_add(self.config.live_contact_secs),
            ),
        );
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
        let reserved_bytes = self.contact(key).map_or(0, |contact| {
            contact.reserved_bytes.min(u64::from(advert.capacity_bytes))
        });
        let merge = match self.insert(Contact {
            key,
            start,
            end,
            rate_bps: advert.rate_bps,
            capacity_bytes: u64::from(advert.capacity_bytes),
            reserved_bytes,
            success_permyriad: Some(advert.success_permyriad),
            flags: advert.flags,
            fresh_until: end.min(received_at.saturating_add(self.config.advert_max_age_secs)),
            sequence: Some(advert.sequence),
            source: ContactSource::Advert,
        }) {
            Some(_) => Merge::Updated,
            None => Merge::Inserted,
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

    /// Every contact usable at `now`.
    pub fn contacts(&self, now: u64) -> impl Iterator<Item = &Contact> {
        self.contacts
            .values()
            .flat_map(BTreeMap::values)
            .filter(move |contact| contact.is_usable_at(now))
    }

    /// The contacts leaving `station` usable at `now`.
    pub fn outgoing(&self, station: Callsign, now: u64) -> impl Iterator<Item = &Contact> {
        self.contacts
            .get(&station)
            .into_iter()
            .flat_map(BTreeMap::values)
            .filter(move |contact| contact.is_usable_at(now))
    }

    /// The links known to leave `station`: `(to, bearer, link)`.
    pub fn links_from(&self, station: Callsign) -> impl Iterator<Item = (Callsign, Bearer, &KnownLink)> {
        self.links
            .get(&station)
            .into_iter()
            .flat_map(BTreeMap::iter)
            .map(|((to, bearer), link)| (*to, *bearer, link))
    }

    /// Every known link: `(from, to, bearer, link)`.
    pub fn links(&self) -> impl Iterator<Item = (Callsign, Callsign, Bearer, &KnownLink)> {
        self.links.iter().flat_map(|(from, links)| {
            links
                .iter()
                .map(move |((to, bearer), link)| (*from, *to, *bearer, link))
        })
    }

    pub fn contact(&self, key: ContactKey) -> Option<&Contact> {
        self.contacts.get(&key.from)?.get(&key)
    }

    fn contact_mut(&mut self, key: ContactKey) -> Option<&mut Contact> {
        self.contacts.get_mut(&key.from)?.get_mut(&key)
    }

    /// Hold `bytes` on every contact of `keys` (a contact named twice holds
    /// twice), or on none if any lacks the room.
    pub fn reserve_many(&mut self, keys: &[ContactKey], bytes: u64) -> Result<(), GraphError> {
        let mut totals = BTreeMap::<ContactKey, u64>::new();
        for key in keys {
            let total = totals.entry(*key).or_default();
            *total = total.saturating_add(bytes);
        }
        for (key, total) in &totals {
            let contact = self.contact(*key).ok_or(GraphError::UnknownContact)?;
            if contact.residual_capacity() < *total {
                return Err(GraphError::Capacity);
            }
        }
        for (key, total) in totals {
            self.contact_mut(key).expect("validated contact").reserved_bytes += total;
        }
        Ok(())
    }

    pub fn release(&mut self, key: ContactKey, bytes: u64) -> Result<(), GraphError> {
        let contact = self.contact_mut(key).ok_or(GraphError::UnknownContact)?;
        contact.reserved_bytes = contact.reserved_bytes.saturating_sub(bytes);
        Ok(())
    }

    pub fn release_many(&mut self, keys: &[ContactKey], bytes: u64) {
        for key in keys {
            let _ = self.release(*key, bytes);
        }
    }

    /// `bytes` held on the contact went over it.
    pub fn consume(&mut self, key: ContactKey, bytes: u64) -> Result<(), GraphError> {
        let contact = self.contact_mut(key).ok_or(GraphError::UnknownContact)?;
        if contact.reserved_bytes < bytes || contact.capacity_bytes < bytes {
            return Err(GraphError::Capacity);
        }
        contact.reserved_bytes -= bytes;
        contact.capacity_bytes -= bytes;
        Ok(())
    }

    /// How long a session seen up stays in the plan, and a station's flags
    /// are believed. Follows the beacon interval, which grows on a busy
    /// channel.
    pub fn set_live_contact_secs(&mut self, secs: u64) {
        self.config.live_contact_secs = secs.max(1);
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

    /// Drop contacts that are over, flags no longer believed, and links not
    /// seen for [`GraphConfig::link_memory_secs`].
    pub fn prune(&mut self, now: u64) {
        for contacts in self.contacts.values_mut() {
            contacts.retain(|_, contact| contact.end > now && contact.fresh_until > now);
        }
        self.contacts.retain(|_, contacts| !contacts.is_empty());
        let forget = self.config.link_memory_secs;
        for links in self.links.values_mut() {
            links.retain(|_, link| link.seen_at.saturating_add(forget) > now);
        }
        self.links.retain(|_, links| !links.is_empty());
        self.node_flags.retain(|_, (_, fresh_until)| *fresh_until > now);
    }

    /// Put `contact` in the plan; returns the one it replaced.
    fn insert(&mut self, contact: Contact) -> Option<Contact> {
        self.contacts
            .entry(contact.key.from)
            .or_default()
            .insert(contact.key, contact)
    }

    /// `a` and `b` seen to hear each other over the radio.
    fn know_path(&mut self, a: Callsign, b: Callsign, seen: LiveContact) {
        self.know(a, b, Bearer::Radio, seen);
        self.know(b, a, Bearer::Radio, seen);
    }

    fn know(&mut self, from: Callsign, to: Callsign, bearer: Bearer, seen: LiveContact) {
        if from == to {
            return;
        }
        let link = self
            .links
            .entry(from)
            .or_default()
            .entry((to, bearer))
            .or_insert(KnownLink {
                rate_bps: seen.rate_bps,
                capacity_bytes: seen.capacity_bytes,
                seen_at: seen.observed_at,
            });
        if seen.observed_at >= link.seen_at {
            *link = KnownLink {
                rate_bps: seen.rate_bps,
                capacity_bytes: seen.capacity_bytes,
                seen_at: seen.observed_at,
            };
        }
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
        if let Some(held) = self.contact(key) {
            // An older observation never replaces a newer one, and a link
            // that could be tried never replaces one seen open.
            let weaker = source == ContactSource::Potential && held.source != ContactSource::Potential;
            if held.start > live.observed_at || (weaker && held.end > live.observed_at) {
                return Ok(key);
            }
        }
        let reserved_bytes = self
            .contact(key)
            .map_or(0, |contact| contact.reserved_bytes.min(live.capacity_bytes));
        self.insert(Contact {
            key,
            start: live.observed_at,
            end,
            rate_bps: live.rate_bps,
            capacity_bytes: live.capacity_bytes,
            reserved_bytes,
            success_permyriad: None,
            flags: live.flags,
            fresh_until: end,
            sequence: None,
            source,
        });
        Ok(key)
    }
}

fn newer_serial(candidate: u32, current: u32) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1_u32 << 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_wire::{ContactBearer, Heard};

    fn call(value: &str) -> Callsign {
        value.parse().unwrap()
    }

    fn graph() -> ContactGraph {
        ContactGraph::new(GraphConfig {
            live_contact_secs: 60,
            advert_max_age_secs: 300,
            link_memory_secs: 3_600,
        })
        .unwrap()
    }

    #[test]
    fn a_beacon_makes_each_path_it_attests_known_both_ways() {
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
        fn known(graph: &ContactGraph, from: Callsign) -> Vec<(Callsign, u64)> {
            graph
                .links_from(from)
                .map(|(to, _, link)| (to, link.seen_at))
                .collect()
        }
        assert_eq!(known(&graph, a), vec![(b, 1_000)]);
        assert_eq!(known(&graph, b), vec![(a, 1_000), (c, 880)]);
        assert_eq!(known(&graph, c), vec![(b, 880)]);
        assert_eq!(graph.flags(b, 1_000), Some(3));
        assert_eq!(beacon.hearings().collect::<Vec<_>>(), vec![(a, 1_000), (c, 880)]);
        // A beacon is not a window: when the links open is the beliefs' forecast.
        assert_eq!(graph.contacts(1_000).count(), 0);
        // Links not seen for the link memory are forgotten.
        graph.prune(880 + 3_600);
        assert_eq!(known(&graph, b), vec![(a, 1_000)]);
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
        // Only the session is a known link.
        assert_eq!(graph.links_from(a).count(), 1);
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
        graph.reserve_many(&[key], 80).unwrap();
        assert_eq!(graph.contact(key).unwrap().residual_capacity(), 20);
        assert_eq!(graph.reserve_many(&[key], 21), Err(GraphError::Capacity));
        assert_eq!(graph.reserve_many(&[key, key], 11), Err(GraphError::Capacity));
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
