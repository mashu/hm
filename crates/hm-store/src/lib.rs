//! Persistent station store on `redb` (pure Rust, ACID, crash-safe).
//!
//! - Objects: signed envelopes, content-addressed by bundle id.
//! - Messages: one record per bundle, inbound or outbound, with its state.
//! - Outbox queue: outbound messages still to deliver, highest precedence
//!   first, then oldest, each with its next attempt time.
//!
//! Every operation is one write transaction, so a crash leaves either the
//! old state or the new one. A bundle is stored at most once by content id
//! (local at-most-once). Hop transfer is at-least-once while `Queued`; after
//! a verified handoff the prior custodian may reclaim via shadow grace,
//! suspect timer, or a custody-fail notice. End-to-end `Delivered` requires
//! the destination receipt (otherwise eventually `DeliveredUnconfirmed` or
//! `Failed`).

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
/// Per-peer outbound chat sequence (callsign bytes → last assigned seq).
const PEER_SEQ: TableDefinition<[u8; 6], u64> = TableDefinition::new("peer_seq");

#[derive(Debug)]
pub enum Error {
    Db(redb::Error),
    /// A stored record could not be decoded (corrupt or from a newer version).
    Corrupt(String),
    NotFound,
    /// This handle was opened with [`Store::open_read_only`].
    ReadOnly,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Db(e) => write!(f, "store: {e}"),
            Error::Corrupt(m) => write!(f, "store: corrupt record: {m}"),
            Error::NotFound => f.write_str("store: no such message"),
            Error::ReadOnly => f.write_str("store: opened read-only"),
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
    /// Hop custody was transferred but no destination receipt returned; delivery
    /// may have succeeded.
    #[n(7)]
    DeliveredUnconfirmed,
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
    /// Retain bytes / advertise holdings until this Unix second after handoff.
    #[n(23)]
    pub shadow_until: Option<u64>,
    /// Sender-assigned conversation sequence from the wire bundle (chat).
    #[n(24)]
    pub wire_seq: Option<u64>,
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
            || (self.state == State::InTransit && self.shadow_until.is_some_and(|until| until > 0))
    }

    pub fn in_shadow(&self, now: u64) -> bool {
        self.state == State::InTransit && self.shadow_until.is_some_and(|until| until > now)
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
    pub wire_seq: Option<u64>,
}

impl RelayMetadata<'_> {
    /// Hop accounting that adds up, no loop in the path, not yet expired.
    fn is_valid(&self, now: u64) -> bool {
        self.max_hops != 0
            && self.max_hops <= 16
            && self.hop_count <= self.max_hops
            && usize::from(self.hop_count) == self.visited.len()
            && self.expires_at > now
            && !self
                .visited
                .iter()
                .enumerate()
                .any(|(index, callsign)| self.visited[..index].contains(callsign))
    }
}

#[derive(Copy, Clone, Debug)]
pub struct ReceivedMessage<'a> {
    pub id: ObjectId,
    pub object: &'a [u8],
    pub from: Callsign,
    pub verified: bool,
    pub wire_seq: Option<u64>,
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

#[derive(Copy, Clone, Debug)]
pub struct EnqueueOpts {
    pub to: Callsign,
    pub precedence: u8,
    pub now: u64,
    pub wire_seq: Option<u64>,
    pub expires_at: Option<u64>,
}

#[derive(Copy, Clone, Debug)]
pub struct CustodyHandoff<'a> {
    pub next_hop: Callsign,
    pub receipt_verified: bool,
    pub by: &'a str,
    pub now: u64,
    pub grace_secs: u64,
    pub suspect_secs: u64,
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

/// Outcome of trying to reclaim or close an in-transit holding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReclaimOutcome {
    /// Back on the delivery queue.
    Requeued,
    /// Origin had handed off; destination receipt never arrived.
    DeliveredUnconfirmed,
    /// Relay (or expired origin) gave up.
    Failed,
    /// Not applicable (wrong state / already terminal).
    Ignored,
}

/// Result of an operator asking to remove one locally stored message.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// A queued outbound message was cancelled and kept for status/history.
    Cancelled,
    /// A terminal or received message and its unreferenced object were removed.
    Deleted,
    /// Relay custody or an end-to-end receipt is still outstanding.
    Active,
}

enum Backend {
    ReadWrite(Database),
    ReadOnly(redb::ReadOnlyDatabase),
}

pub struct Store {
    db: Backend,
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
        Bearer::Modem => 2,
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
        2 => Bearer::Modem,
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
    /// Builder shared by the node (writer) and CLI readers so they can share one file.
    fn builder() -> redb::Builder {
        let mut builder = redb::Builder::new();
        // One writer (`hm node`) plus read-only CLI tools (`hm messages`, …).
        builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
        builder
    }

    /// Open the store at `path`, creating it if needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let db = Self::builder().create(path)?;
        let tx = db.begin_write()?;
        {
            tx.open_table(OBJECTS)?;
            tx.open_table(MESSAGES)?;
            tx.open_table(BY_TIME)?;
            tx.open_table(QUEUE)?;
            tx.open_table(META)?;
            tx.open_table(CONTACT_EVIDENCE)?;
            tx.open_table(PEER_SEQ)?;
        }
        tx.commit()?;
        Ok(Store {
            db: Backend::ReadWrite(db),
        })
    }

    /// Open an existing store for reads while another process holds the write lock
    /// (for example `hm messages` beside a running `hm node`).
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Store> {
        let db = Self::builder().open_read_only(path)?;
        Ok(Store {
            db: Backend::ReadOnly(db),
        })
    }

    fn read_tx(&self) -> Result<redb::ReadTransaction> {
        match &self.db {
            Backend::ReadWrite(db) => Ok(db.begin_read()?),
            Backend::ReadOnly(db) => Ok(db.begin_read()?),
        }
    }

    fn write_tx(&self) -> Result<redb::WriteTransaction> {
        match &self.db {
            Backend::ReadWrite(db) => Ok(db.begin_write()?),
            Backend::ReadOnly(_) => Err(Error::ReadOnly),
        }
    }

    fn next_seq(meta: &mut redb::Table<&str, u64>) -> Result<u64> {
        let seq = meta.get("seq")?.map(|v| v.value()).unwrap_or(0) + 1;
        meta.insert("seq", seq)?;
        Ok(seq)
    }

    fn blank_record(id: ObjectId, direction: Direction, peer: Callsign, now: u64, seq: u64) -> Record {
        Record {
            id,
            direction,
            peer,
            at: now,
            state: State::Unread,
            precedence: 0,
            attempts: 0,
            next_attempt: 0,
            verified: false,
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
            shadow_until: None,
            wire_seq: None,
        }
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
        self.put_received_with(id, object, from, verified, now, None)
    }

    pub fn put_received_with(
        &self,
        id: ObjectId,
        object: &[u8],
        from: Callsign,
        verified: bool,
        now: u64,
        wire_seq: Option<u64>,
    ) -> Result<bool> {
        let tx = self.write_tx()?;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            if messages.get(id.0)?.is_some() {
                return Ok(false); // dropping the transaction aborts it
            }
            let seq = Store::next_seq(&mut tx.open_table(META)?)?;
            let mut r = Store::blank_record(id, Direction::In, from, now, seq);
            r.state = State::Unread;
            r.verified = verified;
            r.wire_seq = wire_seq;
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
        let tx = self.write_tx()?;
        let inserted;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            inserted = messages.get(received.id.0)?.is_none();
            if inserted {
                let seq = Store::next_seq(&mut tx.open_table(META)?)?;
                let mut record = Store::blank_record(received.id, Direction::In, received.from, now, seq);
                record.state = State::Unread;
                record.verified = received.verified;
                record.wire_seq = received.wire_seq;
                tx.open_table(OBJECTS)?.insert(received.id.0, received.object)?;
                messages.insert(received.id.0, encode(&record).as_slice())?;
                tx.open_table(BY_TIME)?
                    .insert((0_u8, now, seq, received.id.0), ())?;
            }
            if messages.get(reply.id.0)?.is_none() {
                let seq = Store::next_seq(&mut tx.open_table(META)?)?;
                let mut record = Store::blank_record(reply.id, Direction::Out, reply.to, now, seq);
                record.state = State::Queued;
                record.precedence = reply.precedence;
                record.next_attempt = now;
                record.final_peer = Some(reply.to);
                record.next_hop = Some(reply.to);
                record.hop_count = Some(0);
                record.expires_at = Some(reply.expires_at);
                record.max_hops = Some(reply.max_hops);
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
        self.enqueue_with(
            id,
            object,
            EnqueueOpts {
                to,
                precedence,
                now,
                wire_seq: None,
                expires_at: None,
            },
        )
    }

    /// Queue a bundle with optional wire conversation sequence and expiry.
    pub fn enqueue_with(&self, id: ObjectId, object: &[u8], opts: EnqueueOpts) -> Result<bool> {
        let EnqueueOpts {
            to,
            precedence,
            now,
            wire_seq,
            expires_at,
        } = opts;
        let tx = self.write_tx()?;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            if messages.get(id.0)?.is_some() {
                return Ok(false);
            }
            let seq = Store::next_seq(&mut tx.open_table(META)?)?;
            let mut r = Store::blank_record(id, Direction::Out, to, now, seq);
            r.state = State::Queued;
            r.precedence = precedence;
            r.next_attempt = now;
            r.final_peer = Some(to);
            r.next_hop = Some(to);
            r.hop_count = Some(0);
            r.wire_seq = wire_seq;
            r.expires_at = expires_at;
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
        if !metadata.is_valid(now) {
            return Err(Error::Corrupt("invalid relay metadata".into()));
        }
        let tx = self.write_tx()?;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            if messages.get(id.0)?.is_some() {
                return Ok(false);
            }
            let seq = Store::next_seq(&mut tx.open_table(META)?)?;
            let mut r = Store::blank_record(id, Direction::Relay, metadata.destination, now, seq);
            r.state = State::Queued;
            r.precedence = metadata.precedence;
            r.next_attempt = now;
            r.verified = true;
            r.final_peer = Some(metadata.destination);
            r.hop_count = Some(metadata.hop_count);
            r.visited = (!metadata.visited.is_empty()).then(|| metadata.visited.to_vec());
            r.expires_at = Some(metadata.expires_at);
            r.custody_from = Some(metadata.custody_from);
            r.max_hops = Some(metadata.max_hops);
            r.wire_seq = metadata.wire_seq;
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
        let tx = self.read_tx()?;
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
        let tx = self.write_tx()?;
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
    /// On `GaveUp` for a relay holding, `custody_fail_to` is the prior custodian.
    pub fn attempt_failed(
        &self,
        id: ObjectId,
        reason: &str,
        policy: RetryPolicy,
        now: u64,
    ) -> Result<(Retry, Option<Callsign>)> {
        self.attempt_failed_or_hold(id, reason, policy, now, false)
    }

    /// An attempt failed. With `hold`, a message that has used up its
    /// attempts is not given up but kept queued, tried again every
    /// `max_delay_secs` and whenever [`Store::wake`] says its destination is
    /// in reach, until the bundle expires: store and forward over links that
    /// open for an hour a day must not drop a message after a few hours of
    /// retries. Without `hold` it fails, as [`Store::attempt_failed`] does.
    pub fn attempt_failed_or_hold(
        &self,
        id: ObjectId,
        reason: &str,
        policy: RetryPolicy,
        now: u64,
        hold: bool,
    ) -> Result<(Retry, Option<Callsign>)> {
        self.update(id, |r| {
            if r.state != State::Queued {
                return (Retry::Inactive, None);
            }
            r.attempts += 1;
            r.note = Some(reason.to_string());
            if r.attempts >= policy.max_attempts && hold {
                r.next_attempt = now + policy.max_delay_secs.max(1);
                r.note = Some(format!("{reason}; held until it expires"));
                (Retry::At(r.next_attempt), None)
            } else if r.attempts >= policy.max_attempts {
                r.state = State::Failed;
                let notify = if r.direction == Direction::Relay {
                    r.custody_from
                } else {
                    None
                };
                (Retry::GaveUp, notify)
            } else {
                r.next_attempt = now + policy.delay_after(r.attempts);
                (Retry::At(r.next_attempt), None)
            }
        })
    }

    /// Give up on an outbound message at once (a failure no retry can fix).
    /// Returns the prior custodian to notify with a custody-fail when this was
    /// a relay holding.
    pub fn abandon(&self, id: ObjectId, reason: &str) -> Result<Option<Callsign>> {
        self.update(id, |r| {
            if r.state != State::Queued {
                return None;
            }
            r.attempts += 1;
            r.state = State::Failed;
            r.note = Some(reason.to_string());
            if r.direction == Direction::Relay {
                r.custody_from
            } else {
                None
            }
        })
    }

    /// `destination` is in reach (heard on the radio, or linked): its queued
    /// messages waiting for a later attempt are tried now. Returns how many.
    pub fn wake(&self, destination: Callsign, now: u64) -> Result<usize> {
        let tx = self.write_tx()?;
        let mut woken = 0;
        {
            let queue = tx.open_table(QUEUE)?;
            let mut messages = tx.open_table(MESSAGES)?;
            let mut records = Vec::new();
            for entry in queue.iter()? {
                let (key, _) = entry?;
                let (_, _, id) = key.value();
                if let Some(bytes) = messages.get(id)? {
                    records.push(decode(bytes.value())?);
                }
            }
            for mut r in records {
                if r.state == State::Queued && r.final_destination() == destination && r.next_attempt > now {
                    r.next_attempt = now;
                    messages.insert(r.id.0, encode(&r).as_slice())?;
                    woken += 1;
                }
            }
        }
        tx.commit()?;
        Ok(woken)
    }

    /// Custody of a relay holding this node had given up on is offered again:
    /// take it back on, with the new holding's metadata. False (and nothing
    /// changed) unless the record is a failed relay holding.
    pub fn revive_relay(&self, id: ObjectId, metadata: RelayMetadata<'_>, now: u64) -> Result<bool> {
        if !metadata.is_valid(now) {
            return Err(Error::Corrupt("invalid relay metadata".into()));
        }
        self.update(id, |r| {
            if r.direction != Direction::Relay
                || r.state != State::Failed
                || r.final_destination() != metadata.destination
            {
                return false;
            }
            r.state = State::Queued;
            r.attempts = 0;
            r.next_attempt = now;
            r.precedence = metadata.precedence;
            r.custody_from = Some(metadata.custody_from);
            r.hop_count = Some(metadata.hop_count);
            r.visited = (!metadata.visited.is_empty()).then(|| metadata.visited.to_vec());
            r.max_hops = Some(metadata.max_hops);
            r.expires_at = Some(metadata.expires_at);
            r.next_hop = None;
            r.next_hops = None;
            r.custody_by = None;
            r.custody_copies = None;
            r.shadow_until = None;
            r.note = Some("custody offered again; relaying".into());
            true
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
    /// Unverified receipts MUST NOT transfer custody.
    ///
    /// After handoff, a shadow copy is retained for holdings pull and a suspect
    /// timer schedules reclaim if no end-to-end receipt arrives.
    pub fn custody_transferred(&self, id: ObjectId, handoff: CustodyHandoff<'_>) -> Result<bool> {
        if !handoff.receipt_verified {
            return Ok(false);
        }
        let CustodyHandoff {
            next_hop,
            receipt_verified,
            by,
            now,
            grace_secs,
            suspect_secs,
        } = handoff;
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
            let mut suspect_at = now.saturating_add(suspect_secs.max(1));
            if let Some(expires) = r.expires_at {
                suspect_at = suspect_at.min(expires);
            }
            r.next_attempt = suspect_at;
            r.shadow_until = Some(now.saturating_add(grace_secs.max(1)));
            r.note = None;
            r.by = Some(by.to_string());
            true
        })
    }

    /// In-transit records whose suspect timer has fired.
    pub fn suspect_due(&self, now: u64) -> Result<Vec<Record>> {
        let tx = self.read_tx()?;
        let messages = tx.open_table(MESSAGES)?;
        let mut out = Vec::new();
        for entry in messages.iter()? {
            let (_, bytes) = entry?;
            let record = decode(bytes.value())?;
            if record.state == State::InTransit
                && matches!(record.direction, Direction::Out | Direction::Relay)
                && record.next_attempt <= now
            {
                out.push(record);
            }
        }
        out.sort_by_key(|r| (r.next_attempt, r.seq));
        Ok(out)
    }

    /// Outcome of trying to reclaim or close an in-transit holding.
    /// Reclaim custody after a suspect timer or a custody-fail notice.
    /// When the bundle is expired, origin outbox items become
    /// `DeliveredUnconfirmed`; relay holdings become `Failed`. Otherwise the
    /// copy is re-queued (object bytes remain in the content store).
    pub fn reclaim_custody(&self, id: ObjectId, now: u64, reason: &str) -> Result<ReclaimOutcome> {
        self.update(id, |r| {
            if r.state != State::InTransit {
                return ReclaimOutcome::Ignored;
            }
            let expired = r.expires_at.is_some_and(|expires| expires <= now);
            if !expired {
                if let Some(by) = r.custody_by.take() {
                    if let Some(copies) = &mut r.custody_copies {
                        copies.retain(|c| *c != by);
                        if copies.is_empty() {
                            r.custody_copies = None;
                        }
                    }
                }
                r.next_hops = None;
                r.next_hop = None;
                r.state = State::Queued;
                r.next_attempt = now;
                r.note = Some(reason.to_string());
                r.attempts = 0;
                return ReclaimOutcome::Requeued;
            }
            r.next_hops = None;
            r.next_hop = None;
            r.note = Some(reason.to_string());
            if r.direction == Direction::Out {
                r.state = State::DeliveredUnconfirmed;
                ReclaimOutcome::DeliveredUnconfirmed
            } else {
                r.state = State::Failed;
                ReclaimOutcome::Failed
            }
        })
    }

    /// Apply a verified custody-fail notice from `from` about holding `id`.
    pub fn apply_custody_fail(
        &self,
        id: ObjectId,
        from: Callsign,
        now: u64,
        reason: &str,
    ) -> Result<ReclaimOutcome> {
        let record = match self.record(id)? {
            Some(record) => record,
            None => return Ok(ReclaimOutcome::Ignored),
        };
        if matches!(
            record.state,
            State::Delivered | State::DeliveredUnconfirmed | State::Cancelled | State::Failed
        ) {
            return Ok(ReclaimOutcome::Ignored);
        }
        if record.state != State::InTransit || record.custody_by != Some(from) {
            return Ok(ReclaimOutcome::Ignored);
        }
        self.reclaim_custody(id, now, reason)
    }

    /// Mark an outbox item as delivered without an e2e receipt.
    pub fn delivered_unconfirmed(&self, id: ObjectId, reason: &str, now: u64) -> Result<bool> {
        self.update(id, |r| {
            if r.direction != Direction::Out || r.state != State::InTransit {
                return false;
            }
            r.state = State::DeliveredUnconfirmed;
            r.next_attempt = now;
            r.note = Some(reason.to_string());
            true
        })
    }

    /// Next outbound chat sequence for `peer` (1-based, persistent).
    pub fn next_peer_seq(&self, peer: Callsign) -> Result<u64> {
        let tx = self.write_tx()?;
        let seq;
        {
            let mut table = tx.open_table(PEER_SEQ)?;
            let key = peer.to_bytes();
            seq = table.get(key)?.map(|v| v.value()).unwrap_or(0) + 1;
            table.insert(key, seq)?;
        }
        tx.commit()?;
        Ok(seq)
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
            r.shadow_until = None;
            true
        })
    }

    /// A receipt signed by `destination` for relay holding `id` passed through
    /// this node: the bundle arrived, so neither send it on nor resend it when
    /// the suspect timer fires. False unless an active holding for that
    /// destination.
    pub fn relay_receipted(
        &self,
        id: ObjectId,
        receipt: ObjectId,
        destination: Callsign,
        now: u64,
    ) -> Result<bool> {
        match self.update(id, |r| {
            if r.direction != Direction::Relay
                || !matches!(r.state, State::Queued | State::InTransit)
                || r.final_destination() != destination
            {
                return false;
            }
            r.state = State::Delivered;
            r.verified = true;
            r.e2e_receipt = Some(receipt);
            r.next_attempt = now;
            r.next_hop = None;
            r.next_hops = None;
            r.shadow_until = None;
            r.note = Some("end-to-end receipt passed through".into());
            true
        }) {
            Err(Error::NotFound) => Ok(false),
            other => other,
        }
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

    /// Cancel an outbound queue entry, or permanently remove inactive local
    /// history. Active relay custody and in-transit messages are never deleted.
    ///
    /// Cancelling is deliberately a separate first step: a late click cannot
    /// erase the operator's only indication that a queued message was stopped.
    pub fn delete(&self, id: ObjectId) -> Result<DeleteOutcome> {
        let tx = self.write_tx()?;
        let record;
        let outcome;
        {
            let mut messages = tx.open_table(MESSAGES)?;
            record = match messages.get(id.0)? {
                Some(bytes) => decode(bytes.value())?,
                None => return Err(Error::NotFound),
            };
            if record.direction == Direction::Out && record.state == State::Queued {
                let mut cancelled = record.clone();
                cancelled.state = State::Cancelled;
                cancelled.next_hop = None;
                cancelled.next_hops = None;
                cancelled.note = Some("cancelled by operator".into());
                messages.insert(id.0, encode(&cancelled).as_slice())?;
                outcome = DeleteOutcome::Cancelled;
            } else if matches!(record.state, State::Queued | State::InTransit) {
                return Ok(DeleteOutcome::Active);
            } else {
                messages.remove(id.0)?;
                outcome = DeleteOutcome::Deleted;
            }
        }
        match outcome {
            DeleteOutcome::Cancelled => {
                tx.open_table(QUEUE)?
                    .remove((255 - record.precedence, record.seq, id.0))?;
            }
            DeleteOutcome::Deleted => {
                tx.open_table(BY_TIME)?
                    .remove((record.direction as u8, record.at, record.seq, id.0))?;
                let mut candidates = vec![id];
                if let Some(receipt) = record.e2e_receipt {
                    candidates.push(receipt);
                }
                for candidate in candidates {
                    let referenced = {
                        let messages = tx.open_table(MESSAGES)?;
                        if messages.get(candidate.0)?.is_some() {
                            true
                        } else {
                            let mut found = false;
                            for entry in messages.iter()? {
                                let (_, bytes) = entry?;
                                if decode(bytes.value())?.e2e_receipt == Some(candidate) {
                                    found = true;
                                    break;
                                }
                            }
                            found
                        }
                    };
                    if !referenced {
                        tx.open_table(OBJECTS)?.remove(candidate.0)?;
                    }
                }
            }
            DeleteOutcome::Active => unreachable!("returned before commit"),
        }
        tx.commit()?;
        Ok(outcome)
    }

    pub fn mark_read(&self, id: ObjectId) -> Result<()> {
        self.update(id, |r| {
            if r.state == State::Unread {
                r.state = State::Read;
            }
        })
    }

    pub fn record(&self, id: ObjectId) -> Result<Option<Record>> {
        let tx = self.read_tx()?;
        let messages = tx.open_table(MESSAGES)?;
        let decoded = messages.get(id.0)?.map(|b| decode(b.value())).transpose()?;
        Ok(decoded)
    }

    pub fn object(&self, id: ObjectId) -> Result<Option<Vec<u8>>> {
        let tx = self.read_tx()?;
        let objects = tx.open_table(OBJECTS)?;
        let bytes = objects.get(id.0)?.map(|b| b.value().to_vec());
        Ok(bytes)
    }

    /// Every content id retained locally, for pairwise holdings filters.
    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        let tx = self.read_tx()?;
        let objects = tx.open_table(OBJECTS)?;
        objects
            .iter()?
            .map(|entry| entry.map(|(key, _)| ObjectId(key.value())).map_err(Error::from))
            .collect()
    }

    /// Active ids to advertise to `peer` during pairwise holdings sync.
    pub fn holding_ids(&self, peer: Callsign, relayable: bool, now: u64) -> Result<Vec<ObjectId>> {
        let tx = self.read_tx()?;
        let messages = tx.open_table(MESSAGES)?;
        // Outbox peer for RF/group bulletins (`hm_xfer::broadcast_peer`).
        let bulletin_dest = Callsign::parse("ALL").expect("ALL is a valid callsign");
        let mut out = Vec::new();
        for entry in messages.iter()? {
            let (_, bytes) = entry?;
            let record = decode(bytes.value())?;
            if record.expires_at.is_some_and(|expires| expires <= now)
                || !matches!(record.direction, Direction::Out | Direction::Relay)
            {
                continue;
            }
            let bulletin = record.direction == Direction::Out
                && record.final_destination() == bulletin_dest
                && matches!(record.state, State::Queued | State::Delivered);
            let shadowed = record.in_shadow(now);
            if !bulletin && record.state != State::Queued && !shadowed {
                continue;
            }
            let eligible = if bulletin {
                // Group bulletins are for every peer that asks, not one destination.
                true
            } else if relayable {
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
        let tx = self.read_tx()?;
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
        let tx = self.read_tx()?;
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
        self.save_contact_evidence_batch(&[(key, evidence)])
    }

    /// Save several links' evidence in one transaction.
    pub fn save_contact_evidence_batch(&self, entries: &[(EdgeKey, Evidence)]) -> Result<()> {
        for (key, evidence) in entries {
            if !evidence.successes.is_finite()
                || !evidence.failures.is_finite()
                || evidence.successes < 0.0
                || evidence.failures < 0.0
                || key.utc_hour.is_some_and(|hour| hour > 23)
            {
                return Err(Error::Corrupt("invalid contact evidence".into()));
            }
        }
        let tx = self.write_tx()?;
        {
            let mut table = tx.open_table(CONTACT_EVIDENCE)?;
            for (key, evidence) in entries {
                table.insert(evidence_key(*key), evidence_value(*evidence).as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn contact_evidence(&self) -> Result<Vec<(EdgeKey, Evidence)>> {
        let tx = self.read_tx()?;
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
        let tx = self.read_tx()?;
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
