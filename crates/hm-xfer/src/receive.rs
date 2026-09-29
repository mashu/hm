//! Incoming transfers: OFFERs and symbols in, decoding, completion and the application's verdict.

use crate::receipt::{object_id, receipt_statement};
use crate::symbols::oti;
use crate::{Event, Xfer};
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use hm_core::{Millis, Output};
use hm_wire::{Callsign, Close, CloseReason, DataPreamble, ObjectId};
use raptorq::{Decoder, EncodingPacket, PayloadId};

pub(crate) struct Incoming {
    pub(crate) id: Option<ObjectId>,
    pub(crate) len: u32,
    pub(crate) symbol_size: u16,
    pub(crate) k: u32,
    pub(crate) decoder: Decoder,
    pub(crate) esis: BTreeSet<u32>,
    pub(crate) decoded: Option<Vec<u8>>,
    pub(crate) done: bool,
    pub(crate) awaiting_application: bool,
    /// Our signature proving completion, sent in every final ACK.
    pub(crate) receipt: Option<[u8; 64]>,
    pub(crate) ack_at: Option<Millis>,
    pub(crate) last_heard: Millis,
    /// Heard on `Dest::Broadcast`: store locally, never ACK; ask for repair.
    pub(crate) broadcast: bool,
    /// Other listeners' repair requests heard since the last over that ask
    /// for at least as much as ours would.
    pub(crate) covering_nacks: u8,
    /// Repair requests we may still repeat if no repair follows ours.
    pub(crate) nack_retries: u8,
}

impl Incoming {
    pub(crate) fn new(len: u32, symbol_size: u16, k: u32, now: Millis, broadcast: bool) -> Incoming {
        Incoming {
            id: None,
            len,
            symbol_size,
            k,
            decoder: Decoder::new(oti(len, symbol_size)),
            esis: BTreeSet::new(),
            decoded: None,
            done: false,
            awaiting_application: false,
            receipt: None,
            ack_at: None,
            last_heard: now,
            broadcast,
            covering_nacks: 0,
            nack_retries: 0,
        }
    }

    pub(crate) fn reset_decoder(&mut self) {
        self.decoder = Decoder::new(oti(self.len, self.symbol_size));
        self.esis.clear();
        self.decoded = None;
    }

    /// Eviction order when slots run out: least valuable first.
    pub(crate) fn value(&self) -> (bool, bool, usize, Millis) {
        (self.done, self.id.is_some(), self.esis.len(), self.last_heard)
    }
}

impl Xfer {
    pub(crate) fn receiving(&self) -> bool {
        self.incoming.values().any(|i| i.ack_at.is_some() && !i.broadcast)
    }

    /// Whether a new transfer from `sender` would have to push out work in
    /// progress from other stations: every slot holds a live, offered,
    /// unfinished transfer from someone else. Then we ask it to come back later.
    pub(crate) fn busy_for(&self, now: Millis, sender: Callsign) -> bool {
        let idle = self.cfg.idle_timeout;
        let live = |i: &Incoming| !i.done && i.id.is_some() && i.last_heard + idle > now;
        let others = self.incoming.iter().filter(|((c, _), _)| *c != sender);
        let from_sender = self.incoming.keys().filter(|(c, _)| *c == sender).count();
        from_sender < self.cfg.max_incoming_per_sender
            && self.incoming.len() >= self.cfg.max_incoming
            && others.clone().count() == self.incoming.len()
            && others.map(|(_, i)| i).all(live)
    }

    /// Make room for a new transfer from `sender`. A sender at its own limit
    /// loses its least valuable transfer; otherwise, when all slots are taken, the
    /// least valuable transfer overall goes. Value: finished, then OFFER seen, then
    /// symbols collected, then most recently heard.
    pub(crate) fn make_room(&mut self, now: Millis, sender: Callsign) {
        let idle = self.cfg.idle_timeout;
        self.incoming.retain(|_, i| i.done || i.last_heard + idle > now);
        let from_sender = self.incoming.keys().filter(|(c, _)| *c == sender).count();
        let victim = if from_sender >= self.cfg.max_incoming_per_sender {
            self.incoming
                .iter()
                .filter(|((c, _), _)| *c == sender)
                .min_by_key(|(_, i)| i.value())
        } else if self.incoming.len() >= self.cfg.max_incoming {
            self.incoming.iter().min_by_key(|(_, i)| i.value())
        } else {
            None
        };
        if let Some(k) = victim.map(|(k, _)| *k) {
            self.incoming.remove(&k);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn slot(
        &mut self,
        now: Millis,
        key: (Callsign, u16),
        len: u32,
        t: u16,
        k: u32,
        authoritative: bool,
        broadcast: bool,
    ) -> bool {
        match self.incoming.get(&key) {
            Some(i) if i.len == len && i.symbol_size == t => {
                if broadcast {
                    self.incoming.get_mut(&key).expect("checked").broadcast = true;
                }
                return true;
            }
            Some(_) if !authoritative => return false,
            Some(_) => {}
            None => self.make_room(now, key.0),
        }
        self.incoming
            .insert(key, Incoming::new(len, t, k, now, broadcast));
        true
    }

    pub(crate) fn on_offer(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        payload: &[u8],
        broadcast: bool,
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok(offer) = hm_wire::Offer::decode(payload) else {
            return;
        };
        let key = (from, session);
        let answer = self.ack_at(now, offer.remaining, offer.symbol_size as usize);
        if offer.object_len > self.cfg.max_object_len {
            if !broadcast {
                let close = Close {
                    reason: CloseReason::TooLarge,
                    retry_after: 0,
                };
                self.closes.insert(key, (close, answer));
            }
            return;
        }
        let Some((t, k)) = self.check_params(offer.object_len, offer.symbol_size as usize) else {
            return;
        };
        let id = ObjectId(offer.hash);
        if !self.incoming.contains_key(&key) && !self.seen.contains_key(&id) && self.busy_for(now, from) {
            if !broadcast {
                let close = Close {
                    reason: CloseReason::Busy,
                    retry_after: self.cfg.busy_retry_secs,
                };
                self.closes.insert(key, (close, answer));
            }
            return;
        }
        if self
            .incoming
            .get(&key)
            .is_some_and(|i| i.id.is_some_and(|known| known != id))
        {
            self.incoming.remove(&key); // a new object on a reused session
        }
        if !self.slot(now, key, offer.object_len, t, k, true, broadcast) {
            return;
        }
        let already = self.seen.contains_key(&id);
        let receipt = self.sign_receipt(key, &id);
        let ack_at = if broadcast {
            let done = already || self.incoming.get(&key).is_some_and(|i| i.done);
            if done {
                None
            } else {
                self.nack_at(now, offer.remaining, offer.symbol_size as usize)
            }
        } else {
            Some(self.ack_at(now, offer.remaining, t as usize))
        };
        let inc = self.incoming.get_mut(&key).expect("slot ensured");
        inc.id = Some(id);
        inc.last_heard = now;
        inc.broadcast = broadcast;
        // Broadcast listeners never ACK (would storm the channel).
        inc.ack_at = ack_at;
        if already {
            inc.done = true;
            if !broadcast {
                inc.receipt = Some(receipt);
            }
        } else if let Some(data) = inc.decoded.take() {
            self.finish(now, key, data, out);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_data(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        esi: u32,
        payload: &[u8],
        broadcast: bool,
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok((pre, symbol)) = DataPreamble::decode(payload) else {
            return;
        };
        let Some((t, k)) = self.check_params(pre.object_len, symbol.len()) else {
            return;
        };
        let key = (from, session);
        // Every DATA frame repeats the object length: if it fits after all, the
        // OFFER that seemed too large was corrupted on the way. Take back the CLOSE.
        if let Some((close, _)) = self.closes.get(&key) {
            if close.reason == CloseReason::TooLarge && pre.object_len <= self.cfg.max_object_len {
                self.closes.remove(&key);
            }
        }
        // Symbols for a transfer we are turning away, or would have to make
        // room for by dropping others' work, are not collected.
        if self.closes.contains_key(&key) || (!self.incoming.contains_key(&key) && self.busy_for(now, from)) {
            return;
        }
        if !self.slot(now, key, pre.object_len, t, k, false, broadcast) {
            return;
        }
        let ack_at = if broadcast {
            if self.incoming.get(&key).is_some_and(|i| i.done) {
                None
            } else {
                self.nack_at(now, pre.remaining, t as usize)
            }
        } else {
            Some(self.ack_at(now, pre.remaining, t as usize))
        };
        let inc = self.incoming.get_mut(&key).expect("slot ensured");
        inc.last_heard = now;
        if broadcast {
            inc.broadcast = true;
            // A new over: requests heard after it are counted afresh.
            inc.covering_nacks = 0;
            inc.nack_retries = 1;
        }
        inc.ack_at = ack_at;
        if inc.done || inc.awaiting_application || inc.decoded.is_some() || !inc.esis.insert(esi) {
            return;
        }
        let packet = EncodingPacket::new(PayloadId::new(0, esi), symbol.to_vec());
        if let Some(data) = inc.decoder.decode(packet) {
            if inc.id.is_some() {
                self.finish(now, key, data, out);
            } else {
                inc.decoded = Some(data); // hold until the OFFER tells us the hash
            }
        }
    }

    pub(crate) fn sign_receipt(&self, key: (Callsign, u16), id: &ObjectId) -> [u8; 64] {
        self.identity
            .sign(&receipt_statement(self.cfg.me, key.0, key.1, id))
    }

    /// Check a decoded object against the offered hash and deliver it.
    pub(crate) fn finish(
        &mut self,
        now: Millis,
        key: (Callsign, u16),
        data: Vec<u8>,
        out: &mut Vec<Output<Event>>,
    ) {
        let id = self.incoming[&key].id.expect("caller checked");
        if object_id(&data) != id {
            // A bad symbol got through; start over.
            self.incoming
                .get_mut(&key)
                .expect("caller holds the key")
                .reset_decoder();
            return;
        }
        if self.application_ack {
            let inc = self.incoming.get_mut(&key).expect("caller holds the key");
            inc.awaiting_application = true;
            inc.last_heard = now;
            out.push(Output::Event(Event::Received {
                from: key.0,
                id,
                object: data,
            }));
        } else {
            self.complete_incoming(now, key, id);
            out.push(Output::Event(Event::Received {
                from: key.0,
                id,
                object: data,
            }));
        }
    }

    pub(crate) fn complete_incoming(&mut self, now: Millis, key: (Callsign, u16), id: ObjectId) {
        let broadcast = self.incoming[&key].broadcast;
        let receipt = if broadcast {
            None
        } else {
            Some(self.sign_receipt(key, &id))
        };
        let inc = self.incoming.get_mut(&key).expect("caller holds the key");
        inc.receipt = receipt;
        inc.done = true;
        inc.awaiting_application = false;
        inc.last_heard = now;
        if broadcast {
            inc.ack_at = None;
        }
        self.seen.insert(id, now + self.cfg.done_ttl);
    }

    pub(crate) fn application_verdict(
        &mut self,
        now: Millis,
        from: Callsign,
        id: ObjectId,
        accepted: bool,
        retry_after: u16,
    ) {
        let key = self
            .incoming
            .iter()
            .find(|((peer, _), incoming)| {
                *peer == from && incoming.id == Some(id) && incoming.awaiting_application
            })
            .map(|(key, _)| *key);
        let Some(key) = key else { return };
        if accepted {
            let broadcast = self.incoming[&key].broadcast;
            self.complete_incoming(now, key, id);
            if !broadcast {
                self.incoming.get_mut(&key).expect("completed above").ack_at = Some(now);
            }
            return;
        }
        self.incoming.remove(&key);
        self.closes.insert(
            key,
            (
                Close {
                    reason: if retry_after == 0 {
                        CloseReason::Refused
                    } else {
                        CloseReason::Busy
                    },
                    retry_after,
                },
                now,
            ),
        );
    }

    pub(crate) fn expire(&mut self, now: Millis) {
        let (idle, ttl) = (self.cfg.idle_timeout, self.cfg.done_ttl);
        self.incoming
            .retain(|_, i| i.last_heard + if i.done { ttl } else { idle } > now);
        self.seen.retain(|_, until| *until > now);
    }
}
