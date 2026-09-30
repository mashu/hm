//! Transfers under way, how they end, and what each ending teaches.

use hm_model::{Bearer, Beliefs, CustodianObservation, Forecast, LinkKey, LinkObservation};
use hm_route::Route;
use hm_store::Retry;
use hm_wire::{Callsign, ObjectId};
use hm_xfer::Failure;

use super::custody::{enqueue_custody_fail, holds_until_expiry, on_gave_up_receipt, retry_policy_for};
use super::Node;
use crate::{log, short, Transfer};

/// A transfer under way to `peer` over `bearer`.
pub(crate) struct InFlight {
    pub bearer: Bearer,
    pub peer: Callsign,
    pub route: Route,
    pub object_bytes: u64,
    /// The route's chance beyond its first hop, and the best other way's:
    /// what the custody suspect time is decided from.
    pub downstream: f64,
    pub alternative: f64,
    pub expires_at: u64,
    /// The chance the beliefs gave the first hop, to check against how the
    /// handoff ends; none for a transfer not planned as a route.
    pub forecast: Option<Forecast>,
}

impl InFlight {
    /// A transfer not planned as a route (a bulletin, a requested copy).
    pub fn direct(bearer: Bearer, peer: Callsign, object_bytes: u64, now: u64) -> InFlight {
        InFlight {
            bearer,
            peer,
            route: Route {
                hops: Vec::new(),
                arrival: now,
                airtime_millis: 0,
                success_probability: 1.0,
                first_hop_probability: 1.0,
                risk_cost: 0.0,
                attempt_cost: 0.0,
                utility: 0.0,
            },
            object_bytes,
            downstream: 1.0,
            alternative: 0.0,
            expires_at: u64::MAX,
            forecast: None,
        }
    }
}

/// How a handoff ended, and so what it says about the link and the custodian.
pub(crate) enum Outcome {
    /// Custody taken, with a verified receipt.
    Delivered,
    /// The handoff failed for a reason of the link (no answer, lost session).
    LinkFailed(String),
    /// The custodian said no; permanent refusals are not retried.
    Refused { reason: String, permanent: bool },
    /// The custodian is busy for `retry_after` seconds.
    Busy { reason: String, retry_after: u64 },
    /// The link carried the object and the answer, but the receipt could not
    /// be verified: custody stays here.
    Unverified(String),
    /// Our side failed (the radio went down, a bad object): no news about
    /// the link or the custodian.
    Local { reason: String, permanent: bool },
}

impl Outcome {
    /// How an internet or modem transfer ended.
    pub fn from_transfer(result: Transfer) -> Outcome {
        match result {
            Transfer::Delivered => Outcome::Delivered,
            Transfer::Busy { retry_after, reason } => Outcome::Busy {
                reason: format!("busy for {retry_after} s: {reason}"),
                retry_after,
            },
            Transfer::Refused { reason, permanent } => Outcome::Refused {
                reason: format!("refused: {reason}"),
                permanent,
            },
            Transfer::Failed(reason) => Outcome::LinkFailed(reason),
        }
    }

    pub fn from_radio(reason: Failure) -> Outcome {
        let text = format!("{reason:?}");
        match reason {
            Failure::NoAnswer => Outcome::LinkFailed(text),
            Failure::Refused | Failure::TooLarge => Outcome::Refused {
                reason: text,
                permanent: false,
            },
            Failure::Empty | Failure::SelfAddressed => Outcome::Local {
                reason: text,
                permanent: true,
            },
        }
    }

    /// Why it did not deliver, and whether that is final; `None` if it did.
    fn failure(self) -> Option<(String, bool)> {
        match self {
            Outcome::Delivered => None,
            Outcome::LinkFailed(reason) | Outcome::Unverified(reason) | Outcome::Busy { reason, .. } => {
                Some((reason, false))
            }
            Outcome::Refused { reason, permanent } | Outcome::Local { reason, permanent } => {
                Some((reason, permanent))
            }
        }
    }
}

/// What a flight's outcome tells the beliefs: the link's part and the
/// custodian's, each only where it bears, and whether the link carried it as
/// often as the chance given for it said (a refusal or a "busy" came back
/// over the link: carried).
pub(crate) fn learn(beliefs: &mut Beliefs, me: Callsign, flight: &InFlight, outcome: &Outcome, now: u64) {
    let link = LinkKey {
        from: me,
        to: flight.peer,
        bearer: flight.bearer,
    };
    let carried = match outcome {
        Outcome::Delivered | Outcome::Refused { .. } | Outcome::Busy { .. } | Outcome::Unverified(_) => {
            Some(true)
        }
        Outcome::LinkFailed(_) => Some(false),
        Outcome::Local { .. } => None,
    };
    if let (Some(forecast), Some(carried)) = (flight.forecast, carried) {
        beliefs.observe_forecast(forecast, carried, now);
    }
    match outcome {
        Outcome::Delivered => {
            beliefs.observe_link(link, now, LinkObservation::Handoff { ok: true });
            beliefs.observe_custodian(flight.peer, now, CustodianObservation::Accepted);
        }
        Outcome::LinkFailed(_) => beliefs.observe_link(link, now, LinkObservation::Handoff { ok: false }),
        Outcome::Refused { .. } => {
            beliefs.observe_link(link, now, LinkObservation::Heard);
            beliefs.observe_custodian(flight.peer, now, CustodianObservation::Refused);
        }
        Outcome::Busy { retry_after, .. } => {
            beliefs.observe_link(link, now, LinkObservation::Heard);
            beliefs.observe_custodian(
                flight.peer,
                now,
                CustodianObservation::Busy {
                    retry_after: *retry_after,
                },
            );
        }
        Outcome::Unverified(_) => beliefs.observe_link(link, now, LinkObservation::Heard),
        Outcome::Local { .. } => {}
    }
}

impl Node {
    /// A transfer of `id` ended with `outcome`: learn from it, then hand
    /// custody over, or schedule the next try.
    pub(super) fn finish(&mut self, now: u64, id: ObjectId, flight: InFlight, outcome: Outcome) {
        let peer = flight.peer;
        let bearer = flight.bearer;
        learn(&mut self.beliefs, self.id.me, &flight, &outcome, now);
        let held = flight.route.contacts();
        let Some((reason, permanent)) = outcome.failure() else {
            // The first hop's room was used; the rest was only held.
            match flight.route.hops.first() {
                Some(first) if !first.forecast => {
                    if let Err(error) = self.graph.consume(first.contact, flight.object_bytes) {
                        log(format!("route capacity: {error}"));
                    }
                    self.graph.release_many(&held[1..], flight.object_bytes);
                }
                _ => self.graph.release_many(&held, flight.object_bytes),
            }
            self.custody_taken(now, id, &flight);
            self.notify.send("message");
            return;
        };
        self.graph.release_many(&held, flight.object_bytes);
        let store = &self.store;
        if let Err(error) = store.clear_next_hop(id, peer) {
            log(format!("store: {error}"));
        }
        let policy = retry_policy_for(store, id, &self.settings);
        let r = if permanent {
            store.abandon(id, &reason).map(|notify| (Retry::GaveUp, notify))
        } else {
            let hold = holds_until_expiry(store, id, &self.settings);
            store.attempt_failed_or_hold(id, &format!("{reason} ({})", bearer.name()), policy, now, hold)
        };
        match r {
            Ok((Retry::At(t), _)) => log(format!(
                "{} to {peer} by {} failed: {reason}; next try in {} s",
                short(&id),
                bearer.name(),
                t.saturating_sub(now)
            )),
            Ok((Retry::GaveUp, notify)) => {
                log(format!("gave up on {} to {peer}: {reason}", short(&id)));
                on_gave_up_receipt(store, id, now);
                if let Some(prior) = notify {
                    enqueue_custody_fail(store, self.id.me, &self.id.identity, id, prior, &reason, now);
                }
            }
            Ok((Retry::Inactive, _)) => {}
            Err(e) => log(format!("store: {e}")),
        }
        self.notify.send("message");
    }

    pub(super) fn net_done(&mut self, now: u64, id: ObjectId, peer: Callsign, result: Transfer) {
        let bulletin = self
            .store
            .record(id)
            .ok()
            .flatten()
            .is_some_and(|r| r.final_destination() == hm_xfer::broadcast_peer());
        let Some(flight) = self.in_flight.remove(&(id, peer)) else {
            return;
        };
        let outcome = Outcome::from_transfer(result);
        if bulletin || matches!(outcome, Outcome::Delivered) {
            self.control.clear_request(id, peer);
        }
        if bulletin {
            self.bulletin_pushed(now, id, flight, outcome);
        } else {
            self.finish(now, id, flight, outcome);
        }
    }

    pub(super) fn modem_done(&mut self, now: u64, id: ObjectId, peer: Callsign, result: Transfer) {
        let outcome = Outcome::from_transfer(result);
        if let Some(flight) = self.in_flight.remove(&(id, peer)) {
            if matches!(outcome, Outcome::Delivered) {
                self.control.clear_request(id, peer);
            }
            self.finish(now, id, flight, outcome);
        }
    }

    /// A bulletin pushed to an internet peer: no custody, it is published.
    fn bulletin_pushed(&mut self, now: u64, id: ObjectId, flight: InFlight, outcome: Outcome) {
        let peer = flight.peer;
        self.graph
            .release_many(&flight.route.contacts(), flight.object_bytes);
        learn(&mut self.beliefs, self.id.me, &flight, &outcome, now);
        match outcome.failure() {
            None => match self.store.delivered(id, false, "internet", now) {
                Ok(()) => log(format!(
                    "published bulletin {} to {peer} over the internet",
                    short(&id)
                )),
                Err(e) => log(format!("store: {e}")),
            },
            Some((reason, _)) => {
                log(format!("bulletin {} to {peer} failed: {reason}", short(&id)));
                // Retry only when nothing else is still carrying this id.
                if !self.in_flight.keys().any(|(flight_id, _)| *flight_id == id) {
                    match self.store.attempt_failed(
                        id,
                        &format!("{reason} (internet)"),
                        self.settings.retry,
                        now,
                    ) {
                        Ok((Retry::At(t), _)) => log(format!(
                            "bulletin {}; next try in {} s",
                            short(&id),
                            t.saturating_sub(now)
                        )),
                        Ok((Retry::GaveUp, _)) => log(format!("gave up on bulletin {}", short(&id))),
                        Ok((Retry::Inactive, _)) => {}
                        Err(err) => log(format!("store: {err}")),
                    }
                }
            }
        }
        self.notify.send("message");
        self.notify.send("status");
    }
}
