//! Low-airtime Phase 2 control-plane primitives.
//!
//! CONTACT adverts use Trickle suppression. Holdings reconciliation is
//! pairwise: FILTER (what the receiver already has), OFFER (enumerable ids),
//! then WANT (the missing subset).

use std::collections::{BTreeMap, VecDeque};

use hm_ident::{Identity, PublicKey};
use hm_route::{ContactGraph, Merge};
use hm_store::{Direction, State, Store};
use hm_wire::{
    Callsign, ContactAdvert, ContactBearer, ObjectId, SyncFilter, SyncMessage, SyncOffer, SyncWant,
    MAX_FILTER_BYTES, MAX_OFFER,
};

use crate::files::Trust;

pub const TRICKLE_MIN_MS: u64 = 5_000;
pub const TRICKLE_MAX_MS: u64 = 60 * 60 * 1_000;
pub const TRICKLE_REDUNDANCY: u8 = 2;
pub const CONTROL_BUDGET_WINDOW_MS: u64 = 60 * 60 * 1_000;
pub const CONTROL_BUDGET_PERMYRIAD: u16 = 200;
const FILTER_FALSE_POSITIVE: f64 = 0.01;
const PAIRWISE_STATE_SECS: u64 = 5 * 60;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AdvertKey {
    pub origin: Callsign,
    pub peer: Callsign,
    pub bearer: ContactBearer,
    pub start: u32,
}

impl From<&ContactAdvert> for AdvertKey {
    fn from(advert: &ContactAdvert) -> Self {
        Self {
            origin: advert.origin,
            peer: advert.peer,
            bearer: advert.bearer,
            start: advert.start,
        }
    }
}

#[derive(Clone, Debug)]
struct TrickleEntry {
    advert: ContactAdvert,
    digest: [u8; 32],
    interval_ms: u64,
    interval_started_ms: u64,
    transmit_at_ms: u64,
    consistent: u8,
    considered: bool,
}

/// RFC 6206-style suppression for versioned CONTACT adverts.
pub struct Trickle {
    entries: BTreeMap<AdvertKey, TrickleEntry>,
    min_ms: u64,
    max_ms: u64,
    redundancy: u8,
}

impl Default for Trickle {
    fn default() -> Self {
        Self::new(TRICKLE_MIN_MS, TRICKLE_MAX_MS, TRICKLE_REDUNDANCY)
            .expect("control-plane constants are valid")
    }
}

impl Trickle {
    pub fn new(min_ms: u64, max_ms: u64, redundancy: u8) -> Result<Self, &'static str> {
        if min_ms < 2 || max_ms < min_ms || redundancy == 0 {
            return Err("invalid Trickle parameters");
        }
        Ok(Self {
            entries: BTreeMap::new(),
            min_ms,
            max_ms,
            redundancy,
        })
    }

    /// Observe a verified advert. Returns true only for new content.
    pub fn observe(&mut self, advert: ContactAdvert, now_ms: u64) -> Result<bool, &'static str> {
        let encoded = advert.encode().map_err(|_| "invalid contact advert")?;
        let digest = *blake3::hash(&encoded).as_bytes();
        let key = AdvertKey::from(&advert);
        if let Some(entry) = self.entries.get_mut(&key) {
            if entry.digest == digest {
                entry.consistent = entry.consistent.saturating_add(1);
                return Ok(false);
            }
        }
        let interval_ms = self.min_ms;
        self.entries.insert(
            key,
            TrickleEntry {
                advert,
                digest,
                interval_ms,
                interval_started_ms: now_ms,
                transmit_at_ms: transmit_time(digest, now_ms, interval_ms),
                consistent: 0,
                considered: false,
            },
        );
        Ok(true)
    }

    /// Adverts whose selected transmit time has arrived and were not
    /// suppressed by enough consistent duplicates.
    pub fn poll(&mut self, now_ms: u64) -> Vec<ContactAdvert> {
        let mut due = Vec::new();
        for entry in self.entries.values_mut() {
            advance_interval(entry, now_ms, self.max_ms);
            if !entry.considered && now_ms >= entry.transmit_at_ms {
                entry.considered = true;
                if entry.consistent < self.redundancy {
                    due.push(entry.advert.clone());
                }
            }
        }
        due
    }

    #[cfg(test)]
    pub fn next_deadline(&self) -> Option<u64> {
        self.entries
            .values()
            .filter(|entry| !entry.considered)
            .map(|entry| entry.transmit_at_ms)
            .min()
    }

    pub fn expire(&mut self, unix_now: u64) {
        self.entries
            .retain(|_, entry| u64::from(entry.advert.end) > unix_now);
    }

    pub fn adverts(&self) -> impl Iterator<Item = &ContactAdvert> {
        self.entries.values().map(|entry| &entry.advert)
    }
}

fn transmit_time(digest: [u8; 32], started_ms: u64, interval_ms: u64) -> u64 {
    let half = interval_ms / 2;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hm/trickle/v0");
    hasher.update(&digest);
    hasher.update(&started_ms.to_be_bytes());
    let word = u64::from_be_bytes(hasher.finalize().as_bytes()[..8].try_into().expect("eight bytes"));
    started_ms + half + word % (interval_ms - half)
}

fn advance_interval(entry: &mut TrickleEntry, now_ms: u64, max_ms: u64) {
    while now_ms >= entry.interval_started_ms.saturating_add(entry.interval_ms) {
        entry.interval_started_ms = entry.interval_started_ms.saturating_add(entry.interval_ms);
        entry.interval_ms = entry.interval_ms.saturating_mul(2).min(max_ms);
        entry.transmit_at_ms = transmit_time(entry.digest, entry.interval_started_ms, entry.interval_ms);
        entry.consistent = 0;
        entry.considered = false;
    }
}

/// Exact sliding-window budget for SYNC radio airtime.
pub struct ControlBudget {
    window_ms: u64,
    allowance_ms: u64,
    used_ms: u64,
    transmissions: VecDeque<(u64, u64)>,
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
            allowance_ms,
            used_ms: 0,
            transmissions: VecDeque::new(),
        })
    }

    pub fn admit(&mut self, now_ms: u64, airtime_ms: u64) -> bool {
        self.prune(now_ms);
        if airtime_ms > self.allowance_ms.saturating_sub(self.used_ms) {
            return false;
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

pub fn sign_contact(identity: &Identity, mut advert: ContactAdvert) -> Result<ContactAdvert, &'static str> {
    advert.signature = [0; 64];
    let statement = advert.signing_statement().map_err(|_| "invalid contact advert")?;
    advert.signature = identity.sign(&statement);
    Ok(advert)
}

pub fn verify_contact(advert: &ContactAdvert, key: PublicKey) -> bool {
    advert
        .signing_statement()
        .ok()
        .is_some_and(|statement| key.verify(&statement, &advert.signature).is_ok())
}

/// Build a compact Bloom filter with approximately one-percent false
/// positives, bounded by the wire maximum.
pub fn holdings_filter(scope: u8, salt: u32, ids: &[ObjectId]) -> Result<SyncFilter, &'static str> {
    let count = ids.len().max(1) as f64;
    let ideal_bits = (-count * FILTER_FALSE_POSITIVE.ln() / core::f64::consts::LN_2.powi(2)).ceil() as usize;
    let bytes = ideal_bits.div_ceil(8).clamp(1, MAX_FILTER_BYTES);
    let bits = bytes * 8;
    let hashes = ((bits as f64 / count) * core::f64::consts::LN_2)
        .round()
        .clamp(1.0, 16.0) as u8;
    let mut filter = SyncFilter::new(scope, hashes, salt, bytes).map_err(|_| "invalid holdings filter")?;
    for id in ids {
        filter.insert(id).map_err(|_| "invalid holdings filter")?;
    }
    Ok(filter)
}

pub fn offer_pages(ids: &[ObjectId], filter: &SyncFilter) -> Result<Vec<SyncOffer>, &'static str> {
    let missing = ids
        .iter()
        .filter_map(|id| match filter.contains(id) {
            Ok(false) => Some(Ok(id.prefix8())),
            Ok(true) => None,
            Err(_) => Some(Err("invalid holdings filter")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(missing
        .chunks(MAX_OFFER)
        .map(|prefixes| SyncOffer {
            scope: filter.scope,
            prefixes: prefixes.to_vec(),
        })
        .collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Reply(Vec<u8>),
    Requested { id: ObjectId, peer: Callsign },
}

/// Contact dissemination and pairwise holdings state. Transport remains in
/// the node so the same logic is shared by radio and internet neighbours.
#[derive(Default)]
pub struct ControlPlane {
    trickle: Trickle,
    offered: BTreeMap<(Callsign, [u8; 8]), u64>,
    wanted: BTreeMap<(ObjectId, Callsign), u64>,
}

impl ControlPlane {
    pub fn observe_local(&mut self, advert: ContactAdvert, now_ms: u64) -> Result<(), String> {
        self.trickle
            .observe(advert, now_ms)
            .map(|_| ())
            .map_err(str::to_string)
    }

    pub fn contact_messages(&self) -> Result<Vec<Vec<u8>>, String> {
        self.trickle
            .adverts()
            .map(|advert| {
                SyncMessage::Contact(advert.clone())
                    .encode()
                    .map_err(|error| error.to_string())
            })
            .collect()
    }

    pub fn due_contacts(&mut self, now_ms: u64, unix_now: u64) -> Result<Vec<Vec<u8>>, String> {
        self.trickle.expire(unix_now);
        self.trickle
            .poll(now_ms)
            .into_iter()
            .map(|advert| {
                SyncMessage::Contact(advert)
                    .encode()
                    .map_err(|error| error.to_string())
            })
            .collect()
    }

    pub fn filters(
        &self,
        peer: Callsign,
        store: &Store,
        relayable: bool,
        now: u64,
    ) -> Result<Vec<Vec<u8>>, String> {
        let ids = store.object_ids().map_err(|error| error.to_string())?;
        let salt = sync_salt(peer, now);
        let mut messages = vec![SyncMessage::Filter(holdings_filter(0, salt, &ids)?)
            .encode()
            .map_err(|error| error.to_string())?];
        if relayable {
            messages.push(
                SyncMessage::Filter(holdings_filter(1, salt ^ 0xA5A5_5A5A, &ids)?)
                    .encode()
                    .map_err(|error| error.to_string())?,
            );
        }
        Ok(messages)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn receive(
        &mut self,
        from: Callsign,
        payload: &[u8],
        trust: &Trust,
        // Transport-authenticated key for `from` (QUIC session on an open hub).
        peer_key: Option<PublicKey>,
        local: (Callsign, PublicKey),
        store: &Store,
        graph: &mut ContactGraph,
        accept_relay: bool,
        now: u64,
    ) -> Result<Vec<ControlAction>, String> {
        // RF SYNC still needs a trust entry. Internet open-hub dialers are
        // authenticated by their QUIC certificate (`peer_key`) instead.
        if trust.key_for(from).is_none() && peer_key.is_none() {
            return Err(format!("untrusted SYNC peer {from}"));
        }
        self.prune_pairwise(now);
        match SyncMessage::decode(payload).map_err(|error| error.to_string())? {
            SyncMessage::Contact(advert) => {
                let key = if advert.origin == local.0 {
                    local.1
                } else if let Some(key) = trust.key_for(advert.origin) {
                    key
                } else if advert.origin == from {
                    peer_key.ok_or_else(|| format!("untrusted CONTACT origin {}", advert.origin))?
                } else {
                    return Err(format!("untrusted CONTACT origin {}", advert.origin));
                };
                if !verify_contact(&advert, key) {
                    return Err(format!("bad CONTACT signature from {}", advert.origin));
                }
                match graph
                    .merge_advert(&advert, now)
                    .map_err(|error| error.to_string())?
                {
                    Merge::Inserted | Merge::Updated | Merge::Duplicate => {
                        self.trickle
                            .observe(advert, now.saturating_mul(1_000))
                            .map_err(str::to_string)?;
                    }
                    Merge::Stale => {}
                }
                Ok(vec![])
            }
            SyncMessage::Filter(filter) => {
                let ids = store
                    .holding_ids(from, filter.scope == 1, now)
                    .map_err(|error| error.to_string())?;
                let offers = offer_pages(&ids, &filter)?;
                let expires = now.saturating_add(PAIRWISE_STATE_SECS);
                let mut actions = Vec::with_capacity(offers.len());
                for offer in offers {
                    for prefix in &offer.prefixes {
                        self.offered.insert((from, *prefix), expires);
                    }
                    actions.push(ControlAction::Reply(
                        SyncMessage::Offer(offer)
                            .encode()
                            .map_err(|error| error.to_string())?,
                    ));
                }
                Ok(actions)
            }
            SyncMessage::Offer(offer) => {
                if offer.scope == 1 && !accept_relay {
                    return Ok(vec![]);
                }
                let mut prefixes = Vec::new();
                for prefix in offer.prefixes {
                    if store
                        .object_with_prefix(prefix)
                        .map_err(|error| error.to_string())?
                        .is_none()
                    {
                        prefixes.push(prefix);
                    }
                }
                if prefixes.is_empty() {
                    Ok(vec![])
                } else {
                    Ok(vec![ControlAction::Reply(
                        SyncMessage::Want(SyncWant { prefixes })
                            .encode()
                            .map_err(|error| error.to_string())?,
                    )])
                }
            }
            SyncMessage::Want(want) => {
                let mut actions = Vec::new();
                let bulletin_dest = Callsign::parse("ALL").expect("ALL is a valid callsign");
                for prefix in want.prefixes {
                    if self
                        .offered
                        .get(&(from, prefix))
                        .is_none_or(|expires| *expires <= now)
                    {
                        continue;
                    }
                    let Some((id, _)) = store
                        .object_with_prefix(prefix)
                        .map_err(|error| error.to_string())?
                    else {
                        continue;
                    };
                    let Some(record) = store.record(id).map_err(|error| error.to_string())? else {
                        continue;
                    };
                    let bulletin = record.direction == Direction::Out
                        && record.final_destination() == bulletin_dest
                        && matches!(record.state, State::Queued | State::Delivered);
                    if !bulletin
                        && (record.state != State::Queued
                            || !matches!(record.direction, Direction::Out | Direction::Relay))
                    {
                        continue;
                    }
                    self.wanted
                        .insert((id, from), now.saturating_add(PAIRWISE_STATE_SECS));
                    actions.push(ControlAction::Requested { id, peer: from });
                }
                Ok(actions)
            }
        }
    }

    pub fn target_for(&mut self, id: ObjectId, now: u64) -> Option<Callsign> {
        self.prune_pairwise(now);
        self.wanted
            .iter()
            .find_map(|((wanted, peer), _)| (*wanted == id).then_some(*peer))
    }

    pub fn clear_request(&mut self, id: ObjectId, peer: Callsign) {
        self.wanted.remove(&(id, peer));
    }

    fn prune_pairwise(&mut self, now: u64) {
        self.offered.retain(|_, expires| *expires > now);
        self.wanted.retain(|_, expires| *expires > now);
    }
}

fn sync_salt(peer: Callsign, now: u64) -> u32 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hm/sync/salt/v0");
    hasher.update(&peer.to_bytes());
    hasher.update(&(now / PAIRWISE_STATE_SECS).to_be_bytes());
    u32::from_be_bytes(hasher.finalize().as_bytes()[..4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn call(value: &str) -> Callsign {
        value.parse().unwrap()
    }

    struct TempDb(PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hm-control-{name}-{}-{}.db",
                std::process::id(),
                getrandom::u64().unwrap()
            ));
            Self(path)
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn advert(sequence: u32) -> ContactAdvert {
        ContactAdvert {
            origin: call("M0AAA"),
            sequence,
            start: 1_700_000_000,
            end: 2_000_000_000,
            peer: call("M0BBB"),
            bearer: ContactBearer::Radio,
            success_permyriad: 8_000,
            rate_bps: 1_200,
            capacity_bytes: 4_096,
            flags: 3,
            signature: [0; 64],
        }
    }

    #[test]
    fn contacts_are_signed_and_changes_fail_verification() {
        let identity = Identity::from_secret([7; 32]);
        let mut contact = sign_contact(&identity, advert(1)).unwrap();
        assert!(verify_contact(&contact, identity.public()));
        contact.capacity_bytes += 1;
        assert!(!verify_contact(&contact, identity.public()));
    }

    #[test]
    fn trickle_suppresses_duplicates_and_slows_when_stable() {
        let mut trickle = Trickle::new(100, 800, 2).unwrap();
        let contact = advert(1);
        assert!(trickle.observe(contact.clone(), 0).unwrap());
        let first = trickle.next_deadline().unwrap();
        assert!((50..100).contains(&first));
        assert!(!trickle.observe(contact.clone(), first - 1).unwrap());
        assert!(!trickle.observe(contact.clone(), first - 1).unwrap());
        assert!(trickle.poll(first).is_empty(), "two duplicates suppress the send");
        assert!(trickle.poll(100).is_empty());
        let second = trickle.next_deadline().unwrap();
        assert!((200..300).contains(&second));
        assert_eq!(trickle.poll(second), vec![contact]);
        assert!(trickle.observe(advert(2), second + 1).unwrap());
        assert!(trickle.next_deadline().unwrap() < second + 101);
    }

    #[test]
    fn rolling_budget_never_exceeds_two_percent() {
        let mut budget = ControlBudget::new(1_000, 200).unwrap();
        assert!(budget.admit(0, 12));
        assert!(budget.admit(1, 8));
        assert!(!budget.admit(2, 1));
        assert_eq!(budget.used_ms(999), 20);
        assert!(budget.admit(1_000, 12));
        assert_eq!(budget.used_ms(1_000), 20);
        assert!(budget.admit(1_001, 8));
    }

    #[test]
    fn filter_avoids_known_ids_and_offers_missing_ids_in_pages() {
        let ids: Vec<ObjectId> = (0..20)
            .map(|value| ObjectId(*blake3::hash(&[value]).as_bytes()))
            .collect();
        let filter = holdings_filter(0, 7, &ids[..2]).unwrap();
        let offers = offer_pages(&ids, &filter).unwrap();
        assert!(offers.len() <= 2);
        assert!(offers.iter().all(|offer| offer.prefixes.len() <= MAX_OFFER));
        assert!(!offers
            .iter()
            .flat_map(|offer| &offer.prefixes)
            .any(|prefix| ids[..2].iter().any(|id| id.prefix8() == *prefix)));
    }

    #[test]
    fn pairwise_filter_offer_want_requests_only_an_offered_holding() {
        let (a, b) = (call("M0AAA"), call("M0BBB"));
        let (a_key, b_key) = (Identity::from_secret([1; 32]), Identity::from_secret([2; 32]));
        let mut a_trust = Trust::default();
        a_trust.insert(b, b_key.public());
        let mut b_trust = Trust::default();
        b_trust.insert(a, a_key.public());
        let (a_db, b_db) = (TempDb::new("a"), TempDb::new("b"));
        let (a_store, b_store) = (Store::open(&a_db.0).unwrap(), Store::open(&b_db.0).unwrap());
        let id = ObjectId(*blake3::hash(b"mail for B").as_bytes());
        a_store.enqueue(id, b"mail for B", b, 0, 10).unwrap();
        let mut a_plane = ControlPlane::default();
        let mut b_plane = ControlPlane::default();
        let mut a_graph = ContactGraph::new(Default::default()).unwrap();
        let mut b_graph = ContactGraph::new(Default::default()).unwrap();

        let filter = b_plane.filters(a, &b_store, false, 20).unwrap().remove(0);
        let offer = match a_plane
            .receive(
                b,
                &filter,
                &a_trust,
                None,
                (a, a_key.public()),
                &a_store,
                &mut a_graph,
                false,
                20,
            )
            .unwrap()
            .remove(0)
        {
            ControlAction::Reply(payload) => payload,
            action => panic!("unexpected action: {action:?}"),
        };
        let want = match b_plane
            .receive(
                a,
                &offer,
                &b_trust,
                None,
                (b, b_key.public()),
                &b_store,
                &mut b_graph,
                false,
                20,
            )
            .unwrap()
            .remove(0)
        {
            ControlAction::Reply(payload) => payload,
            action => panic!("unexpected action: {action:?}"),
        };
        assert_eq!(
            a_plane
                .receive(
                    b,
                    &want,
                    &a_trust,
                    None,
                    (a, a_key.public()),
                    &a_store,
                    &mut a_graph,
                    false,
                    20,
                )
                .unwrap(),
            vec![ControlAction::Requested { id, peer: b }]
        );
        assert_eq!(a_plane.target_for(id, 20), Some(b));
    }

    #[test]
    fn open_hub_accepts_sync_from_transport_authenticated_peer() {
        let (hub, home) = (call("SA0KAM-0"), call("SA0KAM-1"));
        let home_key = Identity::from_secret([3; 32]);
        let empty = Trust::default();
        let db = TempDb::new("hub-open");
        let store = Store::open(&db.0).unwrap();
        let mut plane = ControlPlane::default();
        let mut graph = ContactGraph::new(Default::default()).unwrap();
        let filter = ControlPlane::default()
            .filters(hub, &store, false, 20)
            .unwrap()
            .remove(0);
        assert!(plane
            .receive(
                home,
                &filter,
                &empty,
                None,
                (hub, Identity::from_secret([1; 32]).public()),
                &store,
                &mut graph,
                true,
                20,
            )
            .unwrap_err()
            .contains("untrusted SYNC peer"));
        let actions = plane
            .receive(
                home,
                &filter,
                &empty,
                Some(home_key.public()),
                (hub, Identity::from_secret([1; 32]).public()),
                &store,
                &mut graph,
                true,
                20,
            )
            .unwrap();
        // Empty store → no offers; admission itself is the assertion.
        assert!(actions.is_empty() || actions.iter().any(|a| matches!(a, ControlAction::Reply(_))));
    }
}
