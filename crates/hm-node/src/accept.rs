//! Bundle acceptance gate for local delivery and relay custody.

use hm_bundle::{Address, Bundle, Kind, Opened};
use hm_ident::{Identity, PublicKey};
use hm_store::{AdmissionLimits, Direction, QueuedMessage, ReceivedMessage, RelayMetadata, State, Store};
use hm_wire::{unwrap_routed, Callsign};

use crate::message::{open_message, Verification, MAX_BULLETIN_BYTES, MAX_INBOUND_BULLETINS_PER_ORIGIN_HOUR};
use crate::{addressed_to_us, log, short, Notify, RelaySettings, Trust};

/// What the gate did with an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acceptance {
    Stored,
    Duplicate,
    Busy(String),
    Rejected(String),
}

impl Acceptance {
    pub fn custody_accepted(&self) -> bool {
        matches!(self, Self::Stored | Self::Duplicate)
    }
}

/// What the gate decides with: the station, its store and settings, and the time.
pub struct AcceptanceGate<'a> {
    pub store: &'a Store,
    pub notify: &'a Notify,
    pub trust: &'a Trust,
    pub me: Callsign,
    pub key_call: Callsign,
    pub identity: &'a Identity,
    pub relay: &'a RelaySettings,
    /// Unix seconds.
    pub now: u64,
}

/// True when this origin already has too many inbound bulletins in the last hour.
pub fn inbound_bulletin_flood(store: &Store, origin: Callsign, now: u64) -> bool {
    let Ok(list) = store.list(Direction::In, 200) else {
        return false;
    };
    let hour_ago = now.saturating_sub(3600);
    let mut n = 0usize;
    for record in list {
        if record.at < hour_ago {
            continue;
        }
        let Ok(Some(object)) = store.object(record.id) else {
            continue;
        };
        let Ok(opened) = Opened::decode(&object) else {
            continue;
        };
        if opened.bundle.kind == Kind::Bulletin && opened.bundle.from == origin {
            n += 1;
            if n >= MAX_INBOUND_BULLETINS_PER_ORIGIN_HOUR {
                return true;
            }
        }
    }
    false
}

/// The one gate for final delivery and relay custody.
pub fn accept(
    gate: AcceptanceGate<'_>,
    via: Callsign,
    object: &[u8],
    peer_key: Option<&PublicKey>,
) -> Acceptance {
    let AcceptanceGate {
        store,
        notify,
        trust,
        me,
        key_call,
        identity,
        relay,
        now,
    } = gate;
    let routed = match unwrap_routed(object) {
        Ok(route) => route,
        Err(error) => return Acceptance::Rejected(format!("invalid routing wrapper: {error}")),
    };
    let (inner, hop_count, visited) = match &routed {
        Some(route) => (route.bundle, route.hop_count, route.visited.as_slice()),
        None => (object, 0, &[][..]),
    };
    let m = open_message(via, inner, trust, peer_key);
    let Some(bundle) = &m.bundle else {
        return Acceptance::Rejected(format!("not a bundle: {}", m.error.unwrap_or_default()));
    };
    let our_recipient = bundle.to.iter().find_map(|address| match address {
        Address::Station(station) if addressed_to_us(*station, me, key_call) => Some(*station),
        _ => None,
    });
    if m.verification == Verification::BadSignature {
        log(format!(
            "REJECTED a message claiming to be from {}: bad signature",
            bundle.from
        ));
        return Acceptance::Rejected("signature does not verify".into());
    }
    let verified = m.verification == Verification::Verified;
    if bundle.is_expired(now) {
        return Acceptance::Rejected("bundle expired".into());
    }
    if bundle.kind == Kind::Receipt {
        if let Err(error) = bundle.validate_receipt() {
            return Acceptance::Rejected(error.to_string());
        }
    }
    if bundle.kind == Kind::CustodyFail {
        if let Err(error) = bundle.validate_custody_fail() {
            return Acceptance::Rejected(error.to_string());
        }
    }
    // Bulletins are group-addressed RF posts: store locally, never receipt, never relay.
    let bulletin_group = bundle.to.iter().find_map(|address| match address {
        Address::Group(name) => Some(name.as_str()),
        _ => None,
    });
    if bundle.kind == Kind::Bulletin {
        if bulletin_group.is_none() {
            return Acceptance::Rejected("bulletin requires a group address".into());
        }
        if inner.len() > MAX_BULLETIN_BYTES {
            return Acceptance::Rejected("bulletin too large".into());
        }
        if inbound_bulletin_flood(store, bundle.from, now) {
            return Acceptance::Rejected("bulletin rate limit from this origin".into());
        }
        return match store.put_received(m.id, inner, via, verified, now) {
            Ok(true) => {
                log(format!(
                    "bulletin {} from {} via {via} group {}{}",
                    short(&m.id),
                    bundle.from,
                    bulletin_group.unwrap_or("?"),
                    if verified { "" } else { " (unverified)" }
                ));
                notify.send("message");
                Acceptance::Stored
            }
            Ok(false) => Acceptance::Duplicate,
            Err(error) => Acceptance::Rejected(format!("store: {error}")),
        };
    }
    if our_recipient.is_none() {
        if !relay.enabled && !relay.mailbox {
            return Acceptance::Rejected(format!("not addressed to {me}; relay disabled"));
        }
        if !verified {
            return Acceptance::Rejected("relay requires a verified sender".into());
        }
        let mut destinations = bundle.to.iter().filter_map(|address| match address {
            Address::Station(station) => Some(*station),
            _ => None,
        });
        let Some(destination) = destinations.next() else {
            return Acceptance::Rejected("relay requires one station recipient".into());
        };
        if destinations.next().is_some() {
            return Acceptance::Rejected("multi-recipient relay is not supported".into());
        }
        // Destination need not be in the local trust list: RF authorization is
        // re-checked at transmission time against the verified origin, and each
        // hop authenticates only its next custodian.
        let max_hops = bundle.max_hops().min(relay.max_hops);
        if hop_count >= max_hops || visited.contains(&me) {
            return Acceptance::Rejected("hop limit or routing loop".into());
        }
        // A holding this node gave up on is taken on again when custody is
        // offered anew: the sender has a reason to believe it is reachable
        // now. One this node handed on and gets back from its custodian is
        // taken back now, not when its receipt would have been overdue.
        // Anything else already held is a duplicate.
        let revive = match store.record(m.id) {
            Ok(Some(record)) if record.direction == Direction::Relay && record.state == State::Failed => true,
            Ok(Some(record)) if record.state == State::InTransit && record.custody_by == Some(via) => {
                return match store.reclaim_custody(m.id, now, &format!("handed back by {via}")) {
                    Ok(_) => {
                        log(format!("took {} back from {via}", short(&m.id)));
                        notify.send("message");
                        Acceptance::Duplicate
                    }
                    Err(error) => Acceptance::Rejected(format!("store: {error}")),
                };
            }
            Ok(Some(_)) => return Acceptance::Duplicate,
            Ok(None) => false,
            Err(error) => return Acceptance::Rejected(format!("store: {error}")),
        };
        let usage = match store.relay_usage(now) {
            Ok(usage) => usage,
            Err(error) => return Acceptance::Rejected(format!("store: {error}")),
        };
        let limits = AdmissionLimits {
            max_count: relay.max_holdings,
            max_bytes: relay.max_bytes,
        };
        if !limits.admits(usage, inner.len()) {
            return Acceptance::Busy("relay holdings limit reached".into());
        }
        let metadata = RelayMetadata {
            custody_from: via,
            destination,
            precedence: bundle.precedence().rank(),
            hop_count,
            visited,
            max_hops,
            expires_at: bundle.expires_at(),
            wire_seq: bundle.seq,
        };
        let stored = if revive {
            store.revive_relay(m.id, metadata, now)
        } else {
            store.enqueue_relay(m.id, inner, metadata, now)
        };
        let acceptance = match stored {
            Ok(true) => {
                log(format!(
                    "accepted custody of {} from {via} for {destination}{}",
                    short(&m.id),
                    if revive { " again" } else { "" }
                ));
                notify.send("message");
                Acceptance::Stored
            }
            Ok(false) => Acceptance::Duplicate,
            Err(error) => Acceptance::Rejected(format!("store: {error}")),
        };
        // A destination's receipt on its way back to the origin closes the
        // holding it answers, if it passed this way.
        if bundle.kind == Kind::Receipt && acceptance.custody_accepted() {
            if let Some(original) = bundle.reply_to {
                match store.relay_receipted(original, m.id, bundle.from, now) {
                    Ok(true) => log(format!(
                        "relayed {} delivered to {}; holding closed",
                        short(&original),
                        bundle.from
                    )),
                    Ok(false) => {}
                    Err(error) => log(format!("could not apply receipt {}: {error}", short(&m.id))),
                }
            }
        }
        return acceptance;
    }
    if bundle.kind == Kind::CustodyFail {
        let stored = match store.put_received(m.id, inner, via, verified, now) {
            Ok(stored) => stored,
            Err(error) => return Acceptance::Rejected(format!("store: {error}")),
        };
        if verified {
            if let Some(holding) = bundle.reply_to {
                let reason = bundle
                    .body
                    .as_ref()
                    .and_then(|body| body.as_text().ok())
                    .map(|text| text.into_owned())
                    .unwrap_or_else(|| "custody failed downstream".into());
                match store.apply_custody_fail(holding, bundle.from, now, &reason) {
                    Ok(hm_store::ReclaimOutcome::Requeued) => log(format!(
                        "reclaimed {} after custody-fail from {}",
                        short(&holding),
                        bundle.from
                    )),
                    Ok(hm_store::ReclaimOutcome::DeliveredUnconfirmed) => log(format!(
                        "{} marked delivered-unconfirmed after custody-fail from {}",
                        short(&holding),
                        bundle.from
                    )),
                    Ok(hm_store::ReclaimOutcome::Failed) => log(format!(
                        "{} failed after custody-fail from {}",
                        short(&holding),
                        bundle.from
                    )),
                    Ok(hm_store::ReclaimOutcome::Ignored) => {}
                    Err(error) => log(format!("could not apply custody-fail {}: {error}", short(&m.id))),
                }
            }
        }
        notify.send("message");
        return if stored {
            Acceptance::Stored
        } else {
            Acceptance::Duplicate
        };
    }
    if bundle.kind == Kind::Receipt {
        let stored = match store.put_received(m.id, inner, via, verified, now) {
            Ok(stored) => stored,
            Err(error) => return Acceptance::Rejected(format!("store: {error}")),
        };
        if verified {
            if let Some(original) = bundle.reply_to {
                match store.e2e_delivered(original, m.id, bundle.from, now) {
                    Ok(true) => log(format!(
                        "end-to-end delivery of {} confirmed by {}",
                        short(&original),
                        bundle.from
                    )),
                    Ok(false) => {}
                    Err(error) => log(format!("could not apply receipt {}: {error}", short(&m.id))),
                }
            }
        }
        notify.send("message");
        return if stored {
            Acceptance::Stored
        } else {
            Acceptance::Duplicate
        };
    }
    // Unverified final deliveries are stored but not acknowledged: a receipt
    // would be locally originated and could consume RF for an unauthorized party.
    if !verified {
        return match store.put_received_with(m.id, inner, via, false, now, bundle.seq) {
            Ok(true) => {
                log(format!(
                    "received {} from {} via {via} (unverified; no receipt)",
                    short(&m.id),
                    bundle.from
                ));
                notify.send("message");
                Acceptance::Stored
            }
            Ok(false) => Acceptance::Duplicate,
            Err(error) => Acceptance::Rejected(format!("store: {error}")),
        };
    }
    let receipt_ttl = bundle
        .expires_at()
        .saturating_sub(now)
        .max(24 * 3600)
        .min(u64::from(u32::MAX)) as u32;
    // Receipts are critical control traffic: at least Priority so they outrank routine chat.
    let receipt_prec = bundle.precedence().rank().max(1);
    let receipt = match Bundle::receipt(
        our_recipient.expect("checked above"),
        bundle.from,
        m.id,
        now,
        receipt_ttl,
    )
    .with_precedence(hm_bundle::Precedence::from_u8(receipt_prec))
    .with_max_hops(bundle.max_hops())
    .seal(identity)
    {
        Ok(receipt) => receipt,
        Err(error) => return Acceptance::Rejected(format!("cannot create receipt: {error}")),
    };
    let receipt_bytes = receipt.to_vec();
    let received = ReceivedMessage {
        id: m.id,
        object: inner,
        from: via,
        verified: true,
        wire_seq: bundle.seq,
    };
    let reply = QueuedMessage {
        id: receipt.id(),
        object: &receipt_bytes,
        to: bundle.from,
        precedence: receipt_prec,
        expires_at: now.saturating_add(u64::from(receipt_ttl)),
        max_hops: bundle.max_hops(),
    };
    match store.receive_with_reply(received, reply, now) {
        Ok(true) => {
            log(format!(
                "received {} from {} via {via} (verified)",
                short(&m.id),
                bundle.from
            ));
            notify.send("message");
            Acceptance::Stored
        }
        Ok(false) => Acceptance::Duplicate,
        Err(e) => Acceptance::Rejected(format!("store: {e}")),
    }
}
