//! Low-airtime Phase 2 control-plane primitives.
//!
//! CONTACT adverts use Trickle suppression. Holdings reconciliation is
//! pairwise: FILTER (what the receiver already has), OFFER (enumerable ids),
//! then WANT (the missing subset).
//!
//! On a shared radio channel the control plane must cost a bounded share of
//! the channel however many stations share it. So:
//!
//! - Beacons are the radio topology: each says who its sender hears, which
//!   gives every listener its one- and two-hop neighbourhood. Their rate falls
//!   as more stations share the channel ([`beacon_interval_ms`]).
//! - Live contacts (who hears whom right now) are advertised to the internet
//!   core only, where gateways need them to route into radio areas. On the
//!   radio they would repeat what the beacons say, once per pair of stations.
//! - What does go on air (scheduled contacts) goes by Trickle, with each
//!   station choosing its own moment in the interval, so a copy heard from
//!   one station silences the others.
//! - The stations heard share one control budget ([`control_share_permyriad`]),
//!   and a SYNC frame waiting for it is replaced by a newer one about the same
//!   thing, or dropped once what it says has expired ([`SyncQueue`]).
//! - Holdings are pulled over the radio only from stations whose beacon says
//!   they hold something ([`radio_pull_due`]).

use std::collections::{BTreeMap, VecDeque};

use hm_ident::{Identity, PublicKey};
use hm_route::{ContactGraph, Merge};
use hm_store::{Direction, State, Store};
use hm_wire::{
    Callsign, ContactAdvert, ContactBearer, Dest, ObjectId, SyncFilter, SyncMessage, SyncOffer, SyncWant,
    FLAG_HOLDING, MAX_FILTER_BYTES, MAX_OFFER,
};

use crate::files::Trust;

pub const TRICKLE_MIN_MS: u64 = 5_000;
pub const TRICKLE_MAX_MS: u64 = 60 * 60 * 1_000;
pub const TRICKLE_REDUNDANCY: u8 = 2;
pub const CONTROL_BUDGET_WINDOW_MS: u64 = 60 * 60 * 1_000;
/// Share of a radio channel, in parts per 10,000, that the control traffic of
/// all the stations sharing it may take together (2%).
pub const CONTROL_BUDGET_PERMYRIAD: u16 = 200;
/// Share of a radio channel all stations' beacons together may take (2%).
pub const BEACON_CHANNEL_PERMYRIAD: u64 = 200;
/// A live contact's advert is valid this long, and refreshed this often:
/// long enough to outlast a Trickle interval (at most an hour) with margin.
pub const LIVE_ADVERT_VALIDITY_SECS: u64 = 2 * 3600;
pub const LIVE_ADVERT_REFRESH_SECS: u64 = 30 * 60;
/// Pull holdings from a station heard on the radio at most this often.
pub const RADIO_PULL_SECS: u64 = 30 * 60;
/// Frames waiting for the radio control budget.
pub const SYNC_QUEUE_FRAMES: usize = 64;
const FILTER_FALSE_POSITIVE: f64 = 0.01;
pub const PAIRWISE_STATE_SECS: u64 = 5 * 60;
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

impl AdvertKey {
    fn slot(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"hm/sync-slot/contact");
        hasher.update(&self.origin.to_bytes());
        hasher.update(&self.peer.to_bytes());
        hasher.update(&[self.bearer as u8]);
        hasher.update(&self.start.to_be_bytes());
        *hasher.finalize().as_bytes()
    }
}

/// Where a CONTACT advert came from, which decides where it may go.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AdvertSource {
    /// Our own claim; `on_air` if it belongs on the radio.
    Own { on_air: bool },
    /// Heard on the radio from another station.
    Radio,
    /// Received over an internet link.
    Internet,
}

/// An advert whose Trickle moment has come, and where to send it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueAdvert {
    pub advert: ContactAdvert,
    pub on_air: bool,
    pub internet: bool,
}

#[derive(Clone, Debug)]
struct TrickleEntry {
    advert: ContactAdvert,
    /// Hash of what the advert says about the contact: not its sequence,
    /// validity or signature, which change on every refresh.
    digest: [u8; 32],
    interval_ms: u64,
    interval_started_ms: u64,
    transmit_at_ms: u64,
    /// Consistent copies heard this interval, on the radio and over the internet.
    heard_on_air: u8,
    heard_on_net: u8,
    considered: bool,
    /// Whether it may go on the radio: heard there, or our own radio claim.
    on_air: bool,
}

/// RFC 6206-style suppression for versioned CONTACT adverts.
///
/// An advert keeps its identity ([`AdvertKey`]) across refreshes. A refresh
/// that says the same thing with a newer sequence number replaces the copy
/// we pass on, but is consistent: it neither resets the interval nor counts
/// as news. Each station picks its moment in an interval from its own `salt`,
/// so the stations that heard an advert at the same instant do not all send
/// it again at the same instant, where they would collide and never hear
/// each other's copy.
pub struct Trickle {
    entries: BTreeMap<AdvertKey, TrickleEntry>,
    min_ms: u64,
    max_ms: u64,
    redundancy: u8,
    salt: u64,
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
            salt: 0,
        })
    }

    /// This station's own randomness for picking moments in an interval.
    pub fn with_salt(mut self, salt: u64) -> Self {
        self.salt = salt;
        self
    }

    /// Observe a verified advert. Returns true only for new content.
    pub fn observe(
        &mut self,
        advert: ContactAdvert,
        now_ms: u64,
        source: AdvertSource,
    ) -> Result<bool, &'static str> {
        let digest = semantic_digest(&advert)?;
        let key = AdvertKey::from(&advert);
        // Internet links are never worth a radio transmission.
        let allowed_on_air = advert.bearer != ContactBearer::Internet;
        let on_air = allowed_on_air
            && match source {
                AdvertSource::Own { on_air } => on_air,
                AdvertSource::Radio => true,
                AdvertSource::Internet => false,
            };
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.on_air |= on_air;
            if entry.digest == digest {
                if newer_serial(advert.sequence, entry.advert.sequence) {
                    entry.advert = advert;
                }
                match source {
                    AdvertSource::Radio => entry.heard_on_air = entry.heard_on_air.saturating_add(1),
                    AdvertSource::Internet => entry.heard_on_net = entry.heard_on_net.saturating_add(1),
                    AdvertSource::Own { .. } => {}
                }
                return Ok(false);
            }
            if !newer_serial(advert.sequence, entry.advert.sequence) {
                return Ok(false);
            }
        }
        let on_air = on_air || self.entries.get(&key).is_some_and(|entry| entry.on_air);
        let interval_ms = self.min_ms;
        self.entries.insert(
            key,
            TrickleEntry {
                advert,
                digest,
                interval_ms,
                interval_started_ms: now_ms,
                transmit_at_ms: transmit_time(self.salt, digest, now_ms, interval_ms),
                heard_on_air: 0,
                heard_on_net: 0,
                considered: false,
                on_air,
            },
        );
        Ok(true)
    }

    /// Adverts whose moment in their interval has arrived, and where each may
    /// still go: nowhere a consistent copy was heard often enough this interval.
    pub fn poll(&mut self, now_ms: u64) -> Vec<DueAdvert> {
        let mut due = Vec::new();
        for entry in self.entries.values_mut() {
            advance_interval(entry, now_ms, self.max_ms, self.salt);
            if !entry.considered && now_ms >= entry.transmit_at_ms {
                entry.considered = true;
                let on_air = entry.on_air && entry.heard_on_air < self.redundancy;
                let internet = entry.heard_on_net < self.redundancy;
                if on_air || internet {
                    due.push(DueAdvert {
                        advert: entry.advert.clone(),
                        on_air,
                        internet,
                    });
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

/// RFC 1982 serial-number order, as the contact graph uses for sequences.
fn newer_serial(candidate: u32, current: u32) -> bool {
    candidate != current && candidate.wrapping_sub(current) < 0x8000_0000
}

/// What an advert says about its contact: everything but the sequence number,
/// the end of its validity and the signature.
fn semantic_digest(advert: &ContactAdvert) -> Result<[u8; 32], &'static str> {
    let encoded = advert.encode().map_err(|_| "invalid contact advert")?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&encoded[1..7]);
    hasher.update(&encoded[11..15]);
    hasher.update(&encoded[19..37]);
    Ok(*hasher.finalize().as_bytes())
}

fn transmit_time(salt: u64, digest: [u8; 32], started_ms: u64, interval_ms: u64) -> u64 {
    let half = interval_ms / 2;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hm/trickle/v1");
    hasher.update(&salt.to_be_bytes());
    hasher.update(&digest);
    hasher.update(&started_ms.to_be_bytes());
    let word = u64::from_be_bytes(hasher.finalize().as_bytes()[..8].try_into().expect("eight bytes"));
    started_ms + half + word % (interval_ms - half)
}

fn advance_interval(entry: &mut TrickleEntry, now_ms: u64, max_ms: u64, salt: u64) {
    while now_ms >= entry.interval_started_ms.saturating_add(entry.interval_ms) {
        entry.interval_started_ms = entry.interval_started_ms.saturating_add(entry.interval_ms);
        entry.interval_ms = entry.interval_ms.saturating_mul(2).min(max_ms);
        entry.transmit_at_ms =
            transmit_time(salt, entry.digest, entry.interval_started_ms, entry.interval_ms);
        entry.heard_on_air = 0;
        entry.heard_on_net = 0;
        entry.considered = false;
    }
}

/// A SYNC frame waiting for the radio's control budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedSync {
    pub to: Dest,
    pub payload: Vec<u8>,
    /// Unix seconds after which what it says is no longer worth sending.
    pub expires_at: u64,
    /// Frames with the same slot say the same kind of thing about the same
    /// subject; a newer one replaces an older one still waiting.
    slot: Option<[u8; 32]>,
}

/// SYNC frames waiting for the radio's control budget. The budget releases a
/// frame every minute or so on a busy channel, so a plain queue would send
/// what was true an hour ago: a newer frame about the same contact, or to
/// the same station with the same kind of filter, takes the older one's
/// place, and a frame whose content has expired is dropped instead of sent.
pub struct SyncQueue {
    items: VecDeque<QueuedSync>,
    capacity: usize,
    dropped: u64,
}

impl SyncQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity,
            dropped: 0,
        }
    }

    /// Queue `payload` for `to`, at `now` (Unix seconds).
    pub fn push(&mut self, to: Dest, payload: Vec<u8>, now: u64) {
        let (slot, expires_at) = match SyncMessage::decode(&payload) {
            Ok(SyncMessage::Contact(advert)) => {
                (Some(AdvertKey::from(&advert).slot()), u64::from(advert.end))
            }
            Ok(SyncMessage::Filter(filter)) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(b"hm/sync-slot/filter");
                match to {
                    Dest::Station(call) => hasher.update(&call.to_bytes()),
                    Dest::Broadcast => hasher.update(&[0xff; 6]),
                };
                hasher.update(&[filter.scope]);
                (
                    Some(*hasher.finalize().as_bytes()),
                    now.saturating_add(PAIRWISE_STATE_SECS),
                )
            }
            // Offers and wants answer one exchange; the peer forgets it after this.
            _ => (None, now.saturating_add(PAIRWISE_STATE_SECS)),
        };
        let item = QueuedSync {
            to,
            payload,
            expires_at,
            slot,
        };
        if let Some(slot) = slot {
            if let Some(waiting) = self.items.iter_mut().find(|waiting| waiting.slot == Some(slot)) {
                *waiting = item;
                return;
            }
        }
        if self.items.len() >= self.capacity {
            self.dropped += 1;
            return;
        }
        self.items.push_back(item);
    }

    /// A SYNC frame heard on the air: a queued copy of the same contact, no
    /// newer than what was heard, need not go out again (Trickle suppression,
    /// carried through to frames already waiting for the budget).
    pub fn heard(&mut self, payload: &[u8]) {
        let Ok(SyncMessage::Contact(heard)) = SyncMessage::decode(payload) else {
            return;
        };
        let slot = AdvertKey::from(&heard).slot();
        let before = self.items.len();
        self.items.retain(|item| {
            item.slot != Some(slot)
                || match SyncMessage::decode(&item.payload) {
                    Ok(SyncMessage::Contact(queued)) => newer_serial(queued.sequence, heard.sequence),
                    _ => true,
                }
        });
        self.dropped += (before - self.items.len()) as u64;
    }

    /// The next frame worth sending at `now`, dropping expired ones on the way.
    pub fn front(&mut self, now: u64) -> Option<&QueuedSync> {
        while self.items.front().is_some_and(|item| item.expires_at <= now) {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.front()
    }

    pub fn pop(&mut self) -> Option<QueuedSync> {
        self.items.pop_front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Frames dropped: the queue was full, or they expired waiting.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
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

/// An advert due for dissemination, encoded, and where it may go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueContact {
    pub payload: Vec<u8>,
    pub on_air: bool,
    pub internet: bool,
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
    /// The control plane of station `me`, whose callsign seeds its Trickle moments.
    pub fn for_station(me: Callsign) -> Self {
        Self {
            trickle: Trickle::default().with_salt(me.packed()),
            ..Self::default()
        }
    }

    /// Our own claim. `on_air`: it belongs on the radio as well as the internet
    /// (a scheduled contact); live contacts do not (see the module notes).
    pub fn observe_local(&mut self, advert: ContactAdvert, now_ms: u64, on_air: bool) -> Result<(), String> {
        self.trickle
            .observe(advert, now_ms, AdvertSource::Own { on_air })
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

    pub fn due_contacts(&mut self, now_ms: u64, unix_now: u64) -> Result<Vec<DueContact>, String> {
        self.trickle.expire(unix_now);
        self.trickle
            .poll(now_ms)
            .into_iter()
            .map(|due| {
                Ok(DueContact {
                    payload: SyncMessage::Contact(due.advert)
                        .encode()
                        .map_err(|error| error.to_string())?,
                    on_air: due.on_air,
                    internet: due.internet,
                })
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
        // Heard on the radio (else over an internet link).
        over_radio: bool,
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
                        let source = if over_radio {
                            AdvertSource::Radio
                        } else {
                            AdvertSource::Internet
                        };
                        self.trickle
                            .observe(advert, now.saturating_mul(1_000), source)
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

    fn due(advert: ContactAdvert, on_air: bool, internet: bool) -> DueAdvert {
        DueAdvert {
            advert,
            on_air,
            internet,
        }
    }

    #[test]
    fn trickle_suppresses_duplicates_and_slows_when_stable() {
        let mut trickle = Trickle::new(100, 800, 2).unwrap();
        let contact = advert(1);
        assert!(trickle.observe(contact.clone(), 0, AdvertSource::Radio).unwrap());
        let first = trickle.next_deadline().unwrap();
        assert!((50..100).contains(&first));
        assert!(!trickle
            .observe(contact.clone(), first - 1, AdvertSource::Radio)
            .unwrap());
        assert!(!trickle
            .observe(contact.clone(), first - 1, AdvertSource::Radio)
            .unwrap());
        assert_eq!(
            trickle.poll(first),
            vec![due(contact.clone(), false, true)],
            "two copies on air silence the radio, not the internet"
        );
        assert!(trickle.poll(100).is_empty());
        let second = trickle.next_deadline().unwrap();
        assert!((200..300).contains(&second));
        assert_eq!(trickle.poll(second), vec![due(contact.clone(), true, true)]);
        let mut changed = advert(2);
        changed.success_permyriad = 5_000;
        assert!(trickle.observe(changed, second + 1, AdvertSource::Radio).unwrap());
        assert!(
            trickle.next_deadline().unwrap() < second + 101,
            "new content starts over"
        );
    }

    #[test]
    fn a_refresh_that_says_the_same_is_passed_on_without_starting_over() {
        let mut trickle = Trickle::new(100, 800, 2).unwrap();
        trickle.observe(advert(1), 0, AdvertSource::Radio).unwrap();
        let first = trickle.next_deadline().unwrap();
        trickle.poll(first);
        trickle.poll(100);
        let second = trickle.next_deadline().unwrap();
        let mut refreshed = advert(9);
        refreshed.end += 3600;
        assert!(!trickle
            .observe(refreshed.clone(), 101, AdvertSource::Own { on_air: true })
            .unwrap());
        assert_eq!(
            trickle.next_deadline(),
            Some(second),
            "the interval keeps growing"
        );
        assert_eq!(
            trickle.poll(second)[0].advert,
            refreshed,
            "the newest copy goes out"
        );
        assert!(!trickle
            .observe(advert(3), second + 1, AdvertSource::Radio)
            .unwrap());
        assert_eq!(
            trickle.adverts().next().unwrap().sequence,
            9,
            "an older copy never replaces it"
        );
    }

    #[test]
    fn stations_that_hear_an_advert_together_pick_different_moments() {
        let moments: std::collections::BTreeSet<u64> = ["M0AAA", "M0BBB", "M0CCC", "M0DDD", "M0EEE"]
            .iter()
            .map(|me| {
                let mut trickle = Trickle::new(5_000, 60_000, 2)
                    .unwrap()
                    .with_salt(call(me).packed());
                trickle.observe(advert(1), 1_000, AdvertSource::Radio).unwrap();
                trickle.next_deadline().unwrap()
            })
            .collect();
        assert_eq!(moments.len(), 5);
    }

    #[test]
    fn only_radio_contacts_heard_on_air_or_claimed_for_it_go_on_air() {
        let mut trickle = Trickle::new(100, 800, 2).unwrap();
        let mut from_internet = advert(1);
        from_internet.peer = call("M0CCC");
        let mut internet_link = advert(1);
        internet_link.peer = call("M0DDD");
        internet_link.bearer = ContactBearer::Internet;
        let mut own_live = advert(1);
        own_live.peer = call("M0EEE");
        trickle.observe(advert(1), 0, AdvertSource::Radio).unwrap();
        trickle
            .observe(from_internet.clone(), 0, AdvertSource::Internet)
            .unwrap();
        trickle
            .observe(internet_link.clone(), 0, AdvertSource::Radio)
            .unwrap();
        trickle
            .observe(own_live.clone(), 0, AdvertSource::Own { on_air: false })
            .unwrap();
        let on_air: Vec<Callsign> = trickle
            .poll(99)
            .into_iter()
            .filter(|due| due.on_air)
            .map(|due| due.advert.peer)
            .collect();
        assert_eq!(on_air, vec![call("M0BBB")]);
        // Heard on the radio later: now it belongs there too.
        trickle.observe(from_internet, 150, AdvertSource::Radio).unwrap();
        assert!(trickle
            .poll(299)
            .iter()
            .any(|due| due.on_air && due.advert.peer == call("M0CCC")));
    }

    #[test]
    fn the_sync_queue_sends_the_newest_and_never_the_expired() {
        let mut queue = SyncQueue::new(4);
        let contact = |sequence: u32, end: u32| {
            let mut a = advert(sequence);
            a.start = 10;
            a.end = end;
            SyncMessage::Contact(a).encode().unwrap()
        };
        queue.push(Dest::Broadcast, contact(1, 500), 100);
        queue.push(Dest::Broadcast, contact(2, 900), 100);
        assert_eq!(
            queue.len(),
            1,
            "a newer copy of the same contact takes the old one's place"
        );
        assert_eq!(queue.front(100).unwrap().payload, contact(2, 900));
        let filter = |salt: u32| {
            SyncMessage::Filter(holdings_filter(0, salt, &[]).unwrap())
                .encode()
                .unwrap()
        };
        queue.push(Dest::Station(call("M0BBB")), filter(1), 100);
        queue.push(Dest::Station(call("M0BBB")), filter(2), 110);
        queue.push(Dest::Station(call("M0CCC")), filter(3), 110);
        assert_eq!(queue.len(), 3);
        assert!(queue.front(899).is_some());
        assert!(queue.front(900).is_none(), "everything has expired");
        assert_eq!(queue.dropped(), 3);
    }

    #[test]
    fn a_frame_longer_than_the_share_goes_out_after_enough_quiet() {
        // 0.05% of an hour is 1.8 s; a 4 s frame needs 8,000 s of quiet.
        let mut budget = ControlBudget::new(3_600_000, 200).unwrap();
        budget.set_permyriad(5);
        assert!(budget.admit(0, 4_000));
        assert!(!budget.admit(3_600_000, 4_000));
        assert!(!budget.admit(7_999_999, 1_000));
        assert!(budget.admit(8_000_000, 4_000));
    }

    #[test]
    fn a_contact_heard_on_air_is_not_sent_again() {
        let mut queue = SyncQueue::new(4);
        let contact = |sequence: u32| SyncMessage::Contact(advert(sequence)).encode().unwrap();
        queue.push(Dest::Broadcast, contact(5), 100);
        queue.heard(&contact(4));
        assert_eq!(queue.len(), 1, "an older copy on air says less than ours");
        queue.heard(&contact(5));
        assert!(queue.is_empty());
    }

    #[test]
    fn the_channel_budgets_are_shared_by_the_stations_on_it() {
        assert_eq!(control_share_permyriad(200, 1), 200);
        assert_eq!(control_share_permyriad(200, 10), 20);
        assert_eq!(control_share_permyriad(200, 1_000), 1);
        // Beacons of 2 s: 10 minutes for up to 6 stations, then longer.
        assert_eq!(beacon_interval_ms(600_000, 6, 2_000), 600_000);
        assert_eq!(beacon_interval_ms(600_000, 40, 2_000), 4_000_000);
        assert_eq!(live_window_secs(600), 1_500);
        assert_eq!(live_window_secs(60), 1_200);
        assert!(radio_pull_due(FLAG_HOLDING, None, 0));
        assert!(!radio_pull_due(0, None, 0), "nothing held, nothing to pull");
        assert!(!radio_pull_due(FLAG_HOLDING, Some(0), RADIO_PULL_SECS - 1));
        assert!(radio_pull_due(FLAG_HOLDING, Some(0), RADIO_PULL_SECS));
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
                true,
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
                true,
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
                    true,
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
                false,
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
                false,
                20,
            )
            .unwrap();
        // Empty store → no offers; admission itself is the assertion.
        assert!(actions.is_empty() || actions.iter().any(|a| matches!(a, ControlAction::Reply(_))));
    }
}
