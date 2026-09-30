//! Holdings taken on for others: admission, what is held for whom, and
//! closing a holding when the destination answers.

use hm_wire::{Callsign, ObjectId};
use redb::ReadableTable;

use crate::{decode, encode, Result, Store};
use crate::{Direction, Error, State, BY_TIME, MESSAGES, META, OBJECTS, QUEUE};

/// A bulletin stored without its expiry (by older versions) is offered for
/// this long after it was queued: a bulletin's lifetime.
const BULLETIN_FALLBACK_SECS: u64 = 24 * 3600;

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

impl AdmissionLimits {
    pub fn admits(self, usage: HoldingUsage, object_bytes: usize) -> bool {
        usage.count < self.max_count && usage.bytes.saturating_add(object_bytes as u64) <= self.max_bytes
    }
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

impl Store {
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

    /// Custody of a relay holding this node had given up on is offered again:
    /// take it back on, with the new holding's metadata. False (and nothing
    /// changed) unless the record is a failed relay holding.
    pub fn revive_relay(&self, id: ObjectId, metadata: RelayMetadata<'_>, now: u64) -> Result<bool> {
        if !metadata.is_valid(now) {
            return Err(Error::Corrupt("invalid relay metadata".into()));
        }
        self.update(id, |r| {
            if r.direction != Direction::Relay
                || !(r.state == State::Failed || r.handed_on())
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

    /// A receipt signed by `destination` for relay holding `id` passed through
    /// this node: the bundle arrived, so neither send it on, nor take it on
    /// again if it is offered anew. False unless a holding for that
    /// destination still held here or handed on from here.
    pub fn relay_receipted(
        &self,
        id: ObjectId,
        receipt: ObjectId,
        destination: Callsign,
        now: u64,
    ) -> Result<bool> {
        match self.update(id, |r| {
            if r.direction != Direction::Relay
                || !(matches!(r.state, State::Queued | State::InTransit) || r.handed_on())
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

    /// Active ids to advertise to `peer` during pairwise holdings sync.
    /// Whether this station holds anything another may pull with holdings
    /// SYNC: an outbound or relayed bundle still queued or in its shadow
    /// period, or one of its own bulletins. Beacons say so ([`hm_wire::FLAG_HOLDING`]).
    pub fn holds_for_others(&self, now: u64) -> Result<bool> {
        let tx = self.read_tx()?;
        let messages = tx.open_table(MESSAGES)?;
        let bulletin_dest = Callsign::parse("ALL").expect("ALL is a valid callsign");
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
                && matches!(record.state, State::Queued | State::Delivered)
                && (record.expires_at.is_some() || record.at.saturating_add(BULLETIN_FALLBACK_SECS) > now);
            if bulletin || record.state == State::Queued || record.in_shadow(now) {
                return Ok(true);
            }
        }
        Ok(false)
    }

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
                && matches!(record.state, State::Queued | State::Delivered)
                && (record.expires_at.is_some() || record.at.saturating_add(BULLETIN_FALLBACK_SECS) > now);
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
}
