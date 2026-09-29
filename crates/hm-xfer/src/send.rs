//! Outgoing transfers: the queue, starting them, and choosing which over goes next.

use crate::receipt::object_id;
use crate::symbols::{fit_symbol, oti};
use crate::{Event, Failure, Receipt, Xfer, MAX_ACTIVE, SYMBOL_ALIGNMENT};
use alloc::vec::Vec;
use hm_core::{Millis, Output};
use hm_wire::{Callsign, ObjectId};
use raptorq::{EncodingPacket, SourceBlockEncoder};

pub(crate) struct Pending {
    pub(crate) to: Callsign,
    pub(crate) object: Vec<u8>,
    pub(crate) precedence: u8,
    /// RF bulletin: frames use [`Dest::Broadcast`], no ACK.
    pub(crate) broadcast: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum OutState {
    /// Send the next over at or after this time.
    Ready { at: Millis },
    /// Over sent; waiting for the ACK until this time.
    Waiting { until: Millis },
}

pub(crate) struct Outgoing {
    pub(crate) to: Callsign,
    pub(crate) session: u16,
    pub(crate) id: ObjectId,
    pub(crate) len: u32,
    /// Symbol size of this transfer.
    pub(crate) t: u16,
    pub(crate) precedence: u8,
    pub(crate) encoder: SourceBlockEncoder,
    pub(crate) source: Vec<EncodingPacket>,
    pub(crate) k: u32,
    pub(crate) next_esi: u32,
    /// Overs sent (saturating).
    pub(crate) rounds: u8,
    /// Overs in a row with no progress: counted when an over goes out, cleared
    /// by an ACK that asks for fewer symbols.
    pub(crate) stalls: u8,
    pub(crate) offer_next: bool,
    /// The next over starts with our OPEN (the peer has none from us lately).
    pub(crate) open_next: bool,
    /// Receiver's last reported deficit (K before any ACK).
    pub(crate) need: u32,
    /// Next over is a short probe after a missing ACK.
    pub(crate) probe: bool,
    /// Missing ACKs in a row; drives the random backoff.
    pub(crate) timeouts: u32,
    /// Airtime of the last over, the unit of the backoff.
    pub(crate) last_cost: Millis,
    pub(crate) sent_last_round: u32,
    /// The last over carried an OPEN, so the answer may carry one too.
    pub(crate) opened: bool,
    /// RF bulletin publish: no ACK; listeners may ask for repair.
    pub(crate) broadcast: bool,
    /// Broadcast: repair overs sent, and the most symbols a listener asked
    /// for since the last over.
    pub(crate) repairs: u8,
    pub(crate) asked: u32,
    /// Broadcast: repair windows in a row that brought no request. Two
    /// requests sent at once collide unheard, so one quiet window is not
    /// proof that nobody is missing anything.
    pub(crate) quiet_windows: u8,
    /// Broadcast: nothing more to send; report it done.
    pub(crate) finished: bool,
    /// How long to wait for the ACK once our last over has left the air: the
    /// peer's guard and answer, our slack and a random part.
    pub(crate) ack_wait: Millis,
    pub(crate) state: OutState,
}

impl Xfer {
    pub(crate) fn enqueue(
        &mut self,
        now: Millis,
        to: Callsign,
        object: Vec<u8>,
        precedence: u8,
        broadcast: bool,
        out: &mut Vec<Output<Event>>,
    ) {
        let id = object_id(&object);
        let reason = if !broadcast && to == self.cfg.me {
            Some(Failure::SelfAddressed)
        } else if object.is_empty() {
            Some(Failure::Empty)
        } else if object.len() as u64 > self.cfg.max_object_len as u64
            || self
                .check_params(object.len() as u32, self.cfg.symbol_size as usize)
                .is_none()
        {
            Some(Failure::TooLarge)
        } else {
            None
        };
        if let Some(reason) = reason {
            out.push(Output::Event(Event::Failed { to, id, reason }));
            return;
        }
        // Stable: after everything of equal or higher precedence.
        let pos = self
            .queue
            .iter()
            .position(|p| p.precedence < precedence)
            .unwrap_or(self.queue.len());
        self.queue.insert(
            pos,
            Pending {
                to,
                object,
                precedence,
                broadcast,
            },
        );
        let _ = now;
    }

    /// Start queued transfers, highest precedence first, for peers that have
    /// none under way, while there is room.
    pub(crate) fn start_next(&mut self, now: Millis, out: &mut Vec<Output<Event>>) {
        while self.active.len() < MAX_ACTIVE {
            let busy = |p: &Pending| {
                self.active
                    .iter()
                    .any(|o| o.to == p.to && o.broadcast == p.broadcast)
            };
            let Some(i) = self.queue.iter().position(|p| !busy(p)) else {
                return;
            };
            let p = self.queue.remove(i).expect("index from position");
            self.start(now, p, out);
        }
    }

    pub(crate) fn start(&mut self, now: Millis, p: Pending, out: &mut Vec<Output<Event>>) {
        let len = p.object.len() as u32;
        // What the peer told us it takes: larger objects fail at once, and
        // symbols are no larger than it accepts. Broadcast has no peer OPEN.
        let mut t = self.cfg.symbol_size;
        if !p.broadcast {
            if let Some((open, _)) = self.peers.get(&p.to).filter(|_| self.knows(p.to, now)) {
                if len > open.max_object {
                    out.push(Output::Event(Event::Failed {
                        to: p.to,
                        id: object_id(&p.object),
                        reason: Failure::TooLarge,
                    }));
                    return;
                }
                let theirs = open.max_symbol - open.max_symbol % SYMBOL_ALIGNMENT;
                if theirs >= SYMBOL_ALIGNMENT {
                    t = t.min(theirs);
                }
            }
        }
        let (t, k) = match self.check_params(len, fit_symbol(len, t) as usize) {
            Some(tk) => tk,
            // Too many symbols at the peer's size: ours was checked when queued.
            None => self
                .check_params(len, fit_symbol(len, self.cfg.symbol_size) as usize)
                .expect("checked when queued"),
        };
        // The block encoder wants whole symbols; the decoder truncates to `len` again.
        let mut padded = p.object;
        padded.resize(k as usize * t as usize, 0);
        let encoder = SourceBlockEncoder::new(0, &oti(len, t), &padded);
        padded.truncate(len as usize);
        let id = object_id(&padded);
        let source = encoder.source_packets();
        let session = self.new_session();
        self.active.push(Outgoing {
            to: p.to,
            session,
            id,
            len,
            t,
            precedence: p.precedence,
            encoder,
            source,
            k,
            next_esi: 0,
            rounds: 0,
            stalls: 0,
            offer_next: true,
            open_next: !p.broadcast && self.cfg.sessions && !self.knows(p.to, now),
            need: k,
            probe: false,
            timeouts: 0,
            last_cost: Millis::ZERO,
            sent_last_round: 0,
            opened: false,
            broadcast: p.broadcast,
            repairs: 0,
            asked: 0,
            quiet_windows: 0,
            finished: false,
            ack_wait: Millis::ZERO,
            state: OutState::Ready { at: now },
        });
    }

    /// A random session id: never 0, which a CLOSE uses for "every transfer",
    /// and not one of ours already under way.
    pub(crate) fn new_session(&mut self) -> u16 {
        loop {
            let s = self.rng.next_u64() as u16;
            if s != 0 && self.active.iter().all(|o| o.session != s) {
                return s;
            }
        }
    }

    /// One of our overs is waiting for its ACK: nothing else goes out, since
    /// the answer comes back on the same channel.
    pub(crate) fn waiting(&self) -> bool {
        self.active
            .iter()
            .any(|o| matches!(o.state, OutState::Waiting { .. }))
    }

    /// The transfer whose over goes next among those due now: highest
    /// precedence first, then the one due longest.
    pub(crate) fn next_due(&self, now: Millis) -> Option<usize> {
        self.active
            .iter()
            .enumerate()
            .filter_map(|(i, o)| match o.state {
                OutState::Ready { at } if at <= now => Some((i, o.precedence, at)),
                _ => None,
            })
            .min_by_key(|&(i, precedence, at)| (core::cmp::Reverse(precedence), at, i))
            .map(|(i, _, _)| i)
    }

    /// Start the next over if one is due and the channel is ours.
    pub(crate) fn pump(&mut self, now: Millis, out: &mut Vec<Output<Event>>) {
        self.start_next(now, out);
        if self.receiving() || self.waiting() {
            return; // a peer's over, or the answer to ours, is still to come
        }
        // Transfers out of rounds end before anything more is sent.
        let before = self.active.len();
        let (max_rounds, mut ended) = (self.cfg.max_rounds, Vec::new());
        self.active.retain(|o| {
            let due = matches!(o.state, OutState::Ready { at } if at <= now);
            let done = due
                && if o.broadcast {
                    o.finished
                } else {
                    o.stalls >= max_rounds
                };
            if done {
                ended.push(if o.broadcast {
                    Event::Delivered {
                        to: o.to,
                        id: o.id,
                        rounds: o.rounds,
                        receipt: Receipt::Unverified,
                    }
                } else {
                    Event::Failed {
                        to: o.to,
                        id: o.id,
                        reason: Failure::NoAnswer,
                    }
                });
            }
            !done
        });
        out.extend(ended.into_iter().map(Output::Event));
        if self.active.len() < before {
            return self.pump(now, out);
        }
        while let Some(i) = self.next_due(now) {
            if self.send_over(now, i, out) {
                return;
            }
        }
    }
}
