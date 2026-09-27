use std::collections::BTreeMap;

use hm_wire::{Callsign, ContactAdvert, ContactBearer, Heard};

use crate::beta;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bearer {
    Radio,
    Internet,
}

impl From<ContactBearer> for Bearer {
    fn from(value: ContactBearer) -> Self {
        match value {
            ContactBearer::Radio => Self::Radio,
            ContactBearer::Internet => Self::Internet,
        }
    }
}

impl From<Bearer> for ContactBearer {
    fn from(value: Bearer) -> Self {
        match value {
            Bearer::Radio => Self::Radio,
            Bearer::Internet => Self::Internet,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContactKey {
    pub from: Callsign,
    pub to: Callsign,
    pub bearer: Bearer,
    /// Planned start, or zero for a rolling live contact.
    pub epoch: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContactSource {
    Schedule,
    Beacon,
    LiveLink,
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
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeKey {
    pub from: Callsign,
    pub to: Callsign,
    pub bearer: Bearer,
    pub utc_hour: Option<u8>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Evidence {
    pub successes: f64,
    pub failures: f64,
    pub at: u64,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GraphConfig {
    pub evidence_half_life_secs: u64,
    pub conservative_percentile: f64,
    pub live_contact_secs: u64,
    pub advert_max_age_secs: u64,
    pub prior_success: f64,
    pub prior_failure: f64,
    pub advertised_strength: f64,
    pub bucket_by_utc_hour: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            evidence_half_life_secs: 7 * 24 * 3600,
            conservative_percentile: 0.1,
            live_contact_secs: 20 * 60,
            advert_max_age_secs: 24 * 3600,
            prior_success: 2.0,
            prior_failure: 1.0,
            advertised_strength: 2.0,
            bucket_by_utc_hour: false,
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiveContact {
    pub from: Callsign,
    pub to: Callsign,
    pub bearer: Bearer,
    pub rate_bps: u32,
    pub capacity_bytes: u64,
    pub success_permyriad: Option<u16>,
    pub flags: u8,
    pub observed_at: u64,
}

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
    evidence: BTreeMap<EdgeKey, Evidence>,
    latest_sequence: BTreeMap<ContactKey, u32>,
    node_flags: BTreeMap<Callsign, (u8, u64)>,
}

impl ContactGraph {
    pub fn new(config: GraphConfig) -> Result<Self, GraphError> {
        if config.evidence_half_life_secs == 0
            || config.live_contact_secs == 0
            || config.advert_max_age_secs == 0
            || !(0.0..1.0).contains(&config.conservative_percentile)
            || config.prior_success <= 0.0
            || config.prior_failure <= 0.0
            || config.advertised_strength < 0.0
        {
            return Err(GraphError::InvalidContact("invalid graph configuration"));
        }
        Ok(Self {
            config,
            contacts: BTreeMap::new(),
            evidence: BTreeMap::new(),
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

    pub fn observe_live_link(&mut self, live: LiveContact) -> Result<ContactKey, GraphError> {
        self.put_live(live, ContactSource::LiveLink)?;
        Ok(ContactKey {
            from: live.from,
            to: live.to,
            bearer: live.bearer,
            epoch: 0,
        })
    }

    /// Merge a verified beacon and its signed "heard" observations.
    pub fn observe_beacon(&mut self, beacon: BeaconObservation<'_>) -> Result<(), GraphError> {
        self.put_live(
            LiveContact {
                from: beacon.origin,
                to: beacon.receiver,
                bearer: Bearer::Radio,
                rate_bps: beacon.rate_bps,
                capacity_bytes: beacon.capacity_bytes,
                success_permyriad: Some(8_000),
                flags: beacon.flags,
                observed_at: beacon.observed_at,
            },
            ContactSource::Beacon,
        )?;
        self.record_delivery(
            beacon.origin,
            beacon.receiver,
            Bearer::Radio,
            true,
            beacon.observed_at,
        );
        self.node_flags.insert(
            beacon.origin,
            (
                beacon.flags,
                beacon.observed_at.saturating_add(self.config.live_contact_secs),
            ),
        );
        for observation in beacon.heard {
            if observation.call == beacon.origin {
                continue;
            }
            let age = u64::from(observation.minutes) * 60;
            let observed_at = beacon.observed_at.saturating_sub(age);
            let freshness = 0.5_f64.powf(age as f64 / self.config.live_contact_secs as f64);
            let success_permyriad = (7_000.0 * freshness).round().clamp(1_000.0, 7_000.0) as u16;
            self.put_live(
                LiveContact {
                    from: observation.call,
                    to: beacon.origin,
                    bearer: Bearer::Radio,
                    rate_bps: beacon.rate_bps,
                    capacity_bytes: beacon.capacity_bytes,
                    success_permyriad: Some(success_permyriad),
                    flags: 0,
                    observed_at: beacon.observed_at,
                },
                ContactSource::Beacon,
            )?;
            self.record_delivery(observation.call, beacon.origin, Bearer::Radio, true, observed_at);
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

    pub fn record_delivery(
        &mut self,
        from: Callsign,
        to: Callsign,
        bearer: Bearer,
        success: bool,
        now: u64,
    ) -> (EdgeKey, Evidence) {
        let key = self.edge_key(from, to, bearer, now);
        let faded = self.faded_evidence(key, now);
        let evidence = Evidence {
            successes: faded.successes + f64::from(u8::from(success)),
            failures: faded.failures + f64::from(u8::from(!success)),
            at: now,
        };
        self.evidence.insert(key, evidence);
        (key, evidence)
    }

    pub fn conservative_probability(&self, contact: &Contact, now: u64) -> f64 {
        let at = contact.start.max(now);
        let evidence = self.faded_evidence(
            self.edge_key(contact.key.from, contact.key.to, contact.key.bearer, at),
            now,
        );
        let mut alpha = self.config.prior_success + evidence.successes;
        let mut beta_parameter = self.config.prior_failure + evidence.failures;
        if let Some(permyriad) = contact.success_permyriad {
            let probability = f64::from(permyriad) / 10_000.0;
            alpha += probability * self.config.advertised_strength;
            beta_parameter += (1.0 - probability) * self.config.advertised_strength;
        }
        beta::quantile(alpha, beta_parameter, self.config.conservative_percentile).clamp(1.0e-6, 1.0 - 1.0e-6)
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

    pub fn flags(&self, station: Callsign, now: u64) -> Option<u8> {
        self.node_flags
            .get(&station)
            .filter(|(_, fresh_until)| *fresh_until > now)
            .map(|(flags, _)| *flags)
    }

    pub fn evidence(&self) -> impl Iterator<Item = (EdgeKey, Evidence)> + '_ {
        self.evidence.iter().map(|(key, evidence)| (*key, *evidence))
    }

    pub fn restore_evidence(&mut self, key: EdgeKey, evidence: Evidence) {
        if evidence.successes >= 0.0 && evidence.failures >= 0.0 {
            self.evidence.insert(key, evidence);
        }
    }

    pub fn prune(&mut self, now: u64) {
        self.contacts
            .retain(|_, contact| contact.end > now && contact.fresh_until > now);
        let half_life = self.config.evidence_half_life_secs as f64;
        self.evidence.retain(|_, evidence| {
            let factor = 0.5_f64.powf(now.saturating_sub(evidence.at) as f64 / half_life);
            (evidence.successes + evidence.failures) * factor > 1.0e-6
        });
        self.node_flags.retain(|_, (_, fresh_until)| *fresh_until > now);
    }

    fn put_live(&mut self, live: LiveContact, source: ContactSource) -> Result<(), GraphError> {
        let end = live.observed_at.saturating_add(self.config.live_contact_secs);
        validate_fields(
            live.from,
            live.to,
            live.observed_at,
            end,
            live.rate_bps,
            live.capacity_bytes,
            live.success_permyriad,
        )?;
        let key = ContactKey {
            from: live.from,
            to: live.to,
            bearer: live.bearer,
            epoch: 0,
        };
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
                success_permyriad: live.success_permyriad,
                flags: live.flags,
                fresh_until: end,
                sequence: None,
                source,
            },
        );
        Ok(())
    }

    fn edge_key(&self, from: Callsign, to: Callsign, bearer: Bearer, at: u64) -> EdgeKey {
        EdgeKey {
            from,
            to,
            bearer,
            utc_hour: self.config.bucket_by_utc_hour.then_some(((at / 3600) % 24) as u8),
        }
    }

    fn faded_evidence(&self, key: EdgeKey, now: u64) -> Evidence {
        let Some(evidence) = self.evidence.get(&key).copied() else {
            return Evidence {
                at: now,
                ..Evidence::default()
            };
        };
        let factor =
            0.5_f64.powf(now.saturating_sub(evidence.at) as f64 / self.config.evidence_half_life_secs as f64);
        Evidence {
            successes: evidence.successes * factor,
            failures: evidence.failures * factor,
            at: now,
        }
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
            evidence_half_life_secs: 100,
            live_contact_secs: 60,
            advert_max_age_secs: 300,
            ..GraphConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn delivery_evidence_is_directed_and_decays() {
        let (a, b) = (call("M0AAA"), call("M0BBB"));
        let mut graph = graph();
        let key = graph
            .observe_live_link(LiveContact {
                from: a,
                to: b,
                bearer: Bearer::Radio,
                rate_bps: 1_200,
                capacity_bytes: 10_000,
                success_permyriad: Some(9_000),
                flags: 0,
                observed_at: 0,
            })
            .unwrap();
        let before = graph.conservative_probability(graph.contact(key).unwrap(), 0);
        for now in 1..=10 {
            graph.record_delivery(a, b, Bearer::Radio, true, now);
        }
        let learned = graph.conservative_probability(graph.contact(key).unwrap(), 10);
        assert!(learned > before);
        let faded = graph.conservative_probability(graph.contact(key).unwrap(), 1_010);
        assert!(faded < learned);
        graph.record_delivery(b, a, Bearer::Radio, false, 10);
        assert_eq!(
            graph
                .evidence()
                .filter(|(edge, _)| edge.from == b && edge.to == a)
                .count(),
            1
        );
    }

    #[test]
    fn beacon_builds_observed_directed_edges() {
        let (a, b, c) = (call("M0AAA"), call("M0BBB"), call("M0CCC"));
        let mut graph = graph();
        graph
            .observe_beacon(BeaconObservation {
                origin: b,
                receiver: a,
                heard: &[Heard { call: a, minutes: 1 }, Heard { call: c, minutes: 2 }],
                rate_bps: 1_200,
                capacity_bytes: 4_096,
                flags: 3,
                observed_at: 1_000,
            })
            .unwrap();
        let edges: Vec<_> = graph
            .contacts(1_000)
            .map(|contact| (contact.key.from, contact.key.to))
            .collect();
        assert!(edges.contains(&(b, a)));
        assert!(edges.contains(&(a, b)));
        assert!(edges.contains(&(c, b)));
        assert_eq!(graph.flags(b, 1_000), Some(3));
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
