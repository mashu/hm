//! Airtime: the transmitter's budget, what an over costs, and how busy others keep the channel.

use crate::send::OutState;
use crate::{Xfer, MAX_REMAINING_TRUSTED, NACK_SPREAD_ACKS, OCCUPANCY_EVERY};
use hm_core::Millis;
use hm_model::{ChannelObservation, OverCost};
use hm_wire::{DataPreamble, Dest, FrameHeader, FrameType, HEADER_LEN, OPEN_LEN};

impl Xfer {
    pub(crate) fn refill(&mut self, now: Millis) {
        let elapsed = now.saturating_sub(self.tokens_at).0 as i64;
        let gained = elapsed * self.cfg.duty_cycle_permille as i64 / 1000;
        self.tokens_ms = (self.tokens_ms + gained).min(self.cfg.bucket.0 as i64);
        self.tokens_at = self.tokens_at.max(now);
    }

    /// When `cost` ms of airtime will be affordable. A full bucket always is,
    /// so a single frame longer than the bucket cannot block forever.
    pub(crate) fn affordable_at(&self, now: Millis, cost: Millis) -> Millis {
        let short = cost.0.min(self.cfg.bucket.0) as i64 - self.tokens_ms;
        if short <= 0 || self.cfg.duty_cycle_permille >= 1000 {
            now
        } else {
            now + Millis((short as u64 * 1000).div_ceil(self.cfg.duty_cycle_permille as u64))
        }
    }

    // ---- sending --------------------------------------------------------

    /// How long a broadcaster listens for repair requests after an over: the
    /// listeners' guard, the spread of their moments, one request, our slack.
    pub(crate) fn repair_wait(&self) -> Millis {
        let ack_air = self.ack_air();
        self.cfg.ack_guard + Millis(ack_air.0 * NACK_SPREAD_ACKS) + ack_air + self.cfg.ack_guard
    }

    /// Airtime of an OPEN frame.
    pub(crate) fn open_air(&self) -> Millis {
        self.cfg.air(1, HEADER_LEN + OPEN_LEN)
    }

    /// Airtime of an ACK with a receipt, key-up included.
    /// What an over costs besides its DATA frames: our key-up, the peer's
    /// key-up and ACK, the guard between (for a broadcast, the repair window
    /// listeners ask in), and the wait for the others on the channel to
    /// finish, whose overs are taken to be as long as a full one of ours.
    pub(crate) fn over_cost(&self, now: Millis, frame_air: Millis, cap: u32, broadcast: bool) -> OverCost {
        let ack = self.ack_air().0 as f64;
        let answer = if broadcast {
            (NACK_SPREAD_ACKS + 1) as f64 * ack
        } else {
            ack
        };
        let full_over = (self.cfg.txdelay.0 + frame_air.0 * u64::from(cap)) as f64;
        let wait = self.channel.access_wait(now.0 / 1_000, full_over);
        OverCost {
            frame_ms: frame_air.0 as f64,
            turnaround_ms: 2.0 * self.cfg.txdelay.0 as f64 + self.cfg.ack_guard.0 as f64 + answer + wait,
        }
    }

    /// Take the airtime of others' frames heard lately into the channel belief.
    pub(crate) fn note_occupancy(&mut self, now: Millis) {
        if now < self.busy_since + OCCUPANCY_EVERY {
            return;
        }
        self.channel.observe(
            now.0 / 1_000,
            ChannelObservation::Occupancy {
                busy_ms: self.busy_ms,
                total_ms: now.0 - self.busy_since.0,
            },
        );
        self.busy_ms = 0;
        self.busy_since = now;
    }

    pub(crate) fn ack_air(&self) -> Millis {
        self.cfg.txdelay + self.cfg.air(1, HEADER_LEN + 7 + 8 + hm_wire::RECEIPT_LEN)
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
