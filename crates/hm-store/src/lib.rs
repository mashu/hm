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
            };
            tx.open_table(OBJECTS)?.insert(id.0, object)?;
            messages.insert(id.0, encode(&r).as_slice())?;
            tx.open_table(BY_TIME)?.insert((0u8, now, seq, id.0), ())?;
        }
        tx.commit()?;
        Ok(true)
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
            };
            tx.open_table(OBJECTS)?.insert(id.0, object)?;
            messages.insert(id.0, encode(&r).as_slice())?;
            tx.open_table(BY_TIME)?.insert((1u8, now, seq, id.0), ())?;
            tx.open_table(QUEUE)?.insert((255 - precedence, seq, id.0), ())?;
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
            }
        }
        tx.commit()?;
        Ok(result)
    }

    /// The receiver confirmed an outbound message, over bearer `by`.
    pub fn delivered(&self, id: ObjectId, receipt_verified: bool, by: &str, now: u64) -> Result<()> {
        self.update(id, |r| {
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
            r.attempts += 1;
            r.state = State::Failed;
            r.note = Some(reason.to_string());
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
