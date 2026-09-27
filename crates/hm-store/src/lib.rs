//! Persistent station store on `redb` (pure Rust, ACID, crash-safe).
//!
//! - Objects: signed envelopes, content-addressed by bundle id.
//! - Messages: one record per bundle, inbound or outbound, with its state.
//! - Outbox queue: outbound messages still to deliver, highest precedence
//!   first, then oldest, each with its next attempt time.
//!
//! Every operation is one write transaction, so a crash leaves either the
//! old state or the new one. A bundle is stored once however often it
//! arrives, which is what makes delivery exactly-once across restarts.

use std::path::Path;

use hm_route::{Bearer, EdgeKey, Evidence};
use hm_wire::{Callsign, ObjectId};
use minicbor::{Decode, Encode};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

const OBJECTS: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("objects");
const MESSAGES: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("messages");
/// (direction, time, sequence, id): listing in arrival order.
const BY_TIME: TableDefinition<(u8, u64, u64, [u8; 32]), ()> = TableDefinition::new("by_time");
/// (255 - precedence, sequence, id): outbound messages not yet delivered or abandoned.
const QUEUE: TableDefinition<(u8, u64, [u8; 32]), ()> = TableDefinition::new("queue");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const CONTACT_EVIDENCE: TableDefinition<[u8; 16], &[u8]> = TableDefinition::new("contact_evidence");

#[derive(Debug)]
pub enum Error {
    Db(redb::Error),
    /// A stored record could not be decoded (corrupt or from a newer version).
    Corrupt(String),
    NotFound,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Db(e) => write!(f, "store: {e}"),
            Error::Corrupt(m) => write!(f, "store: corrupt record: {m}"),
            Error::NotFound => f.write_str("store: no such message"),
        }
    }
}

impl std::error::Error for Error {}

macro_rules! from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Error {
                Error::Db(e.into())
            }
        }
    )*};
}
from_redb!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError
);

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum Direction {
    #[n(0)]
    In,
    #[n(1)]
    Out,
    /// A bundle held in custody for another station.
    #[n(2)]
    Relay,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum State {
    /// Inbound, not yet marked read.
    #[n(0)]
    Unread,
    #[n(1)]
    Read,
    /// Outbound, waiting for its next attempt.
    #[n(2)]
    Queued,
    /// Outbound, confirmed by the receiver (see `verified` for the receipt).
    #[n(3)]
    Delivered,
    /// Outbound, abandoned after the retry limit.
    #[n(4)]
    Failed,
    /// A next hop accepted custody; only an end-to-end receipt completes it.
    #[n(5)]
    InTransit,
    /// Removed from the local queue by the operator.
    #[n(6)]
    Cancelled,
}

/// What the store knows about one message.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Record {
    #[n(0)]
    pub id: ObjectId,
    #[n(1)]
    pub direction: Direction,
    /// Inbound: the station it came from over the air. Outbound: the destination.
    #[n(2)]
    pub peer: Callsign,
    /// Unix seconds it was received or queued.
    #[n(3)]
    pub at: u64,
    #[n(4)]
    pub state: State,
    #[n(5)]
    pub precedence: u8,
    #[n(6)]
    pub attempts: u32,
    /// Outbound: Unix seconds of the next attempt.
    #[n(7)]
    pub next_attempt: u64,
    /// Inbound: sender's signature verified. Outbound: receiver's receipt verified.
    #[n(8)]
    pub verified: bool,
    #[n(9)]
    pub seq: u64,
    /// Last failure reason, for the operator.
    #[n(10)]
    pub note: Option<String>,
    /// Outbound: the bearer that delivered it ("radio", "internet").
    #[n(11)]
    pub by: Option<String>,
    /// Final station recipient. Absent on legacy records; then `peer` is final.
    #[n(12)]
    pub final_peer: Option<Callsign>,
    /// Current route's immediate next hop.
    #[n(13)]
    pub next_hop: Option<Callsign>,
    /// Mutable hop count from the routing wrapper.
    #[n(14)]
    pub hop_count: Option<u8>,
    /// Mutable visited path from the routing wrapper.
    #[n(15)]
    pub visited: Option<Vec<Callsign>>,
    /// Station that most recently accepted custody from this node.
    #[n(16)]
    pub custody_by: Option<Callsign>,
    /// Verified destination receipt bundle, if known.
    #[n(17)]
    pub e2e_receipt: Option<ObjectId>,
    /// Bundle expiry in Unix seconds, for relay admission and cleanup.
    #[n(18)]
    pub expires_at: Option<u64>,
    /// Previous custodian, for relayed records.
    #[n(19)]
    pub custody_from: Option<Callsign>,
    /// Signed bundle hop limit, absent on legacy records.
    #[n(20)]
    pub max_hops: Option<u8>,
    /// At most two custodians for Immediate/Flash traffic.
    #[n(21)]
    pub custody_copies: Option<Vec<Callsign>>,
    /// Handoffs currently queued or in flight.
    #[n(22)]
    pub next_hops: Option<Vec<Callsign>>,
}

impl Record {
    pub fn final_destination(&self) -> Callsign {
        self.final_peer.unwrap_or(self.peer)
    }

    pub fn immediate_peer(&self) -> Callsign {
        self.next_hop.unwrap_or(self.peer)
    }

    pub fn is_active_holding(&self) -> bool {
        self.state == State::Queued
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HoldingUsage {
    pub count: usize,
    pub bytes: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AdmissionLimits {
    pub max_count: usize,
    pub max_bytes: u64,
}

#[derive(Copy, Clone, Debug)]
pub struct RelayMetadata<'a> {
    pub custody_from: Callsign,
    pub destination: Callsign,
    pub precedence: u8,
    pub hop_count: u8,
    pub visited: &'a [Callsign],
    pub max_hops: u8,
    pub expires_at: u64,
}

#[derive(Copy, Clone, Debug)]
pub struct ReceivedMessage<'a> {
    pub id: ObjectId,
    pub object: &'a [u8],
    pub from: Callsign,
    pub verified: bool,
}

#[derive(Copy, Clone, Debug)]
pub struct QueuedMessage<'a> {
    pub id: ObjectId,
    pub object: &'a [u8],
    pub to: Callsign,
    pub precedence: u8,
    pub expires_at: u64,
    pub max_hops: u8,
}

impl AdmissionLimits {
    pub fn admits(self, usage: HoldingUsage, object_bytes: usize) -> bool {
        usage.count < self.max_count && usage.bytes.saturating_add(object_bytes as u64) <= self.max_bytes
    }
}

/// When failed deliveries are tried again.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub first_delay_secs: u64,
    pub max_delay_secs: u64,
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    /// 1, 2, 4, ... minutes, capped at an hour, abandoned after 12 attempts.
    fn default() -> Self {
        RetryPolicy {
            first_delay_secs: 60,
            max_delay_secs: 3600,
            max_attempts: 12,
        }
    }
}

impl RetryPolicy {
    pub fn delay_after(&self, attempts: u32) -> u64 {
        let doubled = self
            .first_delay_secs
            .saturating_mul(1u64 << attempts.saturating_sub(1).min(20));
        doubled.min(self.max_delay_secs)
    }
}

/// Outcome of a failed attempt.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Retry {
    At(u64),
    GaveUp,
    Inactive,
}

pub struct Store {
    db: Database,
}

fn encode(r: &Record) -> Vec<u8> {
    minicbor::to_vec(r).expect("writing to a Vec cannot fail")
}

fn decode(bytes: &[u8]) -> Result<Record> {
    minicbor::decode(bytes).map_err(|e| Error::Corrupt(e.to_string()))
}

fn evidence_key(key: EdgeKey) -> [u8; 16] {
    let mut out = [0_u8; 16];
    out[..6].copy_from_slice(&key.from.to_bytes());
    out[6..12].copy_from_slice(&key.to.to_bytes());
    out[12] = match key.bearer {
        Bearer::Radio => 0,
        Bearer::Internet => 1,
    };
    out[13] = key.utc_hour.unwrap_or(u8::MAX);
    out
}

fn decode_evidence_key(bytes: [u8; 16]) -> Result<EdgeKey> {
    let from = Callsign::from_bytes(
        bytes[..6]
            .try_into()
            .map_err(|_| Error::Corrupt("contact evidence key".into()))?,
    )
    .map_err(|error| Error::Corrupt(error.to_string()))?;
    let to = Callsign::from_bytes(
        bytes[6..12]
            .try_into()
            .map_err(|_| Error::Corrupt("contact evidence key".into()))?,
    )
    .map_err(|error| Error::Corrupt(error.to_string()))?;
    let bearer = match bytes[12] {
        0 => Bearer::Radio,
        1 => Bearer::Internet,
        _ => return Err(Error::Corrupt("contact evidence bearer".into())),
    };
    let utc_hour = match bytes[13] {
        u8::MAX => None,
        hour @ 0..=23 => Some(hour),
        _ => return Err(Error::Corrupt("contact evidence hour".into())),
    };
    Ok(EdgeKey {
        from,
        to,
        bearer,
        utc_hour,
    })
}

fn evidence_value(evidence: Evidence) -> [u8; 24] {
    let mut out = [0_u8; 24];
    out[..8].copy_from_slice(&evidence.successes.to_be_bytes());
    out[8..16].copy_from_slice(&evidence.failures.to_be_bytes());
    out[16..].copy_from_slice(&evidence.at.to_be_bytes());
    out
}

fn decode_evidence_value(bytes: &[u8]) -> Result<Evidence> {
    if bytes.len() != 24 {
        return Err(Error::Corrupt("contact evidence length".into()));
    }
    let successes = f64::from_be_bytes(
        bytes[..8]
            .try_into()
            .map_err(|_| Error::Corrupt("contact evidence value".into()))?,
    );
    let failures = f64::from_be_bytes(
        bytes[8..16]
            .try_into()
            .map_err(|_| Error::Corrupt("contact evidence value".into()))?,
    );
    let at = u64::from_be_bytes(
        bytes[16..]
            .try_into()
            .map_err(|_| Error::Corrupt("contact evidence value".into()))?,
    );
    if !successes.is_finite() || !failures.is_finite() || successes < 0.0 || failures < 0.0 {
        return Err(Error::Corrupt("invalid contact evidence".into()));
    }
    Ok(Evidence {
        successes,
        failures,
        at,
    })
}

impl Store {
    /// Open the store at `path`, creating it if needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        {
            tx.open_table(OBJECTS)?;
            tx.open_table(MESSAGES)?;
            tx.open_table(BY_TIME)?;
            tx.open_table(QUEUE)?;
            tx.open_table(META)?;
            tx.open_table(CONTACT_EVIDENCE)?;
        }
        tx.commit()?;
        Ok(Store { db })
    }

    fn next_seq(meta: &mut redb::Table<&str, u64>) -> Result<u64> {
        let seq = meta.get("seq")?.map(|v| v.value()).unwrap_or(0) + 1;
        meta.insert("seq", seq)?;
        Ok(seq)
    }

    /// Store a received bundle. Returns false, changing nothing, if this bundle
    /// is already stored (inbound or outbound).
    pub fn put_received(
        &self,
        id: ObjectId,
        object: &[u8],
        from: Callsign,
        verified: bool,
        now: u64,
    ) -> Result<bool> {
        let tx = self.db.begin_write()?;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            if messages.get(id.0)?.is_some() {
                return Ok(false); // dropping the transaction aborts it
            }
            let seq = Store::next_seq(&mut tx.open_table(META)?)?;
            let r = Record {
                id,
                direction: Direction::In,
                peer: from,
                at: now,
                state: State::Unread,
                precedence: 0,
                attempts: 0,
                next_attempt: 0,
                verified,
                seq,
                note: None,
                by: None,
                final_peer: None,
                next_hop: None,
                hop_count: None,
                visited: None,
                custody_by: None,
                e2e_receipt: None,
                expires_at: None,
                custody_from: None,
                max_hops: None,
                custody_copies: None,
                next_hops: None,
            };
            tx.open_table(OBJECTS)?.insert(id.0, object)?;
            messages.insert(id.0, encode(&r).as_slice())?;
            tx.open_table(BY_TIME)?.insert((0u8, now, seq, id.0), ())?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Atomically store a final delivery and queue its end-to-end receipt.
    pub fn receive_with_reply(
        &self,
        received: ReceivedMessage<'_>,
        reply: QueuedMessage<'_>,
        now: u64,
    ) -> Result<bool> {
        if received.id == reply.id {
            return Err(Error::Corrupt("reply id equals received id".into()));
        }
        let tx = self.db.begin_write()?;
        let inserted;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            inserted = messages.get(received.id.0)?.is_none();
            if inserted {
                let seq = Store::next_seq(&mut tx.open_table(META)?)?;
                let record = Record {
                    id: received.id,
                    direction: Direction::In,
                    peer: received.from,
                    at: now,
                    state: State::Unread,
                    precedence: 0,
                    attempts: 0,
                    next_attempt: 0,
                    verified: received.verified,
                    seq,
                    note: None,
                    by: None,
                    final_peer: None,
                    next_hop: None,
                    hop_count: None,
                    visited: None,
                    custody_by: None,
                    e2e_receipt: None,
                    expires_at: None,
                    custody_from: None,
                    max_hops: None,
                    custody_copies: None,
                    next_hops: None,
                };
                tx.open_table(OBJECTS)?.insert(received.id.0, received.object)?;
                messages.insert(received.id.0, encode(&record).as_slice())?;
                tx.open_table(BY_TIME)?
                    .insert((0_u8, now, seq, received.id.0), ())?;
            }
            if messages.get(reply.id.0)?.is_none() {
                let seq = Store::next_seq(&mut tx.open_table(META)?)?;
                let record = Record {
                    id: reply.id,
                    direction: Direction::Out,
                    peer: reply.to,
                    at: now,
                    state: State::Queued,
                    precedence: reply.precedence,
                    attempts: 0,
                    next_attempt: now,
                    verified: false,
                    seq,
                    note: None,
                    by: None,
                    final_peer: Some(reply.to),
                    next_hop: Some(reply.to),
                    hop_count: Some(0),
                    visited: None,
                    custody_by: None,
                    e2e_receipt: None,
                    expires_at: Some(reply.expires_at),
                    custody_from: None,
                    max_hops: Some(reply.max_hops),
                    custody_copies: None,
                    next_hops: None,
                };
                tx.open_table(OBJECTS)?.insert(reply.id.0, reply.object)?;
                messages.insert(reply.id.0, encode(&record).as_slice())?;
                tx.open_table(BY_TIME)?.insert((1_u8, now, seq, reply.id.0), ())?;
                tx.open_table(QUEUE)?
                    .insert((255 - reply.precedence, seq, reply.id.0), ())?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Queue a bundle for delivery to `to`. Returns false if it is already stored.
    pub fn enqueue(
        &self,
        id: ObjectId,
        object: &[u8],
        to: Callsign,
        precedence: u8,
        now: u64,
    ) -> Result<bool> {
        let tx = self.db.begin_write()?;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            if messages.get(id.0)?.is_some() {
                return Ok(false);
            }
            let seq = Store::next_seq(&mut tx.open_table(META)?)?;
            let r = Record {
                id,
                direction: Direction::Out,
                peer: to,
                at: now,
                state: State::Queued,
                precedence,
                attempts: 0,
                next_attempt: now,
                verified: false,
                seq,
                note: None,
                by: None,
                final_peer: Some(to),
                next_hop: Some(to),
                hop_count: Some(0),
                visited: None,
                custody_by: None,
                e2e_receipt: None,
                expires_at: None,
                custody_from: None,
                max_hops: None,
                custody_copies: None,
                next_hops: None,
            };
            tx.open_table(OBJECTS)?.insert(id.0, object)?;
            messages.insert(id.0, encode(&r).as_slice())?;
            tx.open_table(BY_TIME)?.insert((1u8, now, seq, id.0), ())?;
            tx.open_table(QUEUE)?.insert((255 - precedence, seq, id.0), ())?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Accept durable custody of a bundle that must be relayed onward.
    pub fn enqueue_relay(
        &self,
        id: ObjectId,
        object: &[u8],
        metadata: RelayMetadata<'_>,
        now: u64,
    ) -> Result<bool> {
        if metadata.max_hops == 0
            || metadata.max_hops > 16
            || metadata.hop_count > metadata.max_hops
            || usize::from(metadata.hop_count) != metadata.visited.len()
            || metadata.expires_at <= now
            || metadata
                .visited
                .iter()
                .enumerate()
                .any(|(index, callsign)| metadata.visited[..index].contains(callsign))
        {
            return Err(Error::Corrupt("invalid relay metadata".into()));
        }
        let tx = self.db.begin_write()?;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            if messages.get(id.0)?.is_some() {
                return Ok(false);
            }
            let seq = Store::next_seq(&mut tx.open_table(META)?)?;
            let r = Record {
                id,
                direction: Direction::Relay,
                peer: metadata.destination,
                at: now,
                state: State::Queued,
                precedence: metadata.precedence,
                attempts: 0,
                next_attempt: now,
                verified: true,
                seq,
                note: None,
                by: None,
                final_peer: Some(metadata.destination),
                next_hop: None,
                hop_count: Some(metadata.hop_count),
                visited: (!metadata.visited.is_empty()).then(|| metadata.visited.to_vec()),
                custody_by: None,
                e2e_receipt: None,
                expires_at: Some(metadata.expires_at),
                custody_from: Some(metadata.custody_from),
                max_hops: Some(metadata.max_hops),
                custody_copies: None,
                next_hops: None,
            };
            tx.open_table(OBJECTS)?.insert(id.0, object)?;
            messages.insert(id.0, encode(&r).as_slice())?;
            tx.open_table(BY_TIME)?.insert((2u8, now, seq, id.0), ())?;
            tx.open_table(QUEUE)?
                .insert((255 - metadata.precedence, seq, id.0), ())?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Queued outbound messages whose next attempt is due, in delivery order.
    pub fn due(&self, now: u64) -> Result<Vec<Record>> {
        let tx = self.db.begin_read()?;
        let queue = tx.open_table(QUEUE)?;
        let messages = tx.open_table(MESSAGES)?;
        let mut out = Vec::new();
        for entry in queue.iter()? {
            let (key, _) = entry?;
            let (_, _, id) = key.value();
            let bytes = messages
                .get(id)?
                .ok_or_else(|| Error::Corrupt("queued message has no record".into()))?;
            let r = decode(bytes.value())?;
            if r.next_attempt <= now {
                out.push(r);
            }
        }
        Ok(out)
    }

    fn update<T>(&self, id: ObjectId, f: impl FnOnce(&mut Record) -> T) -> Result<T> {
        let tx = self.db.begin_write()?;
        let result;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            let mut r = match messages.get(id.0)? {
                Some(b) => decode(b.value())?,
                None => return Err(Error::NotFound),
            };
            let was_queued = r.state == State::Queued;
            result = f(&mut r);
            messages.insert(id.0, encode(&r).as_slice())?;
            if was_queued && r.state != State::Queued {
                tx.open_table(QUEUE)?.remove((255 - r.precedence, r.seq, id.0))?;
            } else if !was_queued && r.state == State::Queued {
                tx.open_table(QUEUE)?
                    .insert((255 - r.precedence, r.seq, id.0), ())?;
            }
        }
        tx.commit()?;
        Ok(result)
    }

    /// The receiver confirmed an outbound message, over bearer `by`.
    pub fn delivered(&self, id: ObjectId, receipt_verified: bool, by: &str, now: u64) -> Result<()> {
        self.update(id, |r| {
            if r.state == State::Cancelled {
                return;
            }
            r.state = State::Delivered;
            r.verified = receipt_verified;
            r.attempts += 1;
            r.next_attempt = now;
            r.note = None;
            r.by = Some(by.to_string());
        })
    }

    /// An attempt failed; schedule the next one or give up.
    pub fn attempt_failed(&self, id: ObjectId, reason: &str, policy: RetryPolicy, now: u64) -> Result<Retry> {
        self.update(id, |r| {
            if r.state != State::Queued {
                return Retry::Inactive;
            }
            r.attempts += 1;
            r.note = Some(reason.to_string());
            if r.attempts >= policy.max_attempts {
                r.state = State::Failed;
                Retry::GaveUp
            } else {
                r.next_attempt = now + policy.delay_after(r.attempts);
                Retry::At(r.next_attempt)
            }
        })
    }

    /// Give up on an outbound message at once (a failure no retry can fix).
    pub fn abandon(&self, id: ObjectId, reason: &str) -> Result<()> {
        self.update(id, |r| {
            if r.state != State::Queued {
                return;
            }
            r.attempts += 1;
            r.state = State::Failed;
            r.note = Some(reason.to_string());
        })
    }

    /// Persist the chosen immediate hop before starting a handoff.
    pub fn set_next_hop(&self, id: ObjectId, next_hop: Callsign) -> Result<bool> {
        self.update(id, |r| {
            if r.state != State::Queued {
                return false;
            }
            let limit = if r.precedence >= 2 { 2 } else { 1 };
            let next_hops = r.next_hops.get_or_insert_with(Vec::new);
            if next_hops.contains(&next_hop) {
                return true;
            }
            if next_hops.len() >= limit {
                return false;
            }
            next_hops.push(next_hop);
            r.next_hop = next_hops.first().copied();
            true
        })
    }

    pub fn clear_next_hop(&self, id: ObjectId, next_hop: Callsign) -> Result<()> {
        self.update(id, |r| {
            if let Some(next_hops) = &mut r.next_hops {
                next_hops.retain(|candidate| *candidate != next_hop);
                if next_hops.is_empty() {
                    r.next_hops = None;
                }
            }
            r.next_hop = r.next_hops.as_ref().and_then(|hops| hops.first().copied());
        })
    }

    /// A verified transfer receipt proves durable custody at `next_hop`.
    pub fn custody_transferred(
        &self,
        id: ObjectId,
        next_hop: Callsign,
        receipt_verified: bool,
        by: &str,
        now: u64,
    ) -> Result<bool> {
        self.update(id, |r| {
            if !matches!(r.state, State::Queued | State::InTransit)
                || !r
                    .next_hops
                    .as_deref()
                    .unwrap_or_else(|| std::slice::from_ref(&r.peer))
                    .contains(&next_hop)
            {
                return false;
            }
            let copy_limit = if r.precedence >= 2 { 2 } else { 1 };
            let copies = r.custody_copies.get_or_insert_with(Vec::new);
            if !copies.contains(&next_hop) {
                if copies.len() >= copy_limit {
                    return false;
                }
                copies.push(next_hop);
            }
            if let Some(next_hops) = &mut r.next_hops {
                next_hops.retain(|candidate| *candidate != next_hop);
                if next_hops.is_empty() {
                    r.next_hops = None;
                }
            }
            r.state = State::InTransit;
            r.verified |= receipt_verified;
            r.custody_by = Some(next_hop);
            r.next_hop = r.next_hops.as_ref().and_then(|hops| hops.first().copied());
            r.attempts += 1;
            r.next_attempt = now;
            r.note = None;
            r.by = Some(by.to_string());
            true
        })
    }

    /// Only a receipt signed by the final destination completes an outbox item.
    pub fn e2e_delivered(
        &self,
        id: ObjectId,
        receipt: ObjectId,
        destination: Callsign,
        now: u64,
    ) -> Result<bool> {
        self.update(id, |r| {
            if r.direction != Direction::Out
                || r.state == State::Cancelled
                || r.final_destination() != destination
            {
                return false;
            }
            r.state = State::Delivered;
            r.verified = true;
            r.e2e_receipt = Some(receipt);
            r.next_attempt = now;
            r.note = None;
            true
        })
    }

    /// Drop a locally queued outbound message. Late handoff receipts are ignored.
    pub fn cancel(&self, id: ObjectId) -> Result<bool> {
        self.update(id, |r| {
            if r.direction != Direction::Out || r.state != State::Queued {
                return false;
            }
            r.state = State::Cancelled;
            r.next_hop = None;
            r.next_hops = None;
            r.note = Some("cancelled by operator".into());
            true
        })
    }

    pub fn mark_read(&self, id: ObjectId) -> Result<()> {
        self.update(id, |r| {
            if r.state == State::Unread {
                r.state = State::Read;
            }
        })
    }

    pub fn record(&self, id: ObjectId) -> Result<Option<Record>> {
        let tx = self.db.begin_read()?;
        let messages = tx.open_table(MESSAGES)?;
        messages.get(id.0)?.map(|b| decode(b.value())).transpose()
    }

    pub fn object(&self, id: ObjectId) -> Result<Option<Vec<u8>>> {
        let tx = self.db.begin_read()?;
        let objects = tx.open_table(OBJECTS)?;
        Ok(objects.get(id.0)?.map(|b| b.value().to_vec()))
    }

    /// Every content id retained locally, for pairwise holdings filters.
    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        let tx = self.db.begin_read()?;
        let objects = tx.open_table(OBJECTS)?;
        objects
            .iter()?
            .map(|entry| entry.map(|(key, _)| ObjectId(key.value())).map_err(Error::from))
            .collect()
    }

    /// Active ids to advertise to `peer` during pairwise holdings sync.
    pub fn holding_ids(&self, peer: Callsign, relayable: bool, now: u64) -> Result<Vec<ObjectId>> {
        let tx = self.db.begin_read()?;
        let messages = tx.open_table(MESSAGES)?;
        let mut out = Vec::new();
        for entry in messages.iter()? {
            let (_, bytes) = entry?;
            let record = decode(bytes.value())?;
            if record.state != State::Queued
                || record.expires_at.is_some_and(|expires| expires <= now)
                || !matches!(record.direction, Direction::Out | Direction::Relay)
            {
                continue;
            }
            let eligible = if relayable {
                record.custody_from != Some(peer)
                    && !record
                        .visited
                        .as_deref()
                        .is_some_and(|visited| visited.contains(&peer))
            } else {
                record.final_destination() == peer
            };
            if eligible {
                out.push(record.id);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Current relay-custody pressure; local outbox items are not admission load.
    pub fn relay_usage(&self, now: u64) -> Result<HoldingUsage> {
        let tx = self.db.begin_read()?;
        let messages = tx.open_table(MESSAGES)?;
        let objects = tx.open_table(OBJECTS)?;
        let mut usage = HoldingUsage { count: 0, bytes: 0 };
        for entry in messages.iter()? {
            let (_, bytes) = entry?;
            let record = decode(bytes.value())?;
            if record.direction != Direction::Relay
                || record.state != State::Queued
                || record.expires_at.is_some_and(|expires| expires <= now)
            {
                continue;
            }
            let object = objects
                .get(record.id.0)?
                .ok_or_else(|| Error::Corrupt("relay record has no object".into()))?;
            usage.count += 1;
            usage.bytes = usage.bytes.saturating_add(object.value().len() as u64);
        }
        Ok(usage)
    }

    /// Resolve an abbreviated WANT id only when it is unambiguous locally.
    pub fn object_with_prefix(&self, prefix: [u8; 8]) -> Result<Option<(ObjectId, Vec<u8>)>> {
        let tx = self.db.begin_read()?;
        let objects = tx.open_table(OBJECTS)?;
        let mut found = None;
        for entry in objects.iter()? {
            let (key, value) = entry?;
            let id = ObjectId(key.value());
            if id.prefix8() != prefix {
                continue;
            }
            if found.is_some() {
                return Ok(None);
            }
            found = Some((id, value.value().to_vec()));
        }
        Ok(found)
    }

    pub fn save_contact_evidence(&self, key: EdgeKey, evidence: Evidence) -> Result<()> {
        if !evidence.successes.is_finite()
            || !evidence.failures.is_finite()
            || evidence.successes < 0.0
            || evidence.failures < 0.0
            || key.utc_hour.is_some_and(|hour| hour > 23)
        {
            return Err(Error::Corrupt("invalid contact evidence".into()));
        }
        let tx = self.db.begin_write()?;
        {
            tx.open_table(CONTACT_EVIDENCE)?
                .insert(evidence_key(key), evidence_value(evidence).as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn contact_evidence(&self) -> Result<Vec<(EdgeKey, Evidence)>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(CONTACT_EVIDENCE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            out.push((
                decode_evidence_key(key.value())?,
                decode_evidence_value(value.value())?,
            ));
        }
        Ok(out)
    }

    /// The newest `limit` messages in one direction, newest first.
    pub fn list(&self, direction: Direction, limit: usize) -> Result<Vec<Record>> {
        let d = direction as u8;
        let tx = self.db.begin_read()?;
        let by_time = tx.open_table(BY_TIME)?;
        let messages = tx.open_table(MESSAGES)?;
        let mut out = Vec::new();
        for entry in by_time
            .range((d, 0, 0, [0u8; 32])..=(d, u64::MAX, u64::MAX, [0xFF; 32]))?
            .rev()
        {
            if out.len() == limit {
                break;
            }
            let (key, _) = entry?;
            let (_, _, _, id) = key.value();
            if let Some(b) = messages.get(id)? {
                out.push(decode(b.value())?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
