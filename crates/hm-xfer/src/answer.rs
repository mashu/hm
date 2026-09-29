//! Answers to overs: ACKs when the sender's over ends, and repair requests for broadcasts.

use crate::receive::Incoming;
use crate::{Event, Xfer, MAX_REMAINING_TRUSTED, NACK_SPREAD_ACKS, NACK_SUPPRESS_AFTER};
use alloc::vec::Vec;
use hm_core::{Millis, Output};
use hm_wire::{Ack, Callsign, Close, FrameType, ObjectId, NEED_OFFER};

impl Xfer {
    pub(crate) fn ack_at(&self, now: Millis, remaining: u8, symbol_size: usize) -> Millis {
        let remaining = remaining.min(MAX_REMAINING_TRUSTED) as usize;
        now + self.cfg.air(remaining, self.cfg.data_frame_len(symbol_size)) + self.cfg.ack_guard
    }

    /// When to ask for more of an unfinished broadcast: after the over, at a
    /// random moment, so the first request heard silences the others. None
    /// when this station does not ask for repairs.
    pub(crate) fn nack_at(&mut self, now: Millis, remaining: u8, symbol_size: usize) -> Option<Millis> {
        if self.cfg.broadcast_repairs == 0 {
            return None;
        }
        let spread = self.ack_air().0 * NACK_SPREAD_ACKS;
        Some(self.ack_at(now, remaining, symbol_size) + Millis(self.rng.below(spread + 1)))
    }

    /// Symbols an unfinished broadcast still lacks, as a repair request says.
    pub(crate) fn broadcast_need(inc: &Incoming) -> u32 {
        let have = inc.esis.len() as u32;
        if have < inc.k {
            inc.k - have
        } else {
            1
        }
    }

    /// Another listener asked the broadcaster `to` for more of `session`: if
    /// it asked for at least as much as we would, the answer covers us too.
    pub(crate) fn overhear_nack(&mut self, to: Callsign, session: u16, payload: &[u8]) {
        let Ok(ack) = Ack::decode(payload) else { return };
        let Some(inc) = self.incoming.get_mut(&(to, session)) else {
            return;
        };
        if !inc.broadcast || inc.done || inc.ack_at.is_none() {
            return;
        }
        let asked = if ack.need == NEED_OFFER {
            1
        } else {
            u32::from(ack.need)
        };
        if asked >= Self::broadcast_need(inc) {
            inc.covering_nacks = inc.covering_nacks.saturating_add(1);
            if inc.covering_nacks >= NACK_SUPPRESS_AFTER {
                inc.ack_at = None;
            }
        }
    }

    /// We answer once every over we are hearing has ended, so an ACK never
    /// talks over another station's over.
    pub(crate) fn answer_at(&self) -> Option<Millis> {
        let acks = self
            .incoming
            .values()
            .filter(|i| !i.broadcast)
            .filter_map(|i| i.ack_at);
        acks.chain(self.closes.values().map(|(_, at)| *at)).max()
    }

    /// The next moment we ask a broadcaster for more (independent of our
    /// answers to unicast senders, which must not wait for it).
    pub(crate) fn nack_due(&self) -> Option<Millis> {
        self.incoming
            .values()
            .filter(|i| i.broadcast)
            .filter_map(|i| i.ack_at)
            .min()
    }

    pub(crate) fn send_due_acks(&mut self, now: Millis, out: &mut Vec<Output<Event>>) {
        let repair_wait = self.repair_wait();
        let nacks: Vec<(Callsign, u16)> = self
            .incoming
            .iter()
            .filter(|(_, i)| i.broadcast && i.ack_at.is_some_and(|at| at <= now))
            .map(|(k, _)| *k)
            .collect();
        for key in nacks {
            let inc = self.incoming.get_mut(&key).expect("collected above");
            inc.ack_at = None;
            if inc.done {
                continue;
            }
            let need = Self::broadcast_need(inc);
            // If no repair follows, ask once more a window later.
            if inc.nack_retries > 0 {
                inc.nack_retries -= 1;
                inc.ack_at = Some(now + repair_wait);
            }
            let ack = Ack {
                need: need.min(NEED_OFFER as u32 - 1) as u16,
                ..Ack::default()
            };
            let payload = ack.to_vec().expect("fields in range");
            let f = self.frame(FrameType::Ack, key.0, key.1, 0, &payload, false);
            self.transmit(f, out);
        }
        if self.answer_at().is_none_or(|t| t > now) {
            return;
        }
        let due: Vec<(Callsign, u16)> = self
            .incoming
            .iter()
            .filter(|(_, i)| i.ack_at.is_some() && !i.broadcast)
            .map(|(k, _)| *k)
            .collect();

        let closes: Vec<((Callsign, u16), Close)> = core::mem::take(&mut self.closes)
            .into_iter()
            .map(|(k, (c, _))| (k, c))
            .collect();
        for (key, close) in closes {
            // Our OPEN first, so the sender learns the limit it ran into.
            if self.open_replies.remove(&key.0) {
                let ours = self.our_open(true).to_bytes().expect("in range");
                let f = self.frame(FrameType::Ctrl, key.0, key.1, 0, &ours, false);
                self.transmit(f, out);
            }
            let f = self.frame(FrameType::Ctrl, key.0, key.1, 0, &close.to_bytes(), false);
            self.transmit(f, out);
        }
        for key in due {
            if self.open_replies.remove(&key.0) {
                let ours = self.our_open(true).to_bytes().expect("in range");
                let f = self.frame(FrameType::Ctrl, key.0, key.1, 0, &ours, false);
                self.transmit(f, out);
            }
            let inc = self.incoming.get_mut(&key).expect("collected above");
            inc.ack_at = None;
            let ack = if inc.done {
                Ack {
                    need: 0,
                    completed: inc.id.iter().map(ObjectId::prefix8).collect(),
                    receipt: inc.receipt,
                    ..Ack::default()
                }
            } else if inc.id.is_none() {
                Ack {
                    need: NEED_OFFER,
                    ..Ack::default()
                }
            } else {
                let have = inc.esis.len() as u32;
                let need = if have < inc.k { inc.k - have } else { 1 };
                Ack {
                    need: need.min(NEED_OFFER as u32 - 1) as u16,
                    ..Ack::default()
                }
            };
            let payload = ack.to_vec().expect("fields in range");
            let f = self.frame(FrameType::Ack, key.0, key.1, 0, &payload, false);
            self.transmit(f, out);
        }
    }
}
