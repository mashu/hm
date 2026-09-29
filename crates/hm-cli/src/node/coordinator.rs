//! The delivery coordinator: the I/O shell around [`Node`].
//!
//! Everything the station decides is in [`Node`], a state machine without
//! I/O. The coordinator feeds it what happens (radio events, internet and
//! modem results, SYNC from internet peers, a tick every second with the
//! links up and the modem's state, changed settings) and carries out what it
//! asks for: radio commands, internet and modem transfers, SYNC to internet
//! peers. It also reloads the settings file, restarts the internet endpoint
//! when the settings need one, and publishes the node's status.

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use hm_ident::Identity;
use hm_net::{Net, NetError};
use hm_node::{Command, Input, ModemSpec, Node, NodeIdentity, NodeStatus, RadioCmd, RadioEvt, Transfer};
use hm_store::Store;
use hm_wire::{Callsign, ObjectId};

use super::arq;
use super::internet::ensure_internet;
use super::live::LiveConfig;
use super::types::{log, NodeConfig, Notify, Status};
use crate::station::unix_now;

type Done = (ObjectId, Callsign, Transfer);

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
    let seed = cfg
        .seed
        .unwrap_or_else(|| getrandom::u64().unwrap_or_else(|_| unix_now()));
    let identity = NodeIdentity {
        me: cfg.me,
        key_call: cfg.key.call,
        identity: Identity::from_secret(cfg.key.identity.secret()),
        schedules: cfg.schedules.clone(),
        has_internet: cfg.internet.is_some(),
        modem: cfg.modem.as_ref().map(|m| ModemSpec {
            rate_bps: m.estimated_rate_bps(),
            max_object: arq::MAX_OBJECT as u64,
        }),
        radio_via: cfg.radio.as_ref().map(|r| r.describe()),
        seed,
    };
    let mut node = Node::new(
        identity,
        live.get().node_settings(),
        store.clone(),
        notify.observer(),
        unix_now(),
    );
    let mut live_version = live.version();
    let (net_tx, mut net_rx) = tokio::sync::mpsc::unbounded_channel::<Done>();
    let (modem_tx, mut modem_rx) = tokio::sync::mpsc::unbounded_channel::<Done>();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut out = Vec::new();
    let mut shown = NodeStatus::default();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            Some(event) = radio_evt.recv() => node.handle(unix_now(), Input::Radio(event), &mut out),
            Some((from, payload)) = net_control.recv() => {
                let peer_key = net.as_ref().and_then(|n| n.public_key_of(from));
                node.handle(unix_now(), Input::NetSync { from, payload, peer_key }, &mut out);
            }
            Some((id, peer, result)) = net_rx.recv() => {
                node.handle(unix_now(), Input::NetDone { id, peer, result }, &mut out);
            }
            Some((id, peer, result)) = modem_rx.recv() => {
                node.handle(unix_now(), Input::ModemDone { id, peer, result }, &mut out);
            }
            _ = tick.tick() => {
                match live.reload_if_changed() {
                    Some(Ok(())) => log("settings file changed; applied"),
                    Some(Err(e)) => log(format!("settings file not applied, keeping the settings in use: {e}")),
                    None => {}
                }
                let now = unix_now();
                if live.version() != live_version {
                    live_version = live.version();
                    notify.send("settings");
                    ensure_internet(cfg, store, live, notify, net, &net_control_tx, status);
                    let snapshot = live.get();
                    if let Some(n) = net.as_ref() {
                        n.set_trust(&snapshot.trust.iter().collect::<Vec<_>>());
                        n.set_dial(snapshot.peers.clone());
                    }
                    node.handle(now, Input::Settings(Box::new(snapshot.node_settings())), &mut out);
                }
                let links = net.as_ref().map(|n| n.connected()).unwrap_or_default();
                let modem_state = modem.as_ref().map(|m| {
                    let s = m.status();
                    (s.up, s.peer)
                });
                node.handle(now, Input::Tick { links, modem: modem_state }, &mut out);
                let current = node.status(now);
                publish_status(status, notify, &shown, &current);
                shown = current;
            }
        }
        for command in out.drain(..) {
            execute(
                command,
                &radio_cmd,
                net.as_ref(),
                modem.as_ref(),
                &net_tx,
                &modem_tx,
            );
        }
    }
}

/// Carry out what the node asked for.
fn execute(
    command: Command,
    radio: &mpsc::Sender<RadioCmd>,
    net: Option<&Arc<Net>>,
    modem: Option<&Arc<arq::Arq>>,
    net_done: &tokio::sync::mpsc::UnboundedSender<Done>,
    modem_done: &tokio::sync::mpsc::UnboundedSender<Done>,
) {
    match command {
        Command::Radio(cmd) => {
            let _ = radio.send(cmd);
        }
        Command::NetDeliver { id, peer, object } => match net.cloned() {
            Some(network) => {
                let done = net_done.clone();
                tokio::spawn(async move {
                    let result = network.deliver(peer, &object).await;
                    let _ = done.send((id, peer, net_transfer(result)));
                });
            }
            None => {
                let _ = net_done.send((id, peer, net_transfer(Err(NetError::NotConnected))));
            }
        },
        Command::ModemDeliver { id, peer, object } => match modem.cloned() {
            Some(modem) => {
                let done = modem_done.clone();
                tokio::spawn(async move {
                    let result = modem.deliver(peer, object).await;
                    let _ = done.send((id, peer, result));
                });
            }
            None => {
                let _ = modem_done.send((id, peer, Transfer::Failed("no modem".into())));
            }
        },
        Command::NetSync { peer, payload } => {
            if let Some(network) = net.cloned() {
                tokio::spawn(async move {
                    let _ = network.send_control(peer, &payload).await;
                });
            }
        }
    }
}

/// How an internet transfer ended, as the node sees it. A peer's refusal
/// may not hold for the next try (its limits, its trust list), so it is not
/// taken as permanent.
fn net_transfer(result: Result<(), NetError>) -> Transfer {
    match result {
        Ok(()) => Transfer::Delivered,
        Err(NetError::Busy { retry_after, reason }) => Transfer::Busy {
            retry_after: u64::from(retry_after),
            reason,
        },
        Err(NetError::Rejected(reason)) => Transfer::Refused {
            reason,
            permanent: false,
        },
        Err(error) => Transfer::Failed(error.to_string()),
    }
}

/// Show the node's status, and tell listeners when what they watch changed.
fn publish_status(status: &Mutex<Status>, notify: &Notify, shown: &NodeStatus, current: &NodeStatus) {
    let mut st = status.lock().expect("lock");
    if (shown.radio, &shown.radio_via, &shown.internet_peers)
        != (current.radio, &current.radio_via, &current.internet_peers)
        || (shown.modem, shown.modem_peer) != (current.modem, current.modem_peer)
    {
        notify.send("status");
    }
    st.radio = current.radio;
    st.radio_via = current.radio_via.clone();
    st.internet_peers = current.internet_peers.clone();
    st.modem = current.modem;
    st.modem_peer = current.modem_peer;
    st.estimates = current.estimates.clone();
    st.heard = current.heard.clone();
}
