//! Airtime: the transmitter's budget, what frames and overs cost on air, and
//! the wait for a channel others keep busy.

use alloc::vec::Vec;

use crate::send::OutState;
use crate::{Event, Xfer, MAX_REMAINING_TRUSTED, NACK_SPREAD_ACKS};
use hm_core::{Millis, Output};
use hm_model::OverCost;
use hm_wire::{DataPreamble, Dest, FrameHeader, FrameType, HEADER_LEN, OPEN_LEN};

impl Xfer {
    pub(crate) fn refill(&mut self, now: Millis) {
        let elapsed = now.saturating_sub(self.tokens_at).0 as i64;
        let gained = elapsed * self.cfg.duty_cycle_permille as i64 / 1000;
        self.tokens_ms = (self.tokens_ms + gained).min(self.cfg.bucket.0 as i64);
        self.tokens_at = self.tokens_at.max(now);
    }

    /// Airtime spent at `now` on frames of ours (the key-up included when
    /// the transmitter was idle): taken from the bucket, which refills only
    /// once the transmitter has fallen silent again, as a final amplifier
    /// cools only then. So what the bucket holds bounds a key-up, not only
    /// the airtime over a stretch of time.
    pub(crate) fn charge(&mut self, now: Millis, airtime: Millis) {
        self.refill(now);
        self.tokens_ms -= airtime.0 as i64;
        self.tokens_at = now.max(self.tokens_at) + airtime;
    }

    /// An answer (an ACK, a CLOSE, our OPEN before them) goes at once,
    /// whatever the bucket holds: the station waiting for it would give up.
    /// It is taken from the bucket all the same.
    pub(crate) fn answer(&mut self, now: Millis, frame: Vec<u8>, out: &mut Vec<Output<Event>>) {
        let keyup = if now >= self.tokens_at {
            self.cfg.txdelay
        } else {
            Millis::ZERO
        };
        self.charge(now, keyup + self.cfg.air(1, frame.len()));
        self.transmit(frame, out);
    }

    /// When `cost` ms of airtime will be affordable: once what the bucket
    /// holds covers it, counting refills from when the transmitter falls
    /// silent. A full bucket always is, so a single frame longer than the
    /// bucket cannot block forever.
    pub(crate) fn affordable_at(&self, now: Millis, cost: Millis) -> Millis {
        let short = cost.0.min(self.cfg.bucket.0) as i64 - self.tokens_ms;
        if short <= 0 || self.cfg.duty_cycle_permille >= 1000 {
            now
        } else {
            now.max(self.tokens_at)
                + Millis((short as u64 * 1000).div_ceil(self.cfg.duty_cycle_permille as u64))
        }
    }

    /// How long a broadcaster listens for repair requests after an over: the
    /// listeners' guard, the spread of their moments, one request, our slack.
    pub(crate) fn repair_wait(&self) -> Millis {
        let ack_air = self.ack_air();
        self.cfg.ack_guard + Millis(ack_air.0 * NACK_SPREAD_ACKS) + ack_air + self.cfg.ack_guard
    }

    /// Airtime of a probe after a missed ACK: key-up, OFFER (and OPEN) and
    /// `symbols` DATA frames of `symbol_size`.
    pub(crate) fn probe_air(&self, symbol_size: u16, symbols: u32, open: bool) -> Millis {
        let mut air = self.cfg.txdelay
            + self.cfg.air(1, HEADER_LEN + hm_wire::OFFER_LEN)
            + self.cfg.air(
                symbols as usize,
                self.cfg.data_frame_len(usize::from(symbol_size)),
            );
        if open {
            air += self.open_air();
        }
        air
    }

    /// Airtime of an OPEN frame.
    pub(crate) fn open_air(&self) -> Millis {
        self.cfg.air(1, HEADER_LEN + OPEN_LEN)
    }

    /// Airtime of an ACK with a receipt, key-up included.
    pub(crate) fn ack_air(&self) -> Millis {
        self.cfg.txdelay + self.cfg.air(1, HEADER_LEN + 7 + 8 + hm_wire::RECEIPT_LEN)
    }

    /// What an over costs besides its DATA frames: our key-up, the peer's
    /// key-up and ACK, the guard between (for a broadcast, the repair window
    /// listeners ask in), and the wait for the others on the channel to
    /// finish, whose overs are taken to be as long as a full one of ours.
    pub(crate) fn over_cost(&self, frame_air: Millis, cap: u32, broadcast: bool) -> OverCost {
        let ack = self.ack_air().0 as f64;
        let answer = if broadcast {
            (NACK_SPREAD_ACKS + 1) as f64 * ack
        } else {
            ack
        };
        let full_over = (self.cfg.txdelay.0 + frame_air.0 * u64::from(cap)) as f64;
        let wait = hm_model::access_wait(self.channel_busy, full_over);
        OverCost {
            frame_ms: frame_air.0 as f64,
            turnaround_ms: 2.0 * self.cfg.txdelay.0 as f64 + self.cfg.ack_guard.0 as f64 + answer + wait,
        }
    }

    /// While we wait for an ACK, other traffic on the channel means our over
    /// may not have gone out yet: the link waits for a clear channel before
    /// keying up, and we are not told when it does. Wait as if the over starts
    /// when that traffic ends. If it had gone out already, this costs at most
    /// one over's time, while the channel is busy anyway.
    pub(crate) fn hear_traffic(&mut self, now: Millis, h: &FrameHeader, payload: &[u8]) {
        let traffic_end = match (h.ftype, DataPreamble::decode(payload)) {
            (FrameType::Data, Ok((pre, symbol))) => {
                let remaining = pre.remaining.min(MAX_REMAINING_TRUSTED) as usize;
                now + self.cfg.air(remaining, self.cfg.data_frame_len(symbol.len()))
            }
            _ => now,
        };
        let ack_air = self.ack_air();
        let guard = self.cfg.ack_guard;
        let me = Dest::Station(self.cfg.me);
        for o in &mut self.active {
            if h.src == o.to && h.dst == me {
                continue; // our peer answering us
            }
            if let OutState::Waiting { until } = o.state {
                let later = traffic_end + guard + o.last_cost + guard + ack_air + guard;
                o.state = OutState::Waiting {
                    until: until.max(later),
                };
            }
        }
    }
}
