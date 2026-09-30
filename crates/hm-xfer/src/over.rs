//! One over and its answer: what goes in it, what the ACK says, what silence means.

use crate::receipt::{key_for, receipt_statement};
use crate::send::OutState;
use crate::{Event, Receipt, Xfer, BROADCAST_MAX_ROUNDS, MAX_BACKOFF_DOUBLINGS, MIN_WINDOW};
use alloc::vec::Vec;
use hm_core::{Millis, Output};
use hm_wire::{Ack, Callsign, DataPreamble, FrameType, Offer, HEADER_LEN, MAX_INDEX, NEED_OFFER};

impl Xfer {
    /// Send the next over of transfer `i`, or put it off until the airtime
    /// budget allows it. True when the over went out.
    pub(crate) fn send_over(&mut self, now: Millis, i: usize, out: &mut Vec<Output<Event>>) -> bool {
        let o = &self.active[i];
        let (to, session, broadcast) = (o.to, o.session, o.broadcast);
        let erasure = self.erasure(o.to);
        let window = if o.broadcast {
            self.cfg.max_burst as u32
        } else {
            self.window(o.to)
        };
        let cap = (self.cfg.max_burst as u32)
            .min(window)
            .min(MAX_INDEX - o.next_esi.min(MAX_INDEX))
            .max(1);
        let t = o.t as usize;
        let frame_air = self.cfg.air(1, self.cfg.data_frame_len(t));
        let (need, probe, is_broadcast) = (o.need, o.probe, o.broadcast);
        let over_cost = self.over_cost(frame_air, cap, is_broadcast);
        let n = if probe {
            need.clamp(1, 2).min(cap)
        } else if is_broadcast {
            hm_model::broadcast_burst(need, cap, &erasure, over_cost, self.listeners)
        } else {
            hm_model::burst_size(need, cap, &erasure, over_cost)
        };
        let o = &self.active[i];
        let mut fixed = self.cfg.txdelay;
        if o.offer_next {
            fixed += self.cfg.air(1, HEADER_LEN + hm_wire::OFFER_LEN);
        }
        let open = !o.broadcast && ((o.offer_next && o.open_next) || self.open_replies.contains(&o.to));
        if open {
            fixed += self.open_air();
        }
        // An over never lasts longer than `max_over`, nor costs more than the
        // bucket holds, but it always carries a symbol.
        let room = |limit: u64| (limit.saturating_sub(fixed.0) / frame_air.0.max(1)).max(1) as u32;
        let mut n = n.min(room(self.cfg.max_over.0));
        if self.cfg.duty_cycle_permille < 1000 {
            n = n.min(room(self.cfg.bucket.0));
        }
        let cost = fixed + Millis(frame_air.0 * n as u64);
        self.refill(now);
        let when = self.affordable_at(now, cost);
        if when > now {
            self.active[i].state = OutState::Ready { at: when };
            return false;
        }

        let mut frames = Vec::with_capacity(n as usize + 2);
        if open {
            let reply = self.open_replies.remove(&to);
            let ours = self.our_open(reply).to_bytes().expect("in range");
            frames.push(self.frame(FrameType::Ctrl, to, session, 0, &ours, false));
        }
        let o = &self.active[i];
        if o.offer_next {
            let offer = Offer {
                hash: o.id.0,
                object_len: o.len,
                symbol_size: t as u16,
                precedence: o.precedence,
                remaining: n as u8,
            };
            frames.push(self.frame(
                FrameType::Ctrl,
                o.to,
                o.session,
                0,
                &offer.to_bytes().expect("len checked"),
                broadcast,
            ));
        }
        for j in 0..n {
            let esi = o.next_esi + j;
            let pre = DataPreamble {
                object_len: o.len,
                remaining: (n - 1 - j) as u8,
            };
            let mut payload = pre.to_bytes().expect("len checked").to_vec();
            payload.extend_from_slice(&Self::symbol(o, esi));
            frames.push(self.frame(FrameType::Data, o.to, o.session, esi, &payload, broadcast));
        }
        for f in frames {
            self.transmit(f, out);
        }
        self.tokens_ms -= cost.0 as i64;

        // The peer answers after our over: its guard, its key-up, a full ACK
        // (and its OPEN, if we sent ours), our slack.
        let ack_air = self.ack_air() + if open { self.open_air() } else { Millis::ZERO };
        let jitter = Millis(self.rng.below(self.cfg.ack_guard.0 + 1));
        let ack_wait = self.cfg.ack_guard + ack_air + self.cfg.ack_guard + jitter;
        let o = &mut self.active[i];
        o.next_esi += n;
        o.rounds = o.rounds.saturating_add(1);
        o.stalls = o.stalls.saturating_add(1);
        o.offer_next = false;
        o.open_next = false;
        o.opened = open;
        o.probe = false;
        o.last_cost = cost;
        o.sent_last_round = n;
        if !o.broadcast {
            o.ack_wait = ack_wait;
            o.state = OutState::Waiting {
                until: now + cost + ack_wait,
            };
            return true;
        }
        // A broadcast has no ACK. Once enough source symbols went out (or the
        // round cap is reached) it listens for listeners asking for more, and
        // answers with fresh symbols: any new symbol helps every listener.
        if o.next_esi < o.k && o.rounds < BROADCAST_MAX_ROUNDS {
            o.state = OutState::Ready { at: now + cost };
        } else if o.repairs < self.cfg.broadcast_repairs {
            let repair_wait = self.repair_wait();
            let o = &mut self.active[i];
            o.asked = 0;
            o.ack_wait = repair_wait;
            o.state = OutState::Waiting {
                until: now + cost + repair_wait,
            };
        } else {
            let o = self.active.remove(i);
            out.push(Output::Event(Event::Delivered {
                to: o.to,
                id: o.id,
                rounds: o.rounds,
                receipt: Receipt::Unverified,
            }));
            self.pump(now, out);
        }
        true
    }

    pub(crate) fn on_ack(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        payload: &[u8],
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok(ack) = Ack::decode(payload) else { return };
        // A listener asking for more of our broadcast (a NACK).
        if let Some(o) = self
            .active
            .iter_mut()
            .find(|o| o.broadcast && o.session == session)
        {
            let need = if ack.need == NEED_OFFER {
                1
            } else {
                u32::from(ack.need)
            };
            o.asked = o.asked.max(need);
            return;
        }
        let Some(i) = self
            .active
            .iter()
            .position(|o| !o.broadcast && o.to == from && o.session == session)
        else {
            return;
        };
        // An answer came: the channel carried our over and the reply.
        let grown = self.window(from).saturating_add(1).min(self.cfg.max_burst as u32);
        let o = &mut self.active[i];
        o.timeouts = 0;
        if ack.need == 0 || ack.completed.contains(&o.id.prefix8()) {
            let receipt = match key_for(&self.keys, o.to) {
                None => Receipt::Unverified,
                Some(key) => {
                    let statement = receipt_statement(o.to, self.cfg.me, o.session, &o.id);
                    match ack.receipt {
                        Some(sig) if key.verify(&statement, &sig).is_ok() => Receipt::Verified,
                        _ => {
                            // Forged, or signed by another key: carry on as if unheard.
                            self.rejected_receipts += 1;
                            return;
                        }
                    }
                }
            };
            let o = self.active.remove(i);
            // The last over brought at least what was needed; how many more
            // arrived is not known, so only an over that was all needed counts.
            if o.sent_last_round > 0 && o.sent_last_round <= o.need {
                self.observe_over(o.to, o.sent_last_round, o.sent_last_round, out);
            }
            self.window.insert(o.to, grown);
            out.push(Output::Event(Event::Delivered {
                to: o.to,
                id: o.id,
                rounds: o.rounds,
                receipt,
            }));
            return;
        }
        if ack.need == NEED_OFFER {
            o.offer_next = true;
        } else {
            let new_need = ack.need as u32;
            if new_need < o.need {
                o.stalls = 0; // the over got something through
            }
            let sent = o.sent_last_round;
            // Every symbol that arrives is one fewer needed.
            let got = (sent > 0 && new_need <= o.need).then(|| (o.need - new_need).min(sent));
            o.need = new_need;
            o.state = OutState::Ready { at: now };
            if let Some(got) = got {
                self.observe_over(from, sent, got, out);
            }
        }
        if let Some(o) = self.active.get_mut(i) {
            o.state = OutState::Ready { at: now };
        }
        self.window.insert(from, grown);
    }

    /// No ACK: probe again after a random, exponentially growing backoff, so
    /// stations that cannot hear each other stop colliding in lockstep, and
    /// halve the congestion window. The loss estimate stays as the ACKs left
    /// it: a missing answer may mean a busy or colliding channel, where larger
    /// overs, to make up for "loss", would only make it worse.
    pub(crate) fn on_ack_timeout(&mut self, now: Millis) {
        for i in 0..self.active.len() {
            let OutState::Waiting { until } = self.active[i].state else {
                continue;
            };
            if now < until {
                continue;
            }
            if self.active[i].broadcast {
                // The repair window closed: answer the largest request, or finish.
                let repairs = self.cfg.broadcast_repairs;
                let repair_wait = self.repair_wait();
                let o = &mut self.active[i];
                if o.asked > 0 && o.repairs < repairs {
                    o.repairs += 1;
                    o.need = o.asked;
                    o.offer_next = true;
                    o.quiet_windows = 0;
                    o.state = OutState::Ready { at: now };
                } else if o.asked == 0 && o.quiet_windows == 0 && o.repairs < repairs {
                    // Listen once more: a request may have been lost in a collision.
                    o.quiet_windows = 1;
                    o.state = OutState::Waiting {
                        until: now + repair_wait,
                    };
                } else {
                    o.finished = true;
                    o.state = OutState::Ready { at: now };
                }
                o.asked = 0;
                continue;
            }
            let to = self.active[i].to;
            let halved = (self.window(to) / 2).max(MIN_WINDOW);
            self.window.insert(to, halved);
            if !self.worth_another_over(i, now) {
                self.active[i].abandoned = true;
                self.active[i].state = OutState::Ready { at: now };
                continue;
            }
            let o = &mut self.active[i];
            o.timeouts += 1;
            let window = (o.last_cost.0 + self.cfg.ack_guard.0) << o.timeouts.min(MAX_BACKOFF_DOUBLINGS);
            let backoff = Millis(self.rng.below(window + 1));
            o.offer_next = true;
            o.open_next = o.opened;
            o.probe = true;
            o.state = OutState::Ready { at: now + backoff };
        }
    }

    /// Whether a transfer of `need` symbols of `t` bytes to `to` opens with a
    /// short probe (the OFFER and a symbol or two) rather than a full burst.
    /// A probe costs its airtime whatever happens and saves the rest of the
    /// burst when the path turns out closed; opening with the burst saves a
    /// turnaround (our key-up and the peer's ACK) when it is open. So probe
    /// when `(1 − P(open)) · (burst − probe) > P(open) · turnaround`, both in
    /// airtime. A link believed open, as an unknown one is, opens with the
    /// burst.
    pub(crate) fn opens_with_probe(&self, to: Callsign, need: u32, t: u16) -> bool {
        let belief = self.belief(to);
        let open = belief.open.p;
        if open >= 1.0 {
            return false;
        }
        let frame_air = self.cfg.air(1, self.cfg.data_frame_len(usize::from(t)));
        let cap = (self.cfg.max_burst as u32).min(self.window(to)).max(1);
        let burst = hm_model::burst_size(need, cap, &belief.erasure, self.over_cost(frame_air, cap, false));
        let probe = need.clamp(1, 2).min(burst);
        let saved = f64::from(burst - probe) * frame_air.0 as f64;
        let turnaround = (self.cfg.txdelay + self.ack_air()).0 as f64;
        (1.0 - open) * saved > open * turnaround
    }

    /// After an over that brought no answer: Bayes' rule on whether the link
    /// is open ([`Openness`](hm_model::Openness)), and whether another over
    /// is worth sending. An over is answered, if the link is open, when at
    /// least one of its frames arrives and so does the ACK, under the
    /// Beta-binomial predictive of frame loss; a short probe of an open link
    /// is rarely unanswered, so a few silent ones say the link is closed.
    ///
    /// Another over must be worth its airtime `c` (both in units of what
    /// completing the transfer is worth): `p·a ≥ c`, with `p` the chance the
    /// link is open and `a` that the over is answered if it is. And it must
    /// be worth sending now rather than stopping to send again once the peer
    /// is next heard, when the link is known open. The over's airtime is
    /// wasted if the link has closed; stopping costs, if it is still open,
    /// the wait `w` (the share of the worth the delay loses) and reopening
    /// the transfer `r` (an OPEN, a key-up, and the symbols the receiver
    /// already has, which it forgets when kept waiting). So `c·(1 − p) ≤
    /// p·(w + r)`. Mail, which loses little by waiting a beacon interval,
    /// stops once the link is about as likely closed as open; urgent traffic,
    /// which loses much, keeps trying; a transfer nearly done is not given up
    /// lightly.
    fn worth_another_over(&mut self, i: usize, now: Millis) -> bool {
        let belief = self.belief(self.active[i].to);
        let answered = |frames: u32| belief.erasure.p_at_least(frames, 1) * belief.erasure.p_at_least(1, 1);
        let o = &self.active[i];
        let sent = o.sent_last_round + u32::from(o.opened) + 1;
        let (probe, symbol_size, opened) = (o.need.clamp(1, 2), o.t, o.opened);
        let progress = o.k.saturating_sub(o.need);
        let elapsed = now.saturating_sub(o.open_at).0 as f64 / 1_000.0;
        let open = o.open.after(elapsed).unanswered(answered(sent));
        self.active[i].open = open;
        self.active[i].open_at = now;
        let per_ms = belief.airtime_cost / 1_000.0;
        let cost = per_ms * self.probe_air(symbol_size, probe, opened).0 as f64;
        let frame_air = self.cfg.air(1, self.cfg.data_frame_len(usize::from(symbol_size)));
        let reopen = self.cfg.txdelay + self.open_air() + Millis(frame_air.0 * u64::from(progress));
        let restart = per_ms * reopen.0 as f64;
        let next = open.p * answered(probe + 1 + u32::from(opened));
        next >= cost && cost * (1.0 - open.p) <= open.p * (belief.wait_cost + restart)
    }

    /// The link has put on air everything we gave it, the last frame ending at
    /// `now`. A link that waits for a clear channel may have held our over
    /// back, so the wait for its ACK starts from here, not from when we
    /// handed the over to the link.
    pub(crate) fn on_transmitted(&mut self, now: Millis) {
        for o in &mut self.active {
            if let OutState::Waiting { .. } = o.state {
                o.state = OutState::Waiting {
                    until: now + o.ack_wait,
                };
            }
        }
    }

    /// Take an ACK's count into the loss belief and report it.
    pub(crate) fn observe_over(&mut self, to: Callsign, sent: u32, got: u32, out: &mut Vec<Output<Event>>) {
        let mut belief = self.belief(to);
        belief.erasure.observe(sent, got);
        self.beliefs.insert(to, belief);
        out.push(Output::Event(Event::Over { to, sent, got }));
    }
}
