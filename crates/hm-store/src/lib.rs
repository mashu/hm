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

mod beliefs;
mod custody;
mod inbound;
mod outbox;
mod record;
mod relay;

use std::path::Path;

use hm_wire::{Callsign, ObjectId};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

const OBJECTS: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("objects");
const MESSAGES: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("messages");
/// (direction, time, sequence, id): listing in arrival order.
const BY_TIME: TableDefinition<(u8, u64, u64, [u8; 32]), ()> = TableDefinition::new("by_time");
/// (255 - precedence, sequence, id): outbound messages not yet delivered or abandoned.
const QUEUE: TableDefinition<(u8, u64, [u8; 32]), ()> = TableDefinition::new("queue");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// The station's beliefs about links and custodians (`hm_model::Beliefs`
/// records), opaque to the store.
const BELIEFS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("beliefs");
/// End-to-end deliveries of messages handed to a custodian, not yet taken by
/// the node: message id -> custodian (6 bytes), planned arrival and receipt
/// times.
const CUSTODY_OUTCOMES: TableDefinition<[u8; 32], [u8; 22]> = TableDefinition::new("custody_outcomes");
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

pub use custody::{CustodyHandoff, CustodyOutcome, ReclaimOutcome};
pub use inbound::{QueuedMessage, ReceivedMessage};
pub use outbox::{EnqueueOpts, Retry, RetryPolicy};
pub use record::{Direction, Handed, Record, State};
pub use relay::{AdmissionLimits, HoldingUsage, RelayMetadata};

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
        Self::init(Self::builder().create(path)?)
    }

    /// A store in memory, gone when dropped: for simulations, which give each
    /// of many stations a store of its own.
    pub fn in_memory() -> Result<Store> {
        // One process only: the shared-file concurrency mode needs a file.
        Self::init(redb::Builder::new().create_with_backend(redb::backends::InMemoryBackend::new())?)
    }

    fn init(db: Database) -> Result<Store> {
        let tx = db.begin_write()?;
        {
            tx.open_table(OBJECTS)?;
            tx.open_table(MESSAGES)?;
            tx.open_table(BY_TIME)?;
            tx.open_table(QUEUE)?;
            tx.open_table(META)?;
            tx.open_table(BELIEFS)?;
            tx.open_table(CUSTODY_OUTCOMES)?;
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
            custody_at: None,
            waiting_for: None,
            custody_eta: None,
            first_custody: None,
        }
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
