//! Delivery coordinator: due messages, routing, and multi-bearer handoff.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use hm_bundle::{Bundle, Kind, Opened, Precedence};
use hm_core::DetRng;
use hm_net::{Net, NetError};
use hm_route::{
    plan_routes, BeaconObservation, Bearer as RouteBearer, ContactGraph, ContactKey, GraphConfig,
    LiveContact, Route, RouteRequest, RoutingPolicy,
};
use hm_store::{Direction, ReclaimOutcome, Retry, Store};
use hm_wire::{wrap_routed, Callsign, ObjectId, FLAG_INTERNET};
use hm_xfer::{Failure, Receipt};

use super::accept::{accept, Acceptance, AcceptanceGate};
use super::arq;
use super::choose::{Bearer, Chooser};
use super::control::ControlPlane;
use super::heard;
use super::internet::ensure_internet;
use super::live::LiveConfig;
use super::radio::{RadioCmd, RadioEvt};
use super::rf_policy;
use super::sync::{
    apply_sync_actions, broadcast_sync, live_advert, receive_sync, scheduled_advert, send_sync,
};
use super::types::{log, route_bearer, short, NodeConfig, Notify, Status};
use crate::station::unix_now;

pub(crate) struct InFlight {
    bearer: Bearer,
    peer: Callsign,
    route: Route,
    object_bytes: u64,
}

pub(crate) type NetResult = (ObjectId, Callsign, Result<(), NetError>);

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
            log(format!("cannot build custody-fail for {}: {error}", short(&holding)));
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
        Ok(true) => log(format!(
            "queued custody-fail for {} to {prior}",
            short(&holding)
        )),
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
    match store.delivered_unconfirmed(
        original,
        "end-to-end receipt could not be delivered",
        now,
    ) {
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
    let mut chooser = Chooser::new(
        live.get().costs,
        live.get().evidence_half_life_secs,
        DetRng::from_seed(seed),
    );
    let mut graph = ContactGraph::new(GraphConfig::default()).expect("default contact graph");
    for schedule in &cfg.schedules {
        if let Err(error) = graph.add_schedule(*schedule) {
            log(format!("ignored contact schedule: {error}"));
        }
    }
    let startup = unix_now();
    let mut control = ControlPlane::default();
    for (index, schedule) in cfg.schedules.iter().copied().enumerate() {
        match scheduled_advert(cfg, schedule, (startup as u32).wrapping_add(index as u32)) {
            Ok(Some(advert)) => {
                if let Err(error) = control.observe_local(advert, startup.saturating_mul(1_000)) {
                    log(format!("ignored local CONTACT advert: {error}"));
                }
            }
            Ok(None) => {}
            Err(error) => log(format!("ignored local CONTACT advert: {error}")),
        }
    }
    match store.contact_evidence() {
        Ok(saved) => {
            for (key, evidence) in saved {
                graph.restore_evidence(key, evidence);
                if key.from == cfg.me {
                    let bearer = match key.bearer {
                        RouteBearer::Radio => Bearer::Radio,
                        RouteBearer::Internet => Bearer::Internet,
                        RouteBearer::Modem => Bearer::Modem,
                    };
                    chooser.restore(key.to, bearer, evidence.successes, evidence.failures, evidence.at);
                }
            }
        }
        Err(error) => log(format!("could not restore contact evidence: {error}")),
    }
    let mut radio_up = false;
    let mut last_down: Option<String> = None;
    let mut radio_via = cfg.radio.as_ref().map(|r| r.describe());
    let mut in_flight: BTreeMap<(ObjectId, Callsign), InFlight> = BTreeMap::new();
    let mut radio_ids: BTreeMap<(ObjectId, Callsign), ObjectId> = BTreeMap::new();
    let mut failed_contacts: BTreeMap<ObjectId, BTreeMap<ContactKey, u64>> = BTreeMap::new();
    let mut known_internet_links = BTreeSet::new();
    let mut last_pairwise_sync: BTreeMap<(Callsign, Bearer), u64> = BTreeMap::new();
    let mut last_sync_ignore: Option<(Callsign, u64)> = None;
    let mut advertised_live: BTreeMap<(Callsign, RouteBearer), u64> = BTreeMap::new();
    let (net_tx, mut net_rx) = tokio::sync::mpsc::unbounded_channel::<NetResult>();
    let (modem_tx, mut modem_rx) =
        tokio::sync::mpsc::unbounded_channel::<(ObjectId, Callsign, Result<(), String>)>();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    let finish = |id: ObjectId,
                  flight: InFlight,
                  outcome: Result<(), (String, bool)>,
                  chooser: &mut Chooser,
                  graph: &mut ContactGraph,
                  failed_contacts: &mut BTreeMap<ObjectId, BTreeMap<ContactKey, u64>>| {
        let now = unix_now();
        let peer = flight.peer;
        let bearer = flight.bearer;
        match outcome {
            Ok(()) => {
                chooser.record(peer, bearer, true, now);
                let (key, evidence) = graph.record_delivery(cfg.me, peer, route_bearer(bearer), true, now);
                if let Err(error) = store.save_contact_evidence(key, evidence) {
                    log(format!("store: {error}"));
                }
                if let Some(first) = flight.route.hops.first() {
                    if let Err(error) = graph.consume(first.contact, flight.object_bytes) {
                        log(format!("route capacity: {error}"));
                    }
                    for hop in flight.route.hops.iter().skip(1) {
                        let _ = graph.release(hop.contact, flight.object_bytes);
                    }
                }
                match store.custody_transferred(
                    id,
                    hm_store::CustodyHandoff {
                        next_hop: peer,
                        receipt_verified: true,
                        by: bearer.name(),
                        now,
                        grace_secs: live.get().custody_grace_secs,
                        suspect_secs: live.get().custody_suspect_secs,
                    },
                ) {
                    Ok(true) => log(format!(
                        "custody of {} transferred to {peer} by {}",
                        short(&id),
                        bearer.name()
                    )),
                    Ok(false) => log(format!(
                        "ignored late custody receipt for {} from {peer}",
                        short(&id)
                    )),
                    Err(e) => log(format!("store: {e}")),
                }
            }
            Err((reason, permanent)) => {
                chooser.record(peer, bearer, false, now);
                let (key, evidence) = graph.record_delivery(cfg.me, peer, route_bearer(bearer), false, now);
                if let Err(error) = store.save_contact_evidence(key, evidence) {
                    log(format!("store: {error}"));
                }
                for hop in &flight.route.hops {
                    let _ = graph.release(hop.contact, flight.object_bytes);
                }
                if let Some(first) = flight.route.hops.first() {
                    let cooldown = live.get().retry.delay_after(1).clamp(5, 300);
                    failed_contacts
                        .entry(id)
                        .or_default()
                        .insert(first.contact, now.saturating_add(cooldown));
                }
                if let Err(error) = store.clear_next_hop(id, peer) {
                    log(format!("store: {error}"));
                }
                let policy = retry_policy_for(store, id, &live.get());
                let r = if permanent {
                    store.abandon(id, &reason).map(|notify| (Retry::GaveUp, notify))
                } else {
                    store.attempt_failed(
                        id,
                        &format!("{reason} ({})", bearer.name()),
                        policy,
                        now,
                    )
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
                            enqueue_custody_fail(
                                store,
                                cfg.me,
                                &cfg.key.identity,
                                id,
                                prior,
                                &reason,
                                now,
                            );
                        }
                    }
                    Ok((Retry::Inactive, _)) => {}
                    Err(e) => log(format!("store: {e}")),
                }
            }
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
                    log("radio up");
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
                        finish(
                            key.0,
                            flight,
                            Err(("radio went down".into(), false)),
                            &mut chooser,
                            &mut graph,
                            &mut failed_contacts,
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
                        if beacon.key != heard::KeyCheck::Trusted {
                            continue;
                        }
                        let advert_key = (station.call, RouteBearer::Radio);
                        let advertised_at = advertised_live.get(&advert_key).copied().unwrap_or(0);
                        if now.saturating_sub(advertised_at) >= 10 * 60 {
                            match live_advert(
                                cfg,
                                &live.get().relay,
                                station.call,
                                RouteBearer::Radio,
                                rate,
                                capacity,
                                now,
                            ) {
                                Ok(advert) => {
                                    if let Err(error) =
                                        control.observe_local(advert, now.saturating_mul(1_000))
                                    {
                                        log(format!("ignored local CONTACT advert: {error}"));
                                    } else {
                                        advertised_live.insert(advert_key, now);
                                    }
                                }
                                Err(error) => log(format!("ignored local CONTACT advert: {error}")),
                            }
                        }
                        if let Err(error) = graph.observe_beacon(BeaconObservation {
                            origin: station.call,
                            receiver: cfg.me,
                            heard: &beacon.heard,
                            rate_bps: rate,
                            capacity_bytes: capacity,
                            flags: beacon.flags,
                            observed_at: now,
                        }) {
                            log(format!("ignored beacon contact from {}: {error}", station.call));
                        }
                        let sync_key = (station.call, Bearer::Radio);
                        let last = last_pairwise_sync.get(&sync_key).copied().unwrap_or(0);
                        if now.saturating_sub(last) >= 5 * 60 {
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
                    for (key, evidence) in graph.evidence() {
                        if let Err(error) = store.save_contact_evidence(key, evidence) {
                            log(format!("store: {error}"));
                            break;
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
                                    Ok(())
                                }
                                Receipt::Unverified => Err((
                                    format!(
                                        "custody receipt from {to} is not verified; retaining custody"
                                    ),
                                    false,
                                )),
                            };
                            finish(
                                id,
                                flight,
                                outcome,
                                &mut chooser,
                                &mut graph,
                                &mut failed_contacts,
                            );
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
                            let permanent =
                                matches!(reason, Failure::Empty | Failure::SelfAddressed);
                            finish(
                                id,
                                flight,
                                Err((format!("{reason:?}"), permanent)),
                                &mut chooser,
                                &mut graph,
                                &mut failed_contacts,
                            );
                        }
                    }
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
                        match result {
                            Ok(()) => {
                                chooser.record(peer, flight.bearer, true, now);
                                let (key, evidence) =
                                    graph.record_delivery(cfg.me, peer, route_bearer(flight.bearer), true, now);
                                if let Err(error) = store.save_contact_evidence(key, evidence) {
                                    log(format!("store: {error}"));
                                }
                                match store.delivered(id, false, "internet", now) {
                                    Ok(()) => log(format!(
                                        "published bulletin {} to {peer} over the internet",
                                        short(&id)
                                    )),
                                    Err(e) => log(format!("store: {e}")),
                                }
                            }
                            Err(e) => {
                                chooser.record(peer, flight.bearer, false, now);
                                let (key, evidence) =
                                    graph.record_delivery(cfg.me, peer, route_bearer(flight.bearer), false, now);
                                if let Err(error) = store.save_contact_evidence(key, evidence) {
                                    log(format!("store: {error}"));
                                }
                                let reason = match e {
                                    NetError::Busy {
                                        retry_after,
                                        reason,
                                    } => format!("busy for {retry_after} s: {reason}"),
                                    NetError::Rejected(r) => format!("rejected: {r}"),
                                    other => other.to_string(),
                                };
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
                    let outcome = match result {
                        Ok(()) => Ok(()),
                        Err(NetError::Busy {
                            retry_after,
                            reason,
                        }) => Err((format!("busy for {retry_after} s: {reason}"), false)),
                        Err(NetError::Rejected(r)) => Err((format!("rejected: {r}"), false)),
                        Err(e) => Err((e.to_string(), false)),
                    };
                    if let Some(flight) = in_flight.remove(&(id, peer)) {
                        if outcome.is_ok() {
                            control.clear_request(id, peer);
                        }
                        finish(
                            id,
                            flight,
                            outcome,
                            &mut chooser,
                            &mut graph,
                            &mut failed_contacts,
                        );
                    }
                }
            }
            Some((id, peer, result)) = modem_rx.recv() => {
                let outcome = match result {
                    Ok(()) => Ok(()),
                    Err(e) if e.starts_with("refused: busy for ") => Err((e, false)),
                    Err(e) if e.starts_with("refused: ") => Err((e, true)),
                    Err(e) => Err((e, false)),
                };
                if let Some(flight) = in_flight.remove(&(id, peer)) {
                    if outcome.is_ok() {
                        control.clear_request(id, peer);
                    }
                    finish(
                        id,
                        flight,
                        outcome,
                        &mut chooser,
                        &mut graph,
                        &mut failed_contacts,
                    );
                }
            }
            _ = tick.tick() => {
                match live.reload_if_changed() {
                    Some(Ok(())) => log("settings file changed; applied"),
                    Some(Err(e)) => log(format!("settings file not applied, keeping the settings in use: {e}")),
                    None => {}
                }
                if live.version() != live_version {
                    live_version = live.version();
                    notify.send("settings");
                    let snapshot = live.get();
                    chooser.set_costs(snapshot.costs);
                    chooser.set_half_life(snapshot.evidence_half_life_secs);
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
                }
                let now = unix_now();
                let links = net.as_ref().map(|n| n.connected()).unwrap_or_default();
                let current_links: BTreeSet<Callsign> = links.iter().copied().collect();
                for peer in &current_links {
                    for (from, to) in [(cfg.me, *peer), (*peer, cfg.me)] {
                        if let Err(error) = graph.observe_live_link(LiveContact {
                            from,
                            to,
                            bearer: RouteBearer::Internet,
                            rate_bps: 10_000_000,
                            capacity_bytes: 64 * 1024 * 1024,
                            success_permyriad: Some(9_900),
                            flags: FLAG_INTERNET,
                            observed_at: now,
                        }) {
                            log(format!("ignored internet contact {from} -> {to}: {error}"));
                        }
                    }
                    let advert_key = (*peer, RouteBearer::Internet);
                    let advertised_at = advertised_live.get(&advert_key).copied().unwrap_or(0);
                    if now.saturating_sub(advertised_at) >= 10 * 60 {
                        match live_advert(
                            cfg,
                            &live.get().relay,
                            *peer,
                            RouteBearer::Internet,
                            10_000_000,
                            64 * 1024 * 1024,
                            now,
                        ) {
                            Ok(advert) => {
                                if let Err(error) =
                                    control.observe_local(advert, now.saturating_mul(1_000))
                                {
                                    log(format!("ignored local CONTACT advert: {error}"));
                                } else {
                                    advertised_live.insert(advert_key, now);
                                }
                            }
                            Err(error) => log(format!("ignored local CONTACT advert: {error}")),
                        }
                    }
                    if !known_internet_links.contains(peer) {
                        let (key, evidence) =
                            graph.record_delivery(cfg.me, *peer, RouteBearer::Internet, true, now);
                        if let Err(error) = store.save_contact_evidence(key, evidence) {
                            log(format!("store: {error}"));
                        }
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
                graph.prune(now);
                match control.due_contacts(now.saturating_mul(1_000), now) {
                    Ok(messages) => {
                        for payload in messages {
                            broadcast_sync(
                                net.as_ref(),
                                &radio_cmd,
                                radio_up,
                                &known_internet_links,
                                payload,
                            );
                        }
                    }
                    Err(error) => log(format!("could not encode CONTACT advert: {error}")),
                }
                match store.suspect_due(now) {
                    Ok(suspects) => {
                        for record in suspects {
                            if in_flight.keys().any(|(id, _)| *id == record.id) {
                                continue;
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
                        let empty_route = || Route {
                            hops: Vec::new(),
                            arrival: now,
                            airtime_millis: 0,
                            success_probability: 1.0,
                            risk_cost: 0.0,
                        };
                        // A peer asked for this bulletin over SYNC: send it on the internet.
                        if let Some(peer) = control.target_for(r.id, now) {
                            if net.as_ref().is_some_and(|network| network.is_connected(peer))
                                && !in_flight.contains_key(&(r.id, peer))
                            {
                                in_flight.insert(
                                    (r.id, peer),
                                    InFlight {
                                        bearer: Bearer::Internet,
                                        peer,
                                        route: empty_route(),
                                        object_bytes: object.len() as u64,
                                    },
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
                                InFlight {
                                    bearer: Bearer::Radio,
                                    peer: bulletin_peer,
                                    route: empty_route(),
                                    object_bytes: object.len() as u64,
                                },
                            );
                            let _ = radio_cmd.send(RadioCmd::Broadcast {
                                object,
                                precedence: r.precedence,
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
                                    InFlight {
                                        bearer: Bearer::Internet,
                                        peer: *peer,
                                        route: empty_route(),
                                        object_bytes: object.len() as u64,
                                    },
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
                    if modem_up {
                        let rate_bps = cfg
                            .modem
                            .as_ref()
                            .map_or(1_200, arq::ArqConfig::estimated_rate_bps);
                        let _ = graph.observe_live_link(LiveContact {
                            from: cfg.me,
                            to: route_destination,
                            bearer: RouteBearer::Modem,
                            rate_bps,
                            capacity_bytes: arq::MAX_OBJECT as u64,
                            success_permyriad: Some(7_000),
                            flags: 0,
                            observed_at: now,
                        });
                    }
                    let mut excluded: Vec<ContactKey> = failed_contacts
                        .entry(r.id)
                        .or_default()
                        .iter()
                        .filter_map(|(contact, until)| (*until > now).then_some(*contact))
                        .collect();
                    excluded.extend(
                        graph
                            .outgoing(cfg.me, now)
                            .filter(|contact| match contact.key.bearer {
                                RouteBearer::Radio => !radio_up || !rf_ok,
                                RouteBearer::Internet => net
                                    .as_ref()
                                    .is_none_or(|network| !network.is_connected(contact.key.to)),
                                RouteBearer::Modem => !modem_up || !rf_ok,
                            })
                            .map(|contact| contact.key),
                    );
                    let routed_len = object.len()
                        + usize::from(r.direction == Direction::Relay)
                            * (10 + 6 * (usize::from(r.hop_count.unwrap_or(0)) + 1));
                    let make_request = || RouteRequest {
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
                        urgent_min_gain: live.get().relay.urgent_min_gain,
                        ..RoutingPolicy::default()
                    };
                    let mut plan = plan_routes(&graph, &make_request(), policy);
                    if plan.is_err() && radio_up && rf_ok {
                        let rate = live.get().radio.bitrate;
                        let _ = graph.observe_live_link(LiveContact {
                            from: cfg.me,
                            to: route_destination,
                            bearer: RouteBearer::Radio,
                            rate_bps: rate,
                            capacity_bytes: (u64::from(rate) * 600 / 8).max(routed_len as u64),
                            success_permyriad: Some(3_000),
                            flags: 0,
                            observed_at: now,
                        });
                        plan = plan_routes(&graph, &make_request(), policy);
                    }
                    if plan.is_err()
                        && net
                            .as_ref()
                            .is_some_and(|network| network.is_connected(route_destination))
                    {
                        let _ = graph.observe_live_link(LiveContact {
                            from: cfg.me,
                            to: route_destination,
                            bearer: RouteBearer::Internet,
                            rate_bps: 10_000_000,
                            capacity_bytes: 64 * 1024 * 1024,
                            success_permyriad: Some(9_900),
                            flags: FLAG_INTERNET,
                            observed_at: now,
                        });
                        plan = plan_routes(&graph, &make_request(), policy);
                    }
                    let Ok(plan) = plan else { continue };
                    for route in plan.active {
                        let Some(first) = route.hops.first() else {
                            continue;
                        };
                        if first.depart > now {
                            continue;
                        }
                        let (bearer, available) = match first.contact.bearer {
                            RouteBearer::Radio => (Bearer::Radio, radio_up && rf_ok),
                            RouteBearer::Internet => (
                                Bearer::Internet,
                                net.as_ref().is_some_and(|network| network.is_connected(first.contact.to)),
                            ),
                            RouteBearer::Modem => (Bearer::Modem, modem_up && rf_ok),
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
                        in_flight.insert(
                            (r.id, peer),
                            InFlight {
                                bearer,
                                peer,
                                route,
                                object_bytes: routed_len as u64,
                            },
                        );
                        match bearer {
                            Bearer::Radio => {
                                radio_ids.insert((hm_xfer::object_id(&wire_object), peer), r.id);
                                let _ = radio_cmd.send(RadioCmd::Send {
                                    object: wire_object,
                                    to: peer,
                                    precedence: r.precedence,
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
                st.estimates = chooser
                    .peers()
                    .into_iter()
                    .flat_map(|p| {
                        [Bearer::Radio, Bearer::Internet, Bearer::Modem].map(|b| (p, b.name(), chooser.estimate(p, b, now)))
                    })
                    .collect();
            }
        }
    }
}
