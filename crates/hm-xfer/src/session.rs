//! Sessions: our OPEN and the peers', and CLOSE when a transfer cannot be taken.

use crate::send::OutState;
use crate::{Event, Failure, Xfer, MAX_SYMBOL_SIZE};
use alloc::vec::Vec;
use hm_core::{Millis, Output};
use hm_wire::{Callsign, Close, CloseReason, Open, MAX_OBJECT_LEN, OPEN_REPLY};

impl Xfer {
    /// Our OPEN, as sent to `to`.
    pub(crate) fn our_open(&self, reply: bool) -> Open {
        Open {
            flags: if reply { OPEN_REPLY } else { 0 },
            features: self.cfg.features,
            max_object: self.cfg.max_object_len.min(MAX_OBJECT_LEN),
            max_symbol: MAX_SYMBOL_SIZE,
            max_parallel: self.cfg.max_incoming_per_sender.min(255) as u8,
        }
    }

    /// Whether `peer`'s OPEN is recent enough to rely on.
    pub(crate) fn knows(&self, peer: Callsign, now: Millis) -> bool {
        self.peers
            .get(&peer)
            .is_some_and(|(_, at)| *at + self.cfg.done_ttl > now)
    }

    pub(crate) fn on_open(&mut self, now: Millis, from: Callsign, payload: &[u8]) {
        let Ok(open) = Open::decode(payload) else { return };
        self.peers.insert(from, (open, now));
        if !open.is_reply() && self.cfg.sessions {
            self.open_replies.insert(from);
        }
    }

    pub(crate) fn on_close(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        payload: &[u8],
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok(close) = Close::decode(payload) else { return };
        if close.reason == CloseReason::Done {
            // The sender is finished with us: forget its finished transfers.
            self.incoming
                .retain(|(c, s), i| !(*c == from && (session == 0 || *s == session) && i.done));
            return;
        }
        let Some(i) = self
            .active
            .iter()
            .position(|o| !o.broadcast && o.to == from && (session == 0 || session == o.session))
        else {
            return;
        };
        let o = &mut self.active[i];
        let reason = match close.reason {
            CloseReason::Busy => {
                // Not a failure: come back when asked, starting afresh.
                let wait = Millis::from_secs(u64::from(close.retry_after.max(1)));
                o.offer_next = true;
                o.timeouts = 0;
                o.state = OutState::Ready { at: now + wait };
                return;
            }
            // Believed only when the peer's own OPEN agrees; otherwise the
            // CLOSE answered a corrupted OFFER, so offer again.
            CloseReason::TooLarge
                if self
                    .peers
                    .get(&from)
                    .is_none_or(|(open, _)| o.len <= open.max_object) =>
            {
                o.offer_next = true;
                o.open_next = true;
                o.state = OutState::Ready { at: now };
                return;
            }
            CloseReason::TooLarge => Failure::TooLarge,
            _ => Failure::Refused,
        };
        let o = self.active.remove(i);
        out.push(Output::Event(Event::Failed {
            to: o.to,
            id: o.id,
            reason,
        }));
    }
}
