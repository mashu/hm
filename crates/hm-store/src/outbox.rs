//! Messages to send: queued by precedence and age, tried, retried, held,
//! woken when their station is heard.

use hm_wire::{Callsign, ObjectId};
use redb::ReadableTable;

use crate::{decode, encode, Error, Record, Result, Store};
use crate::{Direction, State, BY_TIME, MESSAGES, META, OBJECTS, PEER_SEQ, QUEUE};

#[derive(Copy, Clone, Debug)]
pub struct EnqueueOpts {
    pub to: Callsign,
    pub precedence: u8,
    pub now: u64,
    pub wire_seq: Option<u64>,
    pub expires_at: Option<u64>,
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

impl Store {
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

    /// Hold a queued message until `until`, waiting to leave through `via`
    /// (see [`Store::wake`]); no attempt is counted. False if it is not queued.
    pub fn defer(&self, id: ObjectId, until: u64, via: Option<Callsign>) -> Result<bool> {
        self.update(id, |r| {
            if r.state != State::Queued {
                return false;
            }
            r.next_attempt = until;
            r.waiting_for = via;
            true
        })
    }

    /// `station` is in reach (heard on the radio, or linked): queued messages
    /// for it, and those waiting to leave through it, waiting for a later
    /// attempt are tried now. Returns how many.
    pub fn wake(&self, station: Callsign, now: u64) -> Result<usize> {
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
                let for_station = r.final_destination() == station || r.waiting_for == Some(station);
                if r.state == State::Queued && for_station && r.next_attempt > now {
                    r.next_attempt = now;
                    messages.insert(r.id.0, encode(&r).as_slice())?;
                    woken += 1;
                }
            }
        }
        tx.commit()?;
        Ok(woken)
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
}
