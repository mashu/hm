//! Custody: handed over, confirmed end to end, or reclaimed when no receipt
//! comes; custody failure notices; our holdings flag.

use hm_bundle::{Bundle, Kind, Opened, Precedence};
use hm_ident::Identity;
use hm_model::{CustodianObservation, HandedOver};
use hm_store::{Direction, ReclaimOutcome, RetryPolicy, Store};
use hm_wire::{Callsign, ObjectId};

use super::handoff::InFlight;
use super::{Command, Node};
use crate::{log, short, RadioCmd, Settings};

/// Check this often whether we hold anything others may pull (our beacons say so).
const HOLDING_CHECK_SECS: u64 = 30;
/// The custody suspect timer never fires sooner than this after a handoff.
const MIN_SUSPECT_SECS: u64 = 60;

impl Node {
    /// `flight`'s custodian took custody with a verified receipt. Wait for
    /// the end-to-end receipt as long as waiting pays: resending sooner risks
    /// a duplicate, later a lost message found out too late. The final
    /// destination itself cannot lose what it holds: from there, resending
    /// never pays. A receipt or a custody-fail notice, which nothing answers,
    /// is the custodian's from here.
    pub(super) fn custody_taken(&mut self, now: u64, id: ObjectId, flight: &InFlight) {
        let peer = flight.peer;
        let at_destination = self
            .store
            .record(id)
            .ok()
            .flatten()
            .is_some_and(|r| r.final_destination() == peer);
        let eta = flight.route.arrival.max(now);
        let handed = HandedOver {
            downstream: flight.downstream,
            alternative: if at_destination { 0.0 } else { flight.alternative },
            // A delivered message's worth in copies: what sending one along
            // the route is expected to cost.
            value: 1.0 / flight.route.attempt_cost.max(1.0e-6),
            expected_secs: eta - now,
            remaining_secs: flight.expires_at.saturating_sub(now),
        };
        let suspect_secs = self.beliefs.suspect_after(
            peer,
            &handed,
            (MIN_SUSPECT_SECS, self.settings.custody_suspect_secs),
            now,
        );
        match self.store.custody_transferred(
            id,
            hm_store::CustodyHandoff {
                next_hop: peer,
                receipt_verified: true,
                by: flight.bearer.name(),
                now,
                grace_secs: self.settings.custody_grace_secs,
                suspect_secs,
                eta,
                answered: answered_end_to_end(&self.store, id),
            },
        ) {
            Ok(true) => {
                self.handed.insert(id, flight.downstream);
                log(format!(
                    "custody of {} transferred to {peer} by {}; receipt expected within {} s",
                    short(&id),
                    flight.bearer.name(),
                    suspect_secs
                ))
            }
            Ok(false) => log(format!(
                "ignored late custody receipt for {} from {peer}",
                short(&id)
            )),
            Err(e) => log(format!("store: {e}")),
        }
    }

    /// End-to-end receipts for messages a custodian took: how late past the
    /// planned arrival, and that the custodian did its part.
    pub(super) fn take_custody_outcomes(&mut self) {
        match self.store.take_custody_outcomes() {
            Ok(outcomes) => {
                for outcome in outcomes {
                    self.handed.remove(&outcome.id);
                    self.beliefs.observe_custodian(
                        outcome.custodian,
                        outcome.delivered_at,
                        CustodianObservation::Delivered {
                            late_secs: outcome.delivered_at.saturating_sub(outcome.expected_at),
                        },
                    );
                }
            }
            Err(error) => log(format!("store: {error}")),
        }
    }

    /// Custody whose end-to-end receipt did not come in time is taken back.
    pub(super) fn reclaim_suspects(&mut self, now: u64) {
        let suspects = match self.store.suspect_due(now) {
            Ok(suspects) => suspects,
            Err(error) => {
                log(format!("store: {error}"));
                return;
            }
        };
        for record in suspects {
            if self.in_flight.keys().any(|(id, _)| *id == record.id) {
                continue;
            }
            // Our own message went to a custodian and no receipt came back in
            // all that time, by any path: the custodian takes its share of
            // the blame, weighed against the rest of the route's chance and
            // the chance the receipt is only late, so a station that takes
            // custody and drops it stops attracting traffic. The link that
            // carried the handoff did its part and is not blamed.
            if record.direction == Direction::Out {
                if let Some(handed) = record.handed() {
                    let downstream = self.handed.remove(&record.id).unwrap_or(0.5);
                    self.beliefs.observe_custodian(
                        handed.custodian,
                        now,
                        CustodianObservation::Silent {
                            downstream,
                            late_secs: now.saturating_sub(handed.eta),
                        },
                    );
                }
            }
            match self
                .store
                .reclaim_custody(record.id, now, "custody suspect; reclaiming")
            {
                Ok(ReclaimOutcome::Requeued) => {
                    log(format!("reclaimed {} (custody suspect)", short(&record.id)));
                    self.notify.send("message");
                }
                Ok(ReclaimOutcome::DeliveredUnconfirmed) => {
                    log(format!(
                        "{} delivered-unconfirmed after custody suspect",
                        short(&record.id)
                    ));
                    self.notify.send("message");
                }
                Ok(ReclaimOutcome::Failed) => {
                    log(format!("{} failed after custody suspect", short(&record.id)));
                    if record.direction == Direction::Relay {
                        if let Some(prior) = record.custody_from {
                            enqueue_custody_fail(
                                &self.store,
                                self.id.me,
                                &self.id.identity,
                                record.id,
                                prior,
                                "custody suspect; expired",
                                now,
                            );
                        }
                    }
                    self.notify.send("message");
                }
                Ok(ReclaimOutcome::Ignored) => {}
                Err(error) => log(format!("store: {error}")),
            }
        }
    }

    /// Tell the radio whether we hold anything others may pull.
    pub(super) fn holding_check(&mut self, now: u64, out: &mut Vec<Command>) {
        if now < self.next_holding_check {
            return;
        }
        self.next_holding_check = now.saturating_add(HOLDING_CHECK_SECS);
        match self.store.holds_for_others(now) {
            Ok(holding) if self.holding_sent != Some(holding) => {
                out.push(Command::Radio(RadioCmd::Holding(holding)));
                self.holding_sent = Some(holding);
            }
            Ok(_) => {}
            Err(error) => log(format!("store: {error}")),
        }
    }
}

/// Tell `prior`, who handed us `holding`, that we could not deliver it.
pub(super) fn enqueue_custody_fail(
    store: &Store,
    me: Callsign,
    identity: &Identity,
    holding: ObjectId,
    prior: Callsign,
    reason: &str,
    now: u64,
) {
    let ttl = 7 * 24 * 3600u32;
    let sealed = match Bundle::custody_fail(me, prior, holding, reason, now, ttl)
        .with_precedence(Precedence::Priority)
        .seal(identity)
    {
        Ok(sealed) => sealed,
        Err(error) => {
            log(format!(
                "cannot build custody-fail for {}: {error}",
                short(&holding)
            ));
            return;
        }
    };
    let bytes = sealed.to_vec();
    match store.enqueue_with(
        sealed.id(),
        &bytes,
        hm_store::EnqueueOpts {
            to: prior,
            precedence: Precedence::Priority.rank(),
            now,
            wire_seq: None,
            expires_at: Some(now.saturating_add(u64::from(ttl))),
        },
    ) {
        Ok(true) => log(format!("queued custody-fail for {} to {prior}", short(&holding))),
        Ok(false) => {}
        Err(error) => log(format!("store: {error}")),
    }
}

/// An end-to-end receipt we could not deliver: its message counts as
/// delivered, unconfirmed.
pub(super) fn on_gave_up_receipt(store: &Store, receipt_id: ObjectId, now: u64) {
    let Ok(Some(object)) = store.object(receipt_id) else {
        return;
    };
    let Ok(opened) = Opened::decode(&object) else {
        return;
    };
    if opened.bundle.kind != Kind::Receipt {
        return;
    }
    let Some(original) = opened.bundle.reply_to else {
        return;
    };
    match store.delivered_unconfirmed(original, "end-to-end receipt could not be delivered", now) {
        Ok(true) => log(format!(
            "{} marked delivered-unconfirmed (receipt gave up)",
            short(&original)
        )),
        Ok(false) => {}
        Err(error) => log(format!("store: {error}")),
    }
}

/// The kind of bundle `id` holds, if it can be read.
fn kind_of(store: &Store, id: ObjectId) -> Option<Kind> {
    let object = store.object(id).ok()??;
    Opened::decode(&object).ok().map(|opened| opened.bundle.kind)
}

/// Whether an end-to-end receipt will answer `id`: nothing answers a receipt
/// or a custody-fail notice.
fn answered_end_to_end(store: &Store, id: ObjectId) -> bool {
    !matches!(kind_of(store, id), Some(Kind::Receipt | Kind::CustodyFail))
}

pub(super) fn retry_policy_for(store: &Store, id: ObjectId, settings: &Settings) -> RetryPolicy {
    if kind_of(store, id) == Some(Kind::Receipt) {
        settings.receipt_retry
    } else {
        settings.retry
    }
}

/// Whether a message that has used up its retries is held until its bundle
/// expires rather than given up: our own messages, and holdings of a mailbox
/// relay, whose job is to wait for a destination that is rarely in reach.
/// A plain relay gives up so the custodian before it can try another path.
pub(super) fn holds_until_expiry(store: &Store, id: ObjectId, settings: &Settings) -> bool {
    match store.record(id) {
        Ok(Some(record)) => match record.direction {
            Direction::Out => true,
            Direction::Relay => settings.relay.mailbox,
            _ => false,
        },
        _ => false,
    }
}

/// `station` is in reach: try its queued messages now rather than at their
/// next scheduled retry, which may be an hour away.
pub(super) fn wake_for(store: &Store, station: Callsign, how: &str, now: u64) {
    match store.wake(station, now) {
        Ok(0) => {}
        Ok(woken) => log(format!("{station} {how}: trying {woken} queued message(s) now")),
        Err(error) => log(format!("store: {error}")),
    }
}
