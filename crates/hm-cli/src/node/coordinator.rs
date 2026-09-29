//! Delivery coordinator: due messages, routing, and multi-bearer handoff.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use hm_bundle::{Bundle, Kind, Opened, Precedence};
use hm_core::DetRng;
use hm_model::{Bearer, Beliefs, CustodianObservation, LinkKey, LinkObservation};
use hm_net::{Net, NetError};
use hm_route::{
    plan_routes, BeaconObservation, ContactGraph, ContactKey, GraphConfig, LiveContact, Route, RouteRequest,
    RoutingPolicy,
};
use hm_store::{Direction, ReclaimOutcome, Retry, Store};
use hm_wire::{wrap_routed, Callsign, ObjectId, FLAG_INTERNET, FLAG_MAILBOX, FLAG_RELAY};
use hm_xfer::{Failure, Receipt};

use super::accept::{accept, Acceptance, AcceptanceGate};
use super::arq;
use super::control::{
    live_window_secs, radio_pull_due, ControlPlane, LIVE_ADVERT_REFRESH_SECS, LIVE_ADVERT_VALIDITY_SECS,
};
use super::heard;
use super::internet::ensure_internet;
use super::live::LiveConfig;
use super::radio::{RadioCmd, RadioEvt};
use super::rf_policy;
use super::sync::{
    advertised_flags, apply_sync_actions, broadcast_sync, live_advert, receive_sync, scheduled_advert,
    send_sync,
};
use super::types::{log, short, NodeConfig, Notify, Status};
use crate::station::unix_now;

pub(crate) struct InFlight {
    bearer: Bearer,
    peer: Callsign,
    route: Route,
    object_bytes: u64,
    /// The route's chance beyond its first hop, and the best other way's:
    /// what the custody suspect time is decided from.
    downstream: f64,
    alternative: f64,
    expires_at: u64,
}

impl InFlight {
    /// A transfer not planned as a route (a bulletin, a requested copy).
    fn direct(bearer: Bearer, peer: Callsign, object_bytes: u64, now: u64) -> InFlight {
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
        }
    }
}

/// How a handoff ended, and so what it says about the link and the custodian.
enum Outcome {
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
    fn from_net(result: Result<(), NetError>) -> Outcome {
        match result {
            Ok(()) => Outcome::Delivered,
            Err(NetError::Busy { retry_after, reason }) => Outcome::Busy {
                reason: format!("busy for {retry_after} s: {reason}"),
                retry_after: u64::from(retry_after),
            },
            Err(NetError::Rejected(r)) => Outcome::Refused {
                reason: format!("rejected: {r}"),
                permanent: false,
            },
            Err(e) => Outcome::LinkFailed(e.to_string()),
        }
    }

    fn from_modem(result: Result<(), String>) -> Outcome {
        match result {
            Ok(()) => Outcome::Delivered,
            Err(e) if e.starts_with("refused: busy for ") => {
                let retry_after = e["refused: busy for ".len()..]
                    .split(' ')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(60);
                Outcome::Busy {
                    reason: e,
                    retry_after,
                }
            }
            Err(e) if e.starts_with("refused: ") => Outcome::Refused {
                reason: e,
                permanent: true,
            },
            Err(e) => Outcome::LinkFailed(e),
        }
    }

    fn from_radio(reason: Failure) -> Outcome {
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
}

/// What a flight's outcome tells the beliefs: the link's part and the
/// custodian's, each only where it bears.
fn learn(beliefs: &mut Beliefs, me: Callsign, flight: &InFlight, outcome: &Outcome, now: u64) {
    let link = LinkKey {
        from: me,
        to: flight.peer,
        bearer: flight.bearer,
    };
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

pub(crate) type NetResult = (ObjectId, Callsign, Result<(), NetError>);

/// Check this often whether we hold anything others may pull (our beacons say so).
const HOLDING_CHECK_SECS: u64 = 30;

/// Our claim of a live contact: since when it has lasted, and when we last
/// claimed it. Refreshes keep `since`, so peers see one contact whose
/// validity grows, not a new contact every time.
#[derive(Copy, Clone, Debug)]
struct LiveClaim {
    since: u64,
    claimed_at: u64,
}

/// Claim (or refresh) our live contact to `peer` over `bearer`, with our
/// belief that a handoff over it completes. Live claims go to the internet
/// core only: on the radio, beacons already tell every station in reach who
/// hears whom.
#[allow(clippy::too_many_arguments)]
fn claim_live(
    cfg: &NodeConfig,
    relay: &crate::config::RelaySettings,
    control: &mut ControlPlane,
    claims: &mut BTreeMap<(Callsign, Bearer), LiveClaim>,
    peer: Callsign,
    bearer: Bearer,
    success: f64,
    rate_bps: u32,
    capacity_bytes: u64,
    now: u64,
) {
    let key = (peer, bearer);
    let claim = claims.get(&key).copied();
    if claim.is_some_and(|c| now.saturating_sub(c.claimed_at) < LIVE_ADVERT_REFRESH_SECS) {
        return;
    }
    // The same stretch of contact while our last claim of it is still valid.
    let since = match claim {
        Some(c) if now.saturating_sub(c.claimed_at) < LIVE_ADVERT_VALIDITY_SECS => c.since,
        _ => now,
    };
    match live_advert(
        cfg,
        relay,
        peer,
        bearer,
        success,
        rate_bps,
        capacity_bytes,
        since,
        now,
    ) {
        Ok(advert) => match control.observe_local(advert, now.saturating_mul(1_000), false) {
            Ok(()) => {
                claims.insert(
                    key,
                    LiveClaim {
                        since,
                        claimed_at: now,
                    },
                );
            }
            Err(error) => log(format!("ignored local CONTACT advert: {error}")),
        },
        Err(error) => log(format!("ignored local CONTACT advert: {error}")),
    }
}

fn enqueue_custody_fail(
    store: &Store,
    me: Callsign,
    identity: &hm_ident::Identity,
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

fn on_gave_up_receipt(store: &Store, receipt_id: ObjectId, now: u64) {
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

fn retry_policy_for(store: &Store, id: ObjectId, live: &super::live::Live) -> hm_store::RetryPolicy {
    let Ok(Some(object)) = store.object(id) else {
        return live.retry;
    };
    let Ok(opened) = Opened::decode(&object) else {
        return live.retry;
    };
    if opened.bundle.kind == Kind::Receipt {
        live.receipt_retry
    } else {
        live.retry
    }
}

/// Whether a message that has used up its retries is held until its bundle
/// expires rather than given up: our own messages, and holdings of a mailbox
/// relay, whose job is to wait for a destination that is rarely in reach.
/// A plain relay gives up so the custodian before it can try another path.
fn holds_until_expiry(store: &Store, id: ObjectId, live: &super::live::Live) -> bool {
    match store.record(id) {
        Ok(Some(record)) => match record.direction {
            Direction::Out => true,
            Direction::Relay => live.relay.mailbox,
            _ => false,
        },
        _ => false,
    }
}

/// `station` is in reach: try its queued messages now rather than at their
/// next scheduled retry, which may be an hour away.
fn wake_for(store: &Store, station: Callsign, how: &str, now: u64) {
    match store.wake(station, now) {
        Ok(0) => {}
        Ok(woken) => log(format!("{station} {how}: trying {woken} queued message(s) now")),
        Err(error) => log(format!("store: {error}")),
    }
}

/// Publish signed claims for our own scheduled contacts, numbered from `base`.
/// Peers keep the claim with the newest sequence number, so republishing with
/// a higher base replaces what they hold, for example after the relay or
/// mailbox flags changed.
fn advertise_schedules(
    cfg: &NodeConfig,
    relay: &crate::config::RelaySettings,
    control: &mut ControlPlane,
    base: u32,
    now: u64,
) {
    for (index, schedule) in cfg.schedules.iter().copied().enumerate() {
        match scheduled_advert(cfg, relay, schedule, base.wrapping_add(index as u32)) {
            Ok(Some(advert)) => {
                if let Err(error) = control.observe_local(advert, now.saturating_mul(1_000), true) {
                    log(format!("ignored local CONTACT advert: {error}"));
                }
            }
            Ok(None) => {}
            Err(error) => log(format!("ignored local CONTACT advert: {error}")),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn coordinator(
    cfg: &NodeConfig,
    store: &Arc<Store>,
    net: &mut Option<Arc<Net>>,
    modem: Option<Arc<arq::Arq>>,
    radio_cmd: mpsc::Sender<RadioCmd>,
    mut radio_evt: tokio::sync::mpsc::UnboundedReceiver<RadioEvt>,
    net_control_tx: tokio::sync::mpsc::UnboundedSender<(Callsign, Vec<u8>)>,
    mut net_control: tokio::sync::mpsc::UnboundedReceiver<(Callsign, Vec<u8>)>,
    status: &Mutex<Status>,
    live: &Arc<LiveConfig>,
    notify: &Notify,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut live_version = live.version();
    let seed = cfg
        .seed
        .unwrap_or_else(|| getrandom::u64().unwrap_or_else(|_| unix_now()));
    // Each route plan draws once from the beliefs (Thompson sampling), from
    // its own stream.
    let rng = DetRng::from_seed(seed);
    let mut plans = 0_u64;
    let mut graph = ContactGraph::new(GraphConfig::default()).expect("default contact graph");
    for schedule in &cfg.schedules {
        if let Err(error) = graph.add_schedule(*schedule) {
            log(format!("ignored contact schedule: {error}"));
        }
    }
    let startup = unix_now();
    let mut control = ControlPlane::for_station(cfg.me);
    // The flags our adverts carry, from the relay settings in use; when they
    // change while running, the schedules are advertised again.
    let mut advertised = advertised_flags(cfg.internet.is_some(), &live.get().relay);
    let mut schedule_base = startup as u32;
    advertise_schedules(cfg, &live.get().relay, &mut control, schedule_base, startup);
    // What this station has learned about links and custodians.
    let mut beliefs = Beliefs::new();
    match store.beliefs() {
        Ok(saved) => {
            for (key, value) in saved {
                if let Err(error) = beliefs.restore(&key, &value) {
                    log(format!("ignored a saved belief: {error}"));
                }
            }
        }
        Err(error) => log(format!("could not restore beliefs: {error}")),
    }
    // Chance, beyond the custodian, of each message handed over lately: what
    // a missing end-to-end receipt is weighed against.
    let mut handed: BTreeMap<ObjectId, f64> = BTreeMap::new();
    let mut radio_up = false;
    let mut last_down: Option<String> = None;
    let mut radio_via = cfg.radio.as_ref().map(|r| r.describe());
    let mut in_flight: BTreeMap<(ObjectId, Callsign), InFlight> = BTreeMap::new();
    let mut radio_ids: BTreeMap<(ObjectId, Callsign), ObjectId> = BTreeMap::new();
    let mut known_internet_links = BTreeSet::new();
    let mut last_pairwise_sync: BTreeMap<(Callsign, Bearer), u64> = BTreeMap::new();
    let mut last_sync_ignore: Option<(Callsign, u64)> = None;
    let mut advertised_live: BTreeMap<(Callsign, Bearer), LiveClaim> = BTreeMap::new();
    // A beacon heard this long ago no longer shows a live radio contact. It
    // follows our beacon interval, which grows as more stations share the channel.
    let mut live_window = live_window_secs(live.get().beacon_secs);
    graph.set_live_contact_secs(live_window);
    // How often stations beacon: a beacon due and not heard is news too.
    let mut beacon_interval = live.get().beacon_secs;
    let mut holding_sent: Option<bool> = None;
    let mut next_holding_check = 0u64;
    // When the beacon of each station last taken into the contact graph was heard.
    let mut beacons_seen: BTreeMap<Callsign, u64> = BTreeMap::new();
    let (net_tx, mut net_rx) = tokio::sync::mpsc::unbounded_channel::<NetResult>();
    let (modem_tx, mut modem_rx) =
        tokio::sync::mpsc::unbounded_channel::<(ObjectId, Callsign, Result<(), String>)>();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    let finish = |id: ObjectId,
                  flight: InFlight,
                  outcome: Outcome,
                  beliefs: &mut Beliefs,
                  graph: &mut ContactGraph,
                  handed: &mut BTreeMap<ObjectId, f64>| {
        let now = unix_now();
        let peer = flight.peer;
        let bearer = flight.bearer;
        learn(beliefs, cfg.me, &flight, &outcome, now);
        let (reason, permanent) = match outcome {
            Outcome::Delivered => {
                if let Some(first) = flight.route.hops.first() {
                    if let Err(error) = graph.consume(first.contact, flight.object_bytes) {
                        log(format!("route capacity: {error}"));
                    }
                    for hop in flight.route.hops.iter().skip(1) {
                        let _ = graph.release(hop.contact, flight.object_bytes);
                    }
                }
                let settings = live.get();
                // Wait for the end-to-end receipt as long as waiting pays:
                // resending sooner risks a duplicate, later a lost message
                // found out too late. The final destination itself cannot
                // lose what it holds: from there, resending never pays.
                let at_destination = store
                    .record(id)
                    .ok()
                    .flatten()
                    .is_some_and(|r| r.final_destination() == peer);
                let suspect_secs = beliefs.suspect_after(
                    peer,
                    flight.downstream,
                    if at_destination { 0.0 } else { flight.alternative },
                    1.0 / settings.costs.of(bearer).max(1.0e-6),
                    flight.expires_at.saturating_sub(now),
                    (60, settings.custody_suspect_secs),
                    now,
                );
                match store.custody_transferred(
                    id,
                    hm_store::CustodyHandoff {
                        next_hop: peer,
                        receipt_verified: true,
                        by: bearer.name(),
                        now,
                        grace_secs: settings.custody_grace_secs,
                        suspect_secs,
                    },
                ) {
                    Ok(true) => {
                        handed.insert(id, flight.downstream);
                        log(format!(
                            "custody of {} transferred to {peer} by {}; receipt expected within {} s",
                            short(&id),
                            bearer.name(),
                            suspect_secs
                        ))
                    }
                    Ok(false) => log(format!(
                        "ignored late custody receipt for {} from {peer}",
                        short(&id)
                    )),
                    Err(e) => log(format!("store: {e}")),
                }
                notify.send("message");
                return;
            }
            Outcome::LinkFailed(reason) | Outcome::Unverified(reason) => (reason, false),
            Outcome::Busy { reason, .. } => (reason, false),
            Outcome::Refused { reason, permanent } | Outcome::Local { reason, permanent } => {
                (reason, permanent)
            }
        };
        for hop in &flight.route.hops {
            let _ = graph.release(hop.contact, flight.object_bytes);
        }
        if let Err(error) = store.clear_next_hop(id, peer) {
            log(format!("store: {error}"));
        }
        let settings = live.get();
        let policy = retry_policy_for(store, id, &settings);
        let r = if permanent {
            store.abandon(id, &reason).map(|notify| (Retry::GaveUp, notify))
        } else {
            let hold = holds_until_expiry(store, id, &settings);
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
                    enqueue_custody_fail(store, cfg.me, &cfg.key.identity, id, prior, &reason, now);
                }
            }
            Ok((Retry::Inactive, _)) => {}
            Err(e) => log(format!("store: {e}")),
        }
        notify.send("message");
    };

    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            Some(ev) = radio_evt.recv() => match ev {
                RadioEvt::Using(via) => {
                    if via != radio_via {
                        match (&via, &radio_via) {
                            (Some(desc), _) => log(format!("radio now {desc}")),
                            (None, Some(_)) => log("radio now off"),
                            (None, None) => {}
                        }
                    }
                    radio_via = via;
                }
                RadioEvt::Up => {
                    radio_up = true;
                    last_down = None;
                    // A new radio session starts without our holding flag.
                    holding_sent = None;
                    next_holding_check = 0;
                    log("radio up");
                }
                RadioEvt::BeaconInterval(secs) => {
                    live_window = live_window_secs(secs);
                    graph.set_live_contact_secs(live_window);
                    beacon_interval = secs;
                }
                RadioEvt::Down(why) => {
                    radio_up = false;
                    // Retries fail the same way every few seconds: say it once.
                    // A radio switched off is not news.
                    if radio_via.is_some() && last_down.as_deref() != Some(why.as_str()) {
                        log(format!("radio down: {why}"));
                        last_down = Some(why);
                    }
                    let lost: Vec<(ObjectId, Callsign)> = in_flight
                        .iter()
                        .filter(|(_, flight)| flight.bearer == Bearer::Radio)
                        .map(|(key, _)| *key)
                        .collect();
                    radio_ids.clear();
                    for key in lost {
                        let flight = in_flight.remove(&key).expect("listed");
                        // Our radio, not the link: nothing learned about the peer.
                        finish(
                            key.0,
                            flight,
                            Outcome::Local {
                                reason: "radio went down".into(),
                                permanent: false,
                            },
                            &mut beliefs,
                            &mut graph,
                            &mut handed,
                        );
                    }
                }
                RadioEvt::Received {
                    from,
                    xfer_id,
                    object,
                } => {
                    let current = live.get();
                    let acceptance = accept(
                        AcceptanceGate {
                            store,
                            notify,
                            trust: &current.trust,
                            me: cfg.me,
                            key_call: cfg.key.call,
                            identity: &cfg.key.identity,
                            relay: &current.relay,
                        },
                        from,
                        &object,
                        None,
                    );
                    let retry_after = if matches!(&acceptance, Acceptance::Busy(_)) {
                        60
                    } else {
                        0
                    };
                    let _ = radio_cmd.send(RadioCmd::Accept {
                        from,
                        xfer_id,
                        accepted: acceptance.custody_accepted(),
                        retry_after,
                    });
                }
                RadioEvt::Sync { from, payload } => {
                    let actions = receive_sync(
                        &mut control,
                        &mut graph,
                        store,
                        live,
                        cfg,
                        None,
                        from,
                        &payload,
                        &mut last_sync_ignore,
                    );
                    apply_sync_actions(
                        actions,
                        net.as_ref(),
                        &radio_cmd,
                        Bearer::Radio,
                        from,
                    );
                }
                RadioEvt::Heard(list) => {
                    let rate = live.get().radio.bitrate;
                    let capacity = (u64::from(rate) * 600 / 8).max(4_096);
                    let now = unix_now();
                    for station in &list {
                        let Some(beacon) = &station.beacon else {
                            continue;
                        };
                        // The table lists each station's latest beacon for a day, and
                        // comes again every half minute: take each beacon once, and
                        // claim a live contact only while one was heard lately.
                        if beacon.key != heard::KeyCheck::Trusted
                            || beacons_seen.get(&station.call) == Some(&beacon.at)
                            || now.saturating_sub(beacon.at) >= live_window
                        {
                            continue;
                        }
                        beacons_seen.insert(station.call, beacon.at);
                        wake_for(store, station.call, "heard", now);
                        let observation = BeaconObservation {
                            origin: station.call,
                            receiver: cfg.me,
                            heard: &beacon.heard,
                            rate_bps: rate,
                            capacity_bytes: capacity,
                            flags: beacon.flags,
                            observed_at: beacon.at,
                        };
                        // The beacon came through from its origin to us; its
                        // signed list says which links into its origin were
                        // open, and when.
                        let radio = |from, to| LinkKey {
                            from,
                            to,
                            bearer: Bearer::Radio,
                        };
                        beliefs.observe_link(radio(station.call, cfg.me), beacon.at, LinkObservation::Beacon);
                        for (heard, at) in observation.hearings() {
                            beliefs.observe_link(radio(heard, station.call), at, LinkObservation::Reported);
                        }
                        claim_live(
                            cfg,
                            &live.get().relay,
                            &mut control,
                            &mut advertised_live,
                            station.call,
                            Bearer::Radio,
                            beliefs.link_success(radio(cfg.me, station.call), now, now),
                            rate,
                            capacity,
                            now,
                        );
                        if let Err(error) = graph.observe_beacon(observation) {
                            log(format!("ignored beacon contact from {}: {error}", station.call));
                        }
                        // Pull only from a station whose beacon says it holds something.
                        let sync_key = (station.call, Bearer::Radio);
                        let last = last_pairwise_sync.get(&sync_key).copied();
                        if radio_pull_due(beacon.flags, last, now) {
                            match control.filters(
                                station.call,
                                store,
                                live.get().relay.enabled || live.get().relay.mailbox,
                                now,
                            ) {
                                Ok(filters) => {
                                    for payload in filters {
                                        send_sync(
                                            net.as_ref(),
                                            &radio_cmd,
                                            Bearer::Radio,
                                            station.call,
                                            payload,
                                        );
                                    }
                                    last_pairwise_sync.insert(sync_key, now);
                                }
                                Err(error) => log(format!("could not SYNC with {}: {error}", station.call)),
                            }
                        }
                    }
                    // Beacons due from stations heard before, and not heard:
                    // closed links, or open ones that lost them.
                    if beacon_interval > 0 {
                        let now = unix_now();
                        let listened: Vec<LinkKey> = beliefs
                            .links()
                            .map(|(key, _)| *key)
                            .filter(|key| key.to == cfg.me && key.bearer == Bearer::Radio)
                            .collect();
                        for key in listened {
                            beliefs.note_silence(key, now, beacon_interval);
                        }
                    }
                    status.lock().expect("lock").heard = list;
                    notify.send("status");
                }
                RadioEvt::Delivered {
                    xfer_id,
                    to,
                    receipt,
                } => {
                    if let Some(id) = radio_ids.remove(&(xfer_id, to)) {
                        if to == hm_xfer::broadcast_peer() {
                            // Bulletin publish: no custody receipt expected.
                            if let Some(flight) = in_flight.remove(&(id, to)) {
                                for hop in &flight.route.hops {
                                    let _ = graph.release(hop.contact, flight.object_bytes);
                                }
                            }
                            match store.delivered(id, false, "radio", unix_now()) {
                                Ok(()) => log(format!("published bulletin {}", short(&id))),
                                Err(e) => log(format!("store: {e}")),
                            }
                            notify.send("message");
                        } else if let Some(flight) = in_flight.remove(&(id, to)) {
                            let outcome = match receipt {
                                Receipt::Verified => {
                                    control.clear_request(id, to);
                                    Outcome::Delivered
                                }
                                Receipt::Unverified => Outcome::Unverified(format!(
                                    "custody receipt from {to} is not verified; retaining custody"
                                )),
                            };
                            finish(id, flight, outcome, &mut beliefs, &mut graph, &mut handed);
                        }
                    }
                }
                RadioEvt::Failed {
                    xfer_id,
                    to,
                    reason,
                } => {
                    if let Some(id) = radio_ids.remove(&(xfer_id, to)) {
                        if let Some(flight) = in_flight.remove(&(id, to)) {
                            finish(
                                id,
                                flight,
                                Outcome::from_radio(reason),
                                &mut beliefs,
                                &mut graph,
                                &mut handed,
                            );
                        }
                    }
                }
                RadioEvt::Over { to, sent, got } => {
                    let link = LinkKey {
                        from: cfg.me,
                        to,
                        bearer: Bearer::Radio,
                    };
                    beliefs.observe_link(link, unix_now(), LinkObservation::Over { sent, got });
                }
            },
            Some((from, payload)) = net_control.recv() => {
                let actions = receive_sync(
                    &mut control,
                    &mut graph,
                    store,
                    live,
                    cfg,
                    net.as_ref(),
                    from,
                    &payload,
                    &mut last_sync_ignore,
                );
                apply_sync_actions(
                    actions,
                    net.as_ref(),
                    &radio_cmd,
                    Bearer::Internet,
                    from,
                );
            }
            Some((id, peer, result)) = net_rx.recv() => {
                let bulletin = store
                    .record(id)
                    .ok()
                    .flatten()
                    .is_some_and(|r| r.final_destination() == hm_xfer::broadcast_peer());
                if bulletin {
                    if let Some(flight) = in_flight.remove(&(id, peer)) {
                        control.clear_request(id, peer);
                        for hop in &flight.route.hops {
                            let _ = graph.release(hop.contact, flight.object_bytes);
                        }
                        let now = unix_now();
                        let outcome = Outcome::from_net(result);
                        learn(&mut beliefs, cfg.me, &flight, &outcome, now);
                        match outcome {
                            Outcome::Delivered => {
                                match store.delivered(id, false, "internet", now) {
                                    Ok(()) => log(format!(
                                        "published bulletin {} to {peer} over the internet",
                                        short(&id)
                                    )),
                                    Err(e) => log(format!("store: {e}")),
                                }
                            }
                            Outcome::LinkFailed(reason)
                            | Outcome::Refused { reason, .. }
                            | Outcome::Busy { reason, .. }
                            | Outcome::Unverified(reason)
                            | Outcome::Local { reason, .. } => {
                                log(format!(
                                    "bulletin {} to {peer} failed: {reason}",
                                    short(&id)
                                ));
                                // Retry only when nothing else is still carrying this id.
                                if !in_flight.keys().any(|(flight_id, _)| *flight_id == id) {
                                    match store.attempt_failed(
                                        id,
                                        &format!("{reason} (internet)"),
                                        live.get().retry,
                                        now,
                                    ) {
                                        Ok((Retry::At(t), _)) => log(format!(
                                            "bulletin {}; next try in {} s",
                                            short(&id),
                                            t.saturating_sub(now)
                                        )),
                                        Ok((Retry::GaveUp, _)) => {
                                            log(format!("gave up on bulletin {}", short(&id)))
                                        }
                                        Ok((Retry::Inactive, _)) => {}
                                        Err(err) => log(format!("store: {err}")),
                                    }
                                }
                            }
                        }
                        notify.send("message");
                        notify.send("status");
                    }
                } else {
                    let outcome = Outcome::from_net(result);
                    if let Some(flight) = in_flight.remove(&(id, peer)) {
                        if matches!(outcome, Outcome::Delivered) {
                            control.clear_request(id, peer);
                        }
                        finish(id, flight, outcome, &mut beliefs, &mut graph, &mut handed);
                    }
                }
            }
            Some((id, peer, result)) = modem_rx.recv() => {
                let outcome = Outcome::from_modem(result);
                if let Some(flight) = in_flight.remove(&(id, peer)) {
                    if matches!(outcome, Outcome::Delivered) {
                        control.clear_request(id, peer);
                    }
                    finish(id, flight, outcome, &mut beliefs, &mut graph, &mut handed);
                }
            }
            _ = tick.tick() => {
                match live.reload_if_changed() {
                    Some(Ok(())) => log("settings file changed; applied"),
                    Some(Err(e)) => log(format!("settings file not applied, keeping the settings in use: {e}")),
                    None => {}
                }
                // Beliefs changed since the last tick, in one transaction.
                beliefs.prune(unix_now());
                if let Err(error) = store.save_beliefs(&beliefs.take_changed()) {
                    log(format!("store: {error}"));
                }
                // End-to-end receipts for messages a custodian took: how long
                // it took, and that it did its part.
                match store.take_custody_outcomes() {
                    Ok(outcomes) => {
                        for outcome in outcomes {
                            handed.remove(&outcome.id);
                            beliefs.observe_custodian(
                                outcome.custodian,
                                outcome.delivered_at,
                                CustodianObservation::Delivered {
                                    delay_secs: outcome.delivered_at.saturating_sub(outcome.handed_at),
                                },
                            );
                        }
                    }
                    Err(error) => log(format!("store: {error}")),
                }
                if live.version() != live_version {
                    live_version = live.version();
                    notify.send("settings");
                    let snapshot = live.get();
                    ensure_internet(
                        cfg,
                        store,
                        live,
                        notify,
                        net,
                        &net_control_tx,
                        status,
                    );
                    if let Some(n) = net.as_ref() {
                        n.set_trust(&snapshot.trust.iter().collect::<Vec<_>>());
                        n.set_dial(snapshot.peers.clone());
                    }
                    let flags = advertised_flags(cfg.internet.is_some(), &snapshot.relay);
                    if flags != advertised {
                        advertised = flags;
                        let now = unix_now();
                        // Above every sequence number used so far, even within one second.
                        schedule_base = (now as u32)
                            .max(schedule_base.wrapping_add(cfg.schedules.len().max(1) as u32));
                        advertise_schedules(cfg, &snapshot.relay, &mut control, schedule_base, now);
                        // Live contacts are claimed again at once, with the new flags.
                        for claim in advertised_live.values_mut() {
                            claim.claimed_at = now.saturating_sub(LIVE_ADVERT_REFRESH_SECS);
                        }
                    }
                }
                let now = unix_now();
                let links = net.as_ref().map(|n| n.connected()).unwrap_or_default();
                let current_links: BTreeSet<Callsign> = links.iter().copied().collect();
                let internet = |from, to| LinkKey {
                    from,
                    to,
                    bearer: Bearer::Internet,
                };
                // Links that went down since the last tick are closed.
                for peer in known_internet_links.difference(&current_links) {
                    beliefs.observe_link(internet(cfg.me, *peer), now, LinkObservation::Down);
                    beliefs.observe_link(internet(*peer, cfg.me), now, LinkObservation::Down);
                }
                for peer in &current_links {
                    for (from, to) in [(cfg.me, *peer), (*peer, cfg.me)] {
                        if let Err(error) = graph.observe_live_link(LiveContact {
                            from,
                            to,
                            bearer: Bearer::Internet,
                            rate_bps: 10_000_000,
                            capacity_bytes: 64 * 1024 * 1024,
                            flags: FLAG_INTERNET,
                            observed_at: now,
                        }) {
                            log(format!("ignored internet contact {from} -> {to}: {error}"));
                        }
                    }
                    claim_live(
                        cfg,
                        &live.get().relay,
                        &mut control,
                        &mut advertised_live,
                        *peer,
                        Bearer::Internet,
                        beliefs.link_success(internet(cfg.me, *peer), now, now),
                        10_000_000,
                        64 * 1024 * 1024,
                        now,
                    );
                    if !known_internet_links.contains(peer) {
                        beliefs.observe_link(internet(cfg.me, *peer), now, LinkObservation::Up);
                        beliefs.observe_link(internet(*peer, cfg.me), now, LinkObservation::Up);
                        wake_for(store, *peer, "linked", now);
                        match control.contact_messages() {
                            Ok(messages) => {
                                for payload in messages {
                                    send_sync(
                                        net.as_ref(),
                                        &radio_cmd,
                                        Bearer::Internet,
                                        *peer,
                                        payload,
                                    );
                                }
                            }
                            Err(error) => log(format!("could not encode CONTACT adverts: {error}")),
                        }
                        match control.filters(
                            *peer,
                            store,
                            live.get().relay.enabled || live.get().relay.mailbox,
                            now,
                        ) {
                            Ok(filters) => {
                                for payload in filters {
                                    send_sync(
                                        net.as_ref(),
                                        &radio_cmd,
                                        Bearer::Internet,
                                        *peer,
                                        payload,
                                    );
                                }
                                last_pairwise_sync.insert((*peer, Bearer::Internet), now);
                            }
                            Err(error) => log(format!("could not SYNC with {peer}: {error}")),
                        }
                    }
                    let sync_key = (*peer, Bearer::Internet);
                    let synced_at = last_pairwise_sync.get(&sync_key).copied().unwrap_or(0);
                    if now.saturating_sub(synced_at) >= 5 * 60 {
                        match control.filters(
                            *peer,
                            store,
                            live.get().relay.enabled || live.get().relay.mailbox,
                            now,
                        ) {
                            Ok(filters) => {
                                for payload in filters {
                                    send_sync(
                                        net.as_ref(),
                                        &radio_cmd,
                                        Bearer::Internet,
                                        *peer,
                                        payload,
                                    );
                                }
                                last_pairwise_sync.insert(sync_key, now);
                            }
                            Err(error) => log(format!("could not SYNC with {peer}: {error}")),
                        }
                    }
                }
                known_internet_links = current_links;
                let no_links = BTreeSet::new();
                graph.prune(now);
                match control.due_contacts(now.saturating_mul(1_000), now) {
                    Ok(due) => {
                        for contact in due {
                            broadcast_sync(
                                net.as_ref(),
                                &radio_cmd,
                                radio_up && contact.on_air,
                                if contact.internet { &known_internet_links } else { &no_links },
                                contact.payload,
                            );
                        }
                    }
                    Err(error) => log(format!("could not encode CONTACT advert: {error}")),
                }
                if now >= next_holding_check {
                    next_holding_check = now.saturating_add(HOLDING_CHECK_SECS);
                    match store.holds_for_others(now) {
                        Ok(holding) if holding_sent != Some(holding) => {
                            let _ = radio_cmd.send(RadioCmd::Holding(holding));
                            holding_sent = Some(holding);
                        }
                        Ok(_) => {}
                        Err(error) => log(format!("store: {error}")),
                    }
                }
                match store.suspect_due(now) {
                    Ok(suspects) => {
                        for record in suspects {
                            if in_flight.keys().any(|(id, _)| *id == record.id) {
                                continue;
                            }
                            // Our own message went to a custodian and no receipt
                            // came back in all that time, by any path: the
                            // custodian takes its share of the blame, weighed
                            // against the rest of the route's chance, so a
                            // station that takes custody and drops it stops
                            // attracting traffic. The link that carried the
                            // handoff did its part and is not blamed.
                            if record.direction == Direction::Out {
                                if let Some(custodian) = record.custody_by {
                                    let downstream = handed.remove(&record.id).unwrap_or(0.5);
                                    beliefs.observe_custodian(
                                        custodian,
                                        now,
                                        CustodianObservation::Lost { downstream },
                                    );
                                }
                            }
                            match store.reclaim_custody(
                                record.id,
                                now,
                                "custody suspect; reclaiming",
                            ) {
                                Ok(ReclaimOutcome::Requeued) => {
                                    log(format!(
                                        "reclaimed {} (custody suspect)",
                                        short(&record.id)
                                    ));
                                    notify.send("message");
                                }
                                Ok(ReclaimOutcome::DeliveredUnconfirmed) => {
                                    log(format!(
                                        "{} delivered-unconfirmed after custody suspect",
                                        short(&record.id)
                                    ));
                                    notify.send("message");
                                }
                                Ok(ReclaimOutcome::Failed) => {
                                    log(format!(
                                        "{} failed after custody suspect",
                                        short(&record.id)
                                    ));
                                    if record.direction == Direction::Relay {
                                        if let Some(prior) = record.custody_from {
                                            enqueue_custody_fail(
                                                store,
                                                cfg.me,
                                                &cfg.key.identity,
                                                record.id,
                                                prior,
                                                "custody suspect; expired",
                                                now,
                                            );
                                        }
                                    }
                                    notify.send("message");
                                }
                                Ok(ReclaimOutcome::Ignored) => {}
                                Err(error) => log(format!("store: {error}")),
                            }
                        }
                    }
                    Err(error) => log(format!("store: {error}")),
                }
                let due = match store.due(now) {
                    Ok(d) => d,
                    Err(e) => {
                        log(format!("store: {e}"));
                        continue;
                    }
                };
                for r in due {
                    if in_flight.keys().any(|(id, _)| *id == r.id) {
                        continue;
                    }
                    let Ok(Some(object)) = store.object(r.id) else {
                        continue;
                    };
                    let Ok(opened) = Opened::decode(&object) else {
                        if let Ok(Some(prior)) =
                            store.abandon(r.id, "stored object is not a bundle")
                        {
                            enqueue_custody_fail(
                                store,
                                cfg.me,
                                &cfg.key.identity,
                                r.id,
                                prior,
                                "stored object is not a bundle",
                                now,
                            );
                        }
                        continue;
                    };
                    let bundle = opened.bundle;
                    if bundle.is_expired(now) {
                        if let Ok(Some(prior)) = store.abandon(r.id, "bundle expired") {
                            enqueue_custody_fail(
                                store,
                                cfg.me,
                                &cfg.key.identity,
                                r.id,
                                prior,
                                "bundle expired",
                                now,
                            );
                        }
                        continue;
                    }
                    if bundle.kind == Kind::Bulletin {
                        let bulletin_peer = hm_xfer::broadcast_peer();
                        // A peer asked for this bulletin over SYNC: send it on the internet.
                        if let Some(peer) = control.target_for(r.id, now) {
                            if net.as_ref().is_some_and(|network| network.is_connected(peer))
                                && !in_flight.contains_key(&(r.id, peer))
                            {
                                in_flight.insert(
                                    (r.id, peer),
                                    InFlight::direct(Bearer::Internet, peer, object.len() as u64, now),
                                );
                                let (network, tx, id) = (
                                    net.clone().expect("checked connected"),
                                    net_tx.clone(),
                                    r.id,
                                );
                                let wire = object.clone();
                                tokio::spawn(async move {
                                    let result = network.deliver(peer, &wire).await;
                                    let _ = tx.send((id, peer, result));
                                });
                                log(format!(
                                    "sending bulletin {} to {peer} over the internet (requested)",
                                    short(&r.id)
                                ));
                            }
                            // Still fall through: RF publish / fan-out can proceed too.
                        }
                        if radio_up && !in_flight.contains_key(&(r.id, bulletin_peer)) {
                            if let Err(error) = store.set_next_hop(r.id, bulletin_peer) {
                                log(format!("store: {error}"));
                                continue;
                            }
                            radio_ids.insert((hm_xfer::object_id(&object), bulletin_peer), r.id);
                            in_flight.insert(
                                (r.id, bulletin_peer),
                                InFlight::direct(Bearer::Radio, bulletin_peer, object.len() as u64, now),
                            );
                            // Listeners' links are like the radio links this
                            // station knows, as a population.
                            let prior = beliefs.link_prior(Bearer::Radio);
                            let _ = radio_cmd.send(RadioCmd::Broadcast {
                                object,
                                precedence: r.precedence,
                                erasure: hm_model::Erasure::from_prior(prior.erasure, prior.dispersion),
                            });
                            log(format!(
                                "publishing bulletin {} on radio (attempt {})",
                                short(&r.id),
                                r.attempts + 1
                            ));
                            continue;
                        }
                        // No radio: push once to each connected internet peer.
                        if !radio_up {
                            let mut started = false;
                            for peer in &links {
                                if in_flight.contains_key(&(r.id, *peer)) {
                                    continue;
                                }
                                if net.as_ref().is_none_or(|n| !n.is_connected(*peer)) {
                                    continue;
                                }
                                in_flight.insert(
                                    (r.id, *peer),
                                    InFlight::direct(Bearer::Internet, *peer, object.len() as u64, now),
                                );
                                let (network, tx, id, peer) = (
                                    net.clone().expect("checked"),
                                    net_tx.clone(),
                                    r.id,
                                    *peer,
                                );
                                let wire = object.clone();
                                tokio::spawn(async move {
                                    let result = network.deliver(peer, &wire).await;
                                    let _ = tx.send((id, peer, result));
                                });
                                log(format!(
                                    "publishing bulletin {} to {peer} over the internet",
                                    short(&r.id)
                                ));
                                started = true;
                            }
                            if !started {
                                // Wait for radio or an internet link.
                                continue;
                            }
                        }
                        continue;
                    }
                    let destination = r.final_destination();
                    let origin = bundle.from;
                    if r.direction == Direction::Relay
                        && live.get().trust.key_for(origin).is_none()
                    {
                        let reason = format!("relay origin {origin} is no longer trusted");
                        if let Ok(Some(prior)) = store.abandon(r.id, &reason) {
                            enqueue_custody_fail(
                                store,
                                cfg.me,
                                &cfg.key.identity,
                                r.id,
                                prior,
                                &reason,
                                now,
                            );
                        }
                        log(format!("stopped relaying {}: {reason}", short(&r.id)));
                        continue;
                    }
                    let rf_ok = rf_policy::may_transmit_rf(
                        origin,
                        cfg.me,
                        cfg.key.call,
                        &live.get().trust,
                    );
                    let requested_peer = control.target_for(r.id, now);
                    let route_destination = requested_peer.unwrap_or(destination);
                    let visited = r.visited.as_deref().unwrap_or(&[]);
                    let mut max_hops = r.max_hops.unwrap_or_else(|| bundle.max_hops());
                    max_hops = max_hops.min(bundle.max_hops()).min(live.get().relay.max_hops);
                    if r.direction == Direction::Relay && !live.get().relay.enabled {
                        max_hops = max_hops.min((visited.len() + 1) as u8);
                    }
                    let modem_up = modem.as_ref().is_some_and(|handle| handle.status().up);
                    let routed_len = object.len()
                        + usize::from(r.direction == Direction::Relay)
                            * (10 + 6 * (usize::from(r.hop_count.unwrap_or(0)) + 1));
                    // Links that could be tried now, never seen open: how
                    // likely each is comes from the beliefs about it, which
                    // start from its bearer's population. An ARQ modem can
                    // call any station; the destination may be in radio
                    // range, unheard; a relaying internet gateway may reach
                    // it through the internet core (a default route).
                    let potential = |from, to, bearer, rate_bps, capacity_bytes| LiveContact {
                        from,
                        to,
                        bearer,
                        rate_bps,
                        capacity_bytes,
                        flags: 0,
                        observed_at: now,
                    };
                    // An internet link may be to the destination under its
                    // base callsign: then the link to it is up, seen.
                    if net
                        .as_ref()
                        .is_some_and(|network| network.is_connected(route_destination))
                    {
                        let link = LinkKey {
                            from: cfg.me,
                            to: route_destination,
                            bearer: Bearer::Internet,
                        };
                        if beliefs.link(link).is_none_or(|l| l.last_open().is_none_or(|t| t + 60 < now)) {
                            beliefs.observe_link(link, now, LinkObservation::Up);
                        }
                        let _ = graph.observe_live_link(LiveContact {
                            flags: FLAG_INTERNET,
                            ..potential(cfg.me, route_destination, Bearer::Internet, 10_000_000, 64 * 1024 * 1024)
                        });
                    }
                    if modem_up && rf_ok {
                        let rate_bps = cfg
                            .modem
                            .as_ref()
                            .map_or(1_200, arq::ArqConfig::estimated_rate_bps);
                        let _ = graph.add_potential(potential(
                            cfg.me,
                            route_destination,
                            Bearer::Modem,
                            rate_bps,
                            arq::MAX_OBJECT as u64,
                        ));
                    }
                    if radio_up && rf_ok {
                        let rate = live.get().radio.bitrate;
                        let _ = graph.add_potential(potential(
                            cfg.me,
                            route_destination,
                            Bearer::Radio,
                            rate,
                            (u64::from(rate) * 600 / 8).max(routed_len as u64),
                        ));
                    }
                    let gateways: Vec<Callsign> = graph
                        .stations_flagged(FLAG_INTERNET, now)
                        .filter(|gateway| {
                            *gateway != cfg.me
                                && *gateway != route_destination
                                && !visited.contains(gateway)
                                && graph
                                    .flags(*gateway, now)
                                    .is_some_and(|flags| flags & (FLAG_RELAY | FLAG_MAILBOX) != 0)
                        })
                        .collect();
                    for gateway in gateways {
                        let _ = graph.add_potential(potential(
                            gateway,
                            route_destination,
                            Bearer::Internet,
                            10_000_000,
                            64 * 1024 * 1024,
                        ));
                    }
                    let excluded: Vec<ContactKey> = graph
                        .outgoing(cfg.me, now)
                        .filter(|contact| match contact.key.bearer {
                            Bearer::Radio => !radio_up || !rf_ok,
                            Bearer::Internet => net
                                .as_ref()
                                .is_none_or(|network| !network.is_connected(contact.key.to)),
                            Bearer::Modem => !modem_up || !rf_ok,
                        })
                        .map(|contact| contact.key)
                        .collect();
                    let request = RouteRequest {
                        source: cfg.me,
                        destination: route_destination,
                        now,
                        expires_at: bundle.expires_at(),
                        object_bytes: routed_len as u64,
                        max_hops: if requested_peer.is_some() { 1 } else { max_hops },
                        airtime_budget_millis: live.get().relay.airtime_budget_secs.saturating_mul(1_000),
                        visited,
                        excluded_contacts: &excluded,
                        urgent: r.precedence >= 2,
                    };
                    let policy = RoutingPolicy {
                        attempt_cost: live.get().costs.attempt_cost(),
                        ..RoutingPolicy::default()
                    };
                    // Plan with one draw from the beliefs: links little is
                    // known about get tried in proportion to the chance that
                    // they are the best.
                    plans += 1;
                    let mut draw = beliefs.thompson(rng.fork(plans), now);
                    let Ok(plan) = plan_routes(&graph, &mut draw, &request, policy) else {
                        continue;
                    };
                    let alternative = plan
                        .alternatives
                        .iter()
                        .map(|route| route.success_probability)
                        .fold(0.0_f64, f64::max);
                    for route in plan.active {
                        let Some(first) = route.hops.first() else {
                            continue;
                        };
                        if first.depart > now {
                            continue;
                        }
                        let bearer = first.contact.bearer;
                        let available = match bearer {
                            Bearer::Radio => radio_up && rf_ok,
                            Bearer::Internet => {
                                net.as_ref().is_some_and(|network| network.is_connected(first.contact.to))
                            }
                            Bearer::Modem => modem_up && rf_ok,
                        };
                        if !available || in_flight.contains_key(&(r.id, first.contact.to)) {
                            continue;
                        }
                        let route_keys: Vec<ContactKey> =
                            route.hops.iter().map(|hop| hop.contact).collect();
                        if graph.reserve_many(&route_keys, routed_len as u64).is_err() {
                            continue;
                        }
                        if !store.set_next_hop(r.id, first.contact.to).unwrap_or(false) {
                            graph.release_many(&route_keys, routed_len as u64);
                            continue;
                        }
                        let wire_object = if r.direction == Direction::Relay {
                            let mut path = visited.to_vec();
                            path.push(cfg.me);
                            match wrap_routed(r.hop_count.unwrap_or(0) + 1, &path, &object) {
                                Ok(wrapped) => wrapped,
                                Err(error) => {
                                    graph.release_many(&route_keys, routed_len as u64);
                                    let _ = store.clear_next_hop(r.id, first.contact.to);
                                    log(format!("cannot route {}: {error}", short(&r.id)));
                                    continue;
                                }
                            }
                        } else {
                            object.clone()
                        };
                        let peer = first.contact.to;
                        log(format!(
                            "sending {} toward {} via {peer} by {} (attempt {})",
                            short(&r.id),
                            destination,
                            bearer.name(),
                            r.attempts + 1
                        ));
                        // The route's chance beyond its first hop.
                        let downstream = route
                            .hops
                            .iter()
                            .skip(1)
                            .map(|hop| f64::from(hop.probability_permillion) / 1_000_000.0)
                            .product::<f64>();
                        in_flight.insert(
                            (r.id, peer),
                            InFlight {
                                bearer,
                                peer,
                                route,
                                object_bytes: routed_len as u64,
                                downstream,
                                alternative,
                                expires_at: bundle.expires_at(),
                            },
                        );
                        match bearer {
                            Bearer::Radio => {
                                radio_ids.insert((hm_xfer::object_id(&wire_object), peer), r.id);
                                let link = LinkKey {
                                    from: cfg.me,
                                    to: peer,
                                    bearer: Bearer::Radio,
                                };
                                let _ = radio_cmd.send(RadioCmd::Send {
                                    object: wire_object,
                                    to: peer,
                                    precedence: r.precedence,
                                    erasure: beliefs.erasure(link, now),
                                });
                            }
                            Bearer::Internet => {
                                let (network, tx, id) = (
                                    net.clone().expect("route checked connected"),
                                    net_tx.clone(),
                                    r.id,
                                );
                                tokio::spawn(async move {
                                    let result = network.deliver(peer, &wire_object).await;
                                    let _ = tx.send((id, peer, result));
                                });
                            }
                            Bearer::Modem => {
                                let (modem, tx, id) = (
                                    modem.clone().expect("route checked available"),
                                    modem_tx.clone(),
                                    r.id,
                                );
                                tokio::spawn(async move {
                                    let result = modem.deliver(peer, wire_object).await;
                                    let _ = tx.send((id, peer, result));
                                });
                            }
                        }
                    }
                }
                let mut st = status.lock().expect("lock");
                let radio = radio_via.as_ref().map(|_| radio_up);
                if (st.radio, &st.radio_via, &st.internet_peers) != (radio, &radio_via, &links) {
                    notify.send("status");
                }
                let (modem_up, modem_peer) = match &modem {
                    Some(m) => {
                        let s = m.status();
                        (Some(s.up), s.peer)
                    }
                    None => (None, None),
                };
                if (st.modem, st.modem_peer) != (modem_up, modem_peer) {
                    notify.send("status");
                }
                st.modem = modem_up;
                st.modem_peer = modem_peer;
                st.radio = radio;
                st.radio_via = radio_via.clone();
                st.internet_peers = links;
                st.estimates = beliefs
                    .links()
                    .filter(|(key, _)| key.from == cfg.me)
                    .map(|(key, _)| (key.to, key.bearer.name(), beliefs.link_success(*key, now, now)))
                    .collect();
            }
        }
    }
}
