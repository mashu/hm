//! Radio events: the link up and down, objects received and sent, beacons
//! heard and beacons due but not heard.

use hm_model::{Bearer, LinkKey, LinkObservation, MISS_HORIZON};
use hm_route::BeaconObservation;
use hm_wire::{Callsign, ObjectId};
use hm_xfer::Receipt;

use super::custody::wake_for;
use super::handoff::Outcome;
use super::{Command, Node};
use crate::accept::{accept, Acceptance, AcceptanceGate};
use crate::control::{live_window_secs, radio_pull_due};
use crate::{heard, log, short, RadioCmd, RadioEvt};

/// A busy node asks radio senders to come back after this many seconds.
const BUSY_RETRY_SECS: u16 = 60;

impl Node {
    pub(super) fn radio_event(&mut self, now: u64, event: RadioEvt, out: &mut Vec<Command>) {
        match event {
            RadioEvt::Using(via) => {
                if via != self.radio_via {
                    match (&via, &self.radio_via) {
                        (Some(desc), _) => log(format!("radio now {desc}")),
                        (None, Some(_)) => log("radio now off"),
                        (None, None) => {}
                    }
                }
                self.radio_via = via;
            }
            RadioEvt::Up => {
                self.radio_up = true;
                self.last_down = None;
                // A new radio session starts without our holding flag.
                self.holding_sent = None;
                self.next_holding_check = 0;
                log("radio up");
            }
            RadioEvt::BeaconInterval(secs) => {
                self.live_window = live_window_secs(secs);
                self.graph.set_live_contact_secs(self.live_window);
                self.beacon_interval = secs;
            }
            RadioEvt::Down(why) => self.radio_down(now, why),
            RadioEvt::Received {
                from,
                xfer_id,
                object,
            } => {
                let acceptance = accept(
                    AcceptanceGate {
                        store: &self.store,
                        notify: &self.notify,
                        trust: &self.settings.trust,
                        me: self.id.me,
                        key_call: self.id.key_call,
                        identity: &self.id.identity,
                        relay: &self.settings.relay,
                        now,
                    },
                    from,
                    &object,
                    None,
                );
                let retry_after = if matches!(&acceptance, Acceptance::Busy(_)) {
                    BUSY_RETRY_SECS
                } else {
                    0
                };
                out.push(Command::Radio(RadioCmd::Accept {
                    from,
                    xfer_id,
                    accepted: acceptance.custody_accepted(),
                    retry_after,
                }));
            }
            RadioEvt::Sync { from, payload } => {
                self.receive_sync(now, Bearer::Radio, from, &payload, None, out)
            }
            RadioEvt::Heard(list) => self.heard(now, list, out),
            RadioEvt::Delivered { xfer_id, to, receipt } => self.radio_delivered(now, xfer_id, to, receipt),
            RadioEvt::Failed { xfer_id, to, reason } => {
                if let Some(id) = self.radio_ids.remove(&(xfer_id, to)) {
                    if let Some(flight) = self.in_flight.remove(&(id, to)) {
                        self.finish(now, id, flight, Outcome::from_radio(reason));
                    }
                }
            }
            RadioEvt::Over { to, sent, got } => {
                let link = LinkKey {
                    from: self.id.me,
                    to,
                    bearer: Bearer::Radio,
                };
                self.beliefs
                    .observe_link(link, now, LinkObservation::Over { sent, got });
            }
        }
    }

    /// Our radio went down: what it was carrying is tried again, and nothing
    /// is learned about the links, which did nothing wrong.
    fn radio_down(&mut self, now: u64, why: String) {
        self.radio_up = false;
        // Retries fail the same way every few seconds: say it once. A radio
        // switched off is not news.
        if self.radio_via.is_some() && self.last_down.as_deref() != Some(why.as_str()) {
            log(format!("radio down: {why}"));
            self.last_down = Some(why);
        }
        let lost: Vec<(ObjectId, Callsign)> = self
            .in_flight
            .iter()
            .filter(|(_, flight)| flight.bearer == Bearer::Radio)
            .map(|(key, _)| *key)
            .collect();
        self.radio_ids.clear();
        for key in lost {
            let flight = self.in_flight.remove(&key).expect("listed");
            self.finish(
                now,
                key.0,
                flight,
                Outcome::Local {
                    reason: "radio went down".into(),
                    permanent: false,
                },
            );
        }
    }

    fn radio_delivered(&mut self, now: u64, xfer_id: ObjectId, to: Callsign, receipt: Receipt) {
        let Some(id) = self.radio_ids.remove(&(xfer_id, to)) else {
            return;
        };
        if to == hm_xfer::broadcast_peer() {
            // Bulletin publish: no custody receipt expected.
            if let Some(flight) = self.in_flight.remove(&(id, to)) {
                for hop in &flight.route.hops {
                    let _ = self.graph.release(hop.contact, flight.object_bytes);
                }
            }
            match self.store.delivered(id, false, "radio", now) {
                Ok(()) => log(format!("published bulletin {}", short(&id))),
                Err(e) => log(format!("store: {e}")),
            }
            self.notify.send("message");
        } else if let Some(flight) = self.in_flight.remove(&(id, to)) {
            let outcome = match receipt {
                Receipt::Verified => {
                    self.control.clear_request(id, to);
                    Outcome::Delivered
                }
                Receipt::Unverified => Outcome::Unverified(format!(
                    "custody receipt from {to} is not verified; retaining custody"
                )),
            };
            self.finish(now, id, flight, outcome);
        }
    }

    /// The table of stations heard, with each one's latest beacon: take each
    /// beacon in once, as a contact and as news about links; count beacons
    /// due and not heard; pull holdings from stations that have some.
    fn heard(&mut self, now: u64, list: Vec<heard::Station>, out: &mut Vec<Command>) {
        let rate = self.settings.radio_bitrate;
        let capacity = (u64::from(rate) * 600 / 8).max(4_096);
        let me = self.id.me;
        let radio = |from, to| LinkKey {
            from,
            to,
            bearer: Bearer::Radio,
        };
        let mut on_air = Vec::new();
        for station in &list {
            let Some(beacon) = &station.beacon else {
                continue;
            };
            // The table lists each station's latest beacon for a day, and
            // comes again every half minute: take each beacon once, and claim
            // a live contact only while one was heard lately.
            if beacon.key != heard::KeyCheck::Trusted
                || self.beacons_seen.get(&station.call) == Some(&beacon.at)
                || now.saturating_sub(beacon.at) >= self.live_window
            {
                continue;
            }
            self.beacons_seen.insert(station.call, beacon.at);
            wake_for(&self.store, station.call, "heard", now);
            let observation = BeaconObservation {
                origin: station.call,
                receiver: me,
                heard: &beacon.heard,
                rate_bps: rate,
                capacity_bytes: capacity,
                flags: beacon.flags,
                observed_at: beacon.at,
            };
            // The beacon came through from its origin to us; its signed list
            // says which links into its origin were open, and when.
            self.beliefs
                .observe_link(radio(station.call, me), beacon.at, LinkObservation::Beacon);
            for (hearing, at) in observation.hearings() {
                self.beliefs
                    .observe_link(radio(hearing, station.call), at, LinkObservation::Reported);
                on_air.push((hearing, at));
            }
            let success = self.beliefs.link_success(radio(me, station.call), now, now);
            self.claim_live(station.call, Bearer::Radio, success, rate, capacity, now);
            if let Err(error) = self.graph.observe_beacon(observation) {
                log(format!("ignored beacon contact from {}: {error}", station.call));
            }
            // Pull only from a station whose beacon says it holds something.
            let sync_key = (station.call, Bearer::Radio);
            let last = self.last_pairwise_sync.get(&sync_key).copied();
            if radio_pull_due(beacon.flags, last, now) {
                self.send_filters(station.call, Bearer::Radio, now, out);
            }
        }
        // Beacons due from stations heard before, and not heard: closed
        // links, or open ones that lost them.
        if self.beacon_interval > 0 {
            let listened: Vec<Callsign> = self
                .beliefs
                .links()
                .map(|(key, _)| *key)
                .filter(|key| key.touches(me) && key.bearer == Bearer::Radio)
                .map(|key| if key.from == me { key.to } else { key.from })
                .collect();
            for station in listened {
                self.beliefs
                    .note_silence(radio(station, me), now, self.beacon_interval);
            }
        }
        self.unheard(&on_air);
        self.heard = list;
        self.notify.send("status");
    }

    /// Stations our neighbours hear and we do not: each transmission of
    /// theirs that a neighbour reports was on the air, and did not reach us.
    /// For a station heard lately, the beacons due from it count instead
    /// ([`hm_model::Beliefs::note_silence`]); this is the evidence about stations never
    /// heard here, or not for longer than that counts.
    fn unheard(&mut self, on_air: &[(Callsign, u64)]) {
        let me = self.id.me;
        // Two neighbours reporting one beacon are one miss, not two.
        let apart = (self.beacon_interval / 2).max(60);
        for &(station, at) in on_air {
            if station == me {
                continue;
            }
            let key = LinkKey {
                from: station,
                to: me,
                bearer: Bearer::Radio,
            };
            let followed = self
                .beliefs
                .link(key)
                .and_then(|link| link.last_open())
                .is_some_and(|open| open.saturating_add(MISS_HORIZON) >= at);
            let counted = self.unheard_until.get(&station).copied();
            if followed || counted.is_some_and(|last| at < last.saturating_add(apart)) {
                continue;
            }
            self.unheard_until.insert(station, at);
            self.beliefs.observe_link(key, at, LinkObservation::Missed);
        }
    }
}
