//! Start and lazily open the internet (QUIC) bearer.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};

use hm_ident::Identity;
use hm_net::{Net, NetConfig};
use hm_store::Store;
use hm_wire::Callsign;

use super::accept::{accept, AcceptanceGate};
use super::live::LiveConfig;
use super::types::{log, NodeConfig, Notify, Status};

#[allow(clippy::too_many_arguments)]
pub(crate) fn start_net(
    cfg: &NodeConfig,
    store: &Arc<Store>,
    live: &Arc<LiveConfig>,
    notify: &Notify,
    listen: SocketAddr,
    net_control_tx: tokio::sync::mpsc::UnboundedSender<(Callsign, Vec<u8>)>,
) -> io::Result<Arc<Net>> {
    let (store, gate_live, me, key_call, gate_identity, gate_notify, gate_relay) = (
        Arc::clone(store),
        Arc::clone(live),
        cfg.me,
        cfg.key.call,
        Identity::from_secret(cfg.key.identity.secret()),
        notify.clone(),
        cfg.relay.clone(),
    );
    // Weak so the accept gate does not keep Net (and the store) alive after stop.
    let net_slot: Arc<Mutex<Weak<Net>>> = Arc::new(Mutex::new(Weak::new()));
    let gate_net = Arc::clone(&net_slot);
    let gate: hm_net::Accept = Arc::new(move |via, obj| {
        let current = gate_live.get();
        let peer_key = gate_net
            .lock()
            .expect("lock")
            .upgrade()
            .and_then(|n| n.public_key_of(via));
        accept(
            AcceptanceGate {
                store: &store,
                notify: &gate_notify,
                trust: &current.trust,
                me,
                key_call,
                identity: &gate_identity,
                relay: &gate_relay,
            },
            via,
            &obj,
            peer_key.as_ref(),
        )
        .verdict()
    });
    let control: hm_net::Control = Arc::new(move |from, payload| {
        let _ = net_control_tx.send((from, payload));
    });
    let current = live.get();
    let net = Net::start_with_control(
        NetConfig {
            me: cfg.me,
            secret: cfg.key.identity.secret(),
            trust: current.trust.iter().collect(),
            listen,
            dial: current.peers,
            open: cfg.internet.as_ref().is_some_and(|i| i.open_hub),
        },
        gate,
        control,
    )?;
    *net_slot.lock().expect("lock") = Arc::downgrade(&net);
    Ok(net)
}

/// Open the internet stack when dial peers appear after a radio-only start.
pub(crate) fn ensure_internet(
    cfg: &NodeConfig,
    store: &Arc<Store>,
    live: &Arc<LiveConfig>,
    notify: &Notify,
    net: &mut Option<Arc<Net>>,
    net_control_tx: &tokio::sync::mpsc::UnboundedSender<(Callsign, Vec<u8>)>,
    status: &Mutex<Status>,
) {
    let peers = live.get().peers;
    if net.is_some() || peers.is_empty() {
        return;
    }
    let listen = cfg
        .internet
        .as_ref()
        .map(|ic| ic.listen)
        .unwrap_or_else(|| "0.0.0.0:0".parse().expect("valid"));
    match start_net(cfg, store, live, notify, listen, net_control_tx.clone()) {
        Ok(n) => {
            let addr = n.local_addr().ok();
            status.lock().expect("lock").internet_listen = addr;
            log(format!(
                "internet on {} (started for dial peers)",
                addr.map_or_else(|| "unknown".into(), |a| a.to_string())
            ));
            *net = Some(n);
            notify.send("status");
        }
        Err(error) => log(format!("could not start internet for dial peers: {error}")),
    }
}
