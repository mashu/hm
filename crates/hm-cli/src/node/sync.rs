//! Contact adverts and pairwise holdings SYNC over radio and internet.

use std::collections::BTreeSet;
use std::sync::{mpsc, Arc};

use hm_net::Net;
use hm_route::{Bearer as RouteBearer, ContactGraph, ScheduledContact};
use hm_store::Store;
use hm_wire::{Callsign, ContactAdvert, ContactBearer, Dest, FLAG_INTERNET, FLAG_MAILBOX, FLAG_RELAY};

use super::choose::Bearer;
use super::control::{sign_contact, ControlAction, ControlPlane};
use super::live::LiveConfig;
use super::radio::RadioCmd;
use super::types::{log, short, NodeConfig};
use crate::station::unix_now;

pub(crate) fn send_sync(
    net: Option<&Arc<Net>>,
    radio: &mpsc::Sender<RadioCmd>,
    bearer: Bearer,
    to: Callsign,
    payload: Vec<u8>,
) {
    match bearer {
        Bearer::Radio => {
            let _ = radio.send(RadioCmd::Sync {
                to: Dest::Station(to),
                payload,
            });
        }
        Bearer::Internet => {
            let Some(network) = net.cloned() else {
                return;
            };
            tokio::spawn(async move {
                let _ = network.send_control(to, &payload).await;
            });
        }
        Bearer::Modem => {}
    }
}

pub(crate) fn broadcast_sync(
    net: Option<&Arc<Net>>,
    radio: &mpsc::Sender<RadioCmd>,
    radio_up: bool,
    internet_peers: &BTreeSet<Callsign>,
    payload: Vec<u8>,
) {
    if radio_up {
        let _ = radio.send(RadioCmd::Sync {
            to: Dest::Broadcast,
            payload: payload.clone(),
        });
    }
    for peer in internet_peers {
        send_sync(net, radio, Bearer::Internet, *peer, payload.clone());
    }
}

pub(crate) fn receive_sync(
    control: &mut ControlPlane,
    graph: &mut ContactGraph,
    store: &Store,
    live: &LiveConfig,
    cfg: &NodeConfig,
    from: Callsign,
    payload: &[u8],
) -> Vec<ControlAction> {
    match control.receive(
        from,
        payload,
        &live.get().trust,
        (cfg.me, cfg.key.identity.public()),
        store,
        graph,
        cfg.relay.enabled || cfg.relay.mailbox,
        unix_now(),
    ) {
        Ok(actions) => actions,
        Err(error) => {
            log(format!("ignored SYNC from {from}: {error}"));
            vec![]
        }
    }
}

pub(crate) fn apply_sync_actions(
    actions: Vec<ControlAction>,
    net: Option<&Arc<Net>>,
    radio: &mpsc::Sender<RadioCmd>,
    bearer: Bearer,
    from: Callsign,
) {
    for action in actions {
        match action {
            ControlAction::Reply(payload) => send_sync(net, radio, bearer, from, payload),
            ControlAction::Requested { id, peer } => {
                log(format!("{} requested custody of {}", peer, short(&id)));
            }
        }
    }
}

pub(crate) fn advertised_flags(cfg: &NodeConfig) -> u8 {
    let mut flags = if cfg.internet.is_some() { FLAG_INTERNET } else { 0 };
    if cfg.relay.enabled {
        flags |= FLAG_RELAY;
    }
    if cfg.relay.mailbox {
        flags |= FLAG_MAILBOX;
    }
    flags
}

pub(crate) fn scheduled_advert(
    cfg: &NodeConfig,
    schedule: ScheduledContact,
    sequence: u32,
) -> Result<Option<ContactAdvert>, String> {
    if schedule.from != cfg.me {
        return Ok(None);
    }
    let start = u32::try_from(schedule.start).map_err(|_| "contact start exceeds wire range")?;
    let end = u32::try_from(schedule.end).map_err(|_| "contact end exceeds wire range")?;
    let capacity_bytes =
        u32::try_from(schedule.capacity_bytes).map_err(|_| "contact capacity exceeds wire range")?;
    let advert = ContactAdvert {
        origin: cfg.me,
        sequence,
        start,
        end,
        peer: schedule.to,
        bearer: ContactBearer::from(schedule.bearer),
        success_permyriad: schedule.success_permyriad.unwrap_or(5_000),
        rate_bps: schedule.rate_bps,
        capacity_bytes,
        flags: schedule.flags | advertised_flags(cfg),
        signature: [0; 64],
    };
    sign_contact(&cfg.key.identity, advert)
        .map(Some)
        .map_err(str::to_string)
}

pub(crate) fn live_advert(
    cfg: &NodeConfig,
    peer: Callsign,
    bearer: RouteBearer,
    rate_bps: u32,
    capacity_bytes: u64,
    now: u64,
) -> Result<ContactAdvert, String> {
    let start = u32::try_from(now).map_err(|_| "contact start exceeds wire range")?;
    let end = u32::try_from(now.saturating_add(20 * 60)).map_err(|_| "contact end exceeds wire range")?;
    let capacity_bytes = u32::try_from(capacity_bytes.min(u64::from(u32::MAX))).expect("bounded to u32");
    sign_contact(
        &cfg.key.identity,
        ContactAdvert {
            origin: cfg.me,
            sequence: start,
            start,
            end,
            peer,
            bearer: ContactBearer::from(bearer),
            success_permyriad: if bearer == RouteBearer::Internet {
                9_900
            } else {
                7_000
            },
            rate_bps,
            capacity_bytes,
            flags: advertised_flags(cfg),
            signature: [0; 64],
        },
    )
    .map_err(str::to_string)
}
