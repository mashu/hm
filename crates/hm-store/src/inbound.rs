//! Messages received for this station, and the receipts that answer them.

use hm_wire::{Callsign, ObjectId};
use redb::ReadableTable;

use crate::{decode, encode, Error, Result, Store};
use crate::{Direction, State, BY_TIME, MESSAGES, META, OBJECTS, QUEUE};

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

impl Store {
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
            let known = match messages.get(received.id.0)? {
                Some(bytes) => Some(decode(bytes.value())?),
                None => None,
            };
            inserted = known.is_none();
            // A message received again was sent again: its sender has not
            // seen the receipt. Answer again, unless the receipt is still on
            // its way.
            let answering = match known.as_ref().and_then(|record| record.e2e_receipt) {
                Some(receipt) => match messages.get(receipt.0)? {
                    Some(bytes) => {
                        let receipt = decode(bytes.value())?;
                        !matches!(receipt.state, State::Queued | State::InTransit)
                    }
                    None => true,
                },
                None => true,
            };
            if inserted || answering {
                let mut record = match known {
                    Some(record) => record,
                    None => {
                        let seq = Store::next_seq(&mut tx.open_table(META)?)?;
                        let mut record =
                            Store::blank_record(received.id, Direction::In, received.from, now, seq);
                        record.state = State::Unread;
                        record.verified = received.verified;
                        record.wire_seq = received.wire_seq;
                        tx.open_table(OBJECTS)?.insert(received.id.0, received.object)?;
                        tx.open_table(BY_TIME)?
                            .insert((0_u8, now, seq, received.id.0), ())?;
                        record
                    }
                };
                record.e2e_receipt = Some(reply.id);
                messages.insert(received.id.0, encode(&record).as_slice())?;
            }
            if answering && messages.get(reply.id.0)?.is_none() {
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

    pub fn mark_read(&self, id: ObjectId) -> Result<()> {
        self.update(id, |r| {
            if r.state == State::Unread {
                r.state = State::Read;
            }
        })
    }
}
