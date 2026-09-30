//! Custody handed on: waiting for the end-to-end receipt, reclaiming when it
//! does not come, and what the receipts that do come say about custodians.

use hm_wire::{Callsign, ObjectId};
use redb::ReadableTable;

use crate::{decode, Error, Record, Result, Store};
use crate::{Direction, State, CUSTODY_OUTCOMES, MESSAGES};

#[derive(Copy, Clone, Debug)]
pub struct CustodyHandoff<'a> {
    pub next_hop: Callsign,
    pub receipt_verified: bool,
    pub by: &'a str,
    pub now: u64,
    pub grace_secs: u64,
    pub suspect_secs: u64,
    /// When the route planned the message to arrive.
    pub eta: u64,
    /// This station waits for the end-to-end receipt, and reclaims custody
    /// if it does not come: the message's origin does. A relay's part ends
    /// with the next custodian's receipt (the origin covers a custodian that
    /// loses the message), as does that of anything no receipt answers (a
    /// receipt, a custody-fail notice).
    pub awaits_receipt: bool,
}

/// A message handed to `custodian`, on a route planned to arrive at
/// `expected_at`, was confirmed delivered end to end at `delivered_at`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CustodyOutcome {
    pub id: ObjectId,
    pub custodian: Callsign,
    pub expected_at: u64,
    pub delivered_at: u64,
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

impl Store {
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
            eta,
            awaits_receipt,
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
            r.custody_at = Some(now);
            r.custody_eta = Some(eta.max(now));
            r.next_hop = r.next_hops.as_ref().and_then(|hops| hops.first().copied());
            r.attempts += 1;
            r.by = Some(by.to_string());
            if !awaits_receipt {
                // Nothing to wait for: handed to its destination it is
                // delivered, to a custodian it is that custodian's.
                r.state = if next_hop == r.final_destination() {
                    State::Delivered
                } else {
                    State::DeliveredUnconfirmed
                };
                r.next_attempt = now;
                r.shadow_until = None;
                r.note = Some(format!("custody passed to {next_hop}"));
                return true;
            }
            let mut suspect_at = now.saturating_add(suspect_secs.max(1));
            if let Some(expires) = r.expires_at {
                suspect_at = suspect_at.min(expires);
            }
            r.next_attempt = suspect_at;
            r.shadow_until = Some(now.saturating_add(grace_secs.max(1)));
            r.note = None;
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
            if r.state != State::InTransit && !r.handed_on() {
                return ReclaimOutcome::Ignored;
            }
            let expired = r.expires_at.is_some_and(|expires| expires <= now);
            if !expired {
                if r.first_custody.is_none() {
                    r.first_custody = r.handed();
                }
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
        // A relay's part ended when it handed the holding on; the custodian
        // that cannot deliver hands it back.
        if !(record.state == State::InTransit || record.handed_on()) || record.custody_by != Some(from) {
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

    /// Only a receipt signed by the final destination completes an outbox item.
    pub fn e2e_delivered(
        &self,
        id: ObjectId,
        receipt: ObjectId,
        destination: Callsign,
        now: u64,
    ) -> Result<bool> {
        let outcome = self.update(id, |r| {
            if r.direction != Direction::Out
                || r.state == State::Cancelled
                || r.final_destination() != destination
            {
                return None;
            }
            // The first custodian's copy had the head start: credit it,
            // even when the receipt comes after it was reclaimed.
            let handed = r
                .first_custody
                .or_else(|| (r.state == State::InTransit).then(|| r.handed()).flatten());
            r.state = State::Delivered;
            r.verified = true;
            r.e2e_receipt = Some(receipt);
            r.next_attempt = now;
            r.note = None;
            r.shadow_until = None;
            Some(handed)
        })?;
        let Some(handed) = outcome else {
            return Ok(false);
        };
        if let Some(handed) = handed {
            let mut value = [0_u8; 22];
            value[..6].copy_from_slice(&handed.custodian.to_bytes());
            value[6..14].copy_from_slice(&handed.eta.to_be_bytes());
            value[14..].copy_from_slice(&now.to_be_bytes());
            let tx = self.write_tx()?;
            tx.open_table(CUSTODY_OUTCOMES)?.insert(id.0, value)?;
            tx.commit()?;
        }
        Ok(true)
    }

    /// End-to-end deliveries of messages that went through a custodian since
    /// the last call, removed as they are returned.
    pub fn take_custody_outcomes(&self) -> Result<Vec<CustodyOutcome>> {
        let tx = self.write_tx()?;
        let mut out = Vec::new();
        {
            let mut table = tx.open_table(CUSTODY_OUTCOMES)?;
            for entry in table.iter()? {
                let (id, value) = entry?;
                let value = value.value();
                let custodian = Callsign::from_bytes(value[..6].try_into().expect("6 bytes"))
                    .map_err(|e| Error::Corrupt(e.to_string()))?;
                out.push(CustodyOutcome {
                    id: ObjectId(id.value()),
                    custodian,
                    expected_at: u64::from_be_bytes(value[6..14].try_into().expect("8 bytes")),
                    delivered_at: u64::from_be_bytes(value[14..].try_into().expect("8 bytes")),
                });
            }
            for outcome in &out {
                table.remove(outcome.id.0)?;
            }
        }
        tx.commit()?;
        Ok(out)
    }
}
