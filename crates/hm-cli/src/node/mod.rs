//! The station daemon (`hm node`).
//!
//! One process owns the store and the bearers:
//!
//! - **radio** (optional): a thread running the transfer engine over a KISS
//!   TNC or the built-in modem, reconnecting if the link goes away. It also
//!   sends a signed BEACON now and then, and keeps the table of stations heard;
//! - **internet** (optional): QUIC links to other stations (`hm-net`);
//! - **ARQ modem** (optional): VARA, Mercury or ARDOP host interfaces.
//!
//! A coordinator routes every due message over a probabilistic contact graph,
//! reserves capacity along the selected path, and transfers custody to its
//! next hop only after a verified receipt. RF and ARQ transmissions are gated
//! by [`rf_policy`]: the end-to-end origin must be this station or a trusted
//! station. Everything that arrives over any bearer goes through one
//! acceptance gate for destination delivery or relay custody.
//!
//! The HTTP thread serves the JSON API and web page behind an access token.

mod api;
pub mod arq;
pub mod choose;
mod control;
pub mod heard;
pub mod live;
mod rf_policy;

mod accept;
mod coordinator;
mod internet;
mod radio;
mod sync;
mod types;

pub use types::{
    addressed_to_us, log, radio_config, InternetConfig, NodeConfig, NodeHandle, Notify, RadioBuilder,
    RadioConfig, RadioLink, Status,
};

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use hm_ident::Identity;
use hm_store::Store;

use accept::{accept, AcceptanceGate};
use coordinator::coordinator;
use internet::start_net;
use live::LiveConfig;
use radio::radio_thread;

/// Open the store, bind HTTP, start the bearers and the coordinator.
pub fn start(cfg: NodeConfig) -> io::Result<NodeHandle> {
    let store = Arc::new(Store::open(&cfg.store).map_err(|e| io::Error::other(e.to_string()))?);
    let listener = TcpListener::bind(cfg.http)?;
    listener.set_nonblocking(true)?;
    let http_addr = listener.local_addr()?;
    if !http_addr.ip().is_loopback() {
        log(format!(
            "API on {http_addr} is reachable over plain HTTP; keep it private or put it behind an HTTPS reverse proxy"
        ));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let (radio_evt_tx, radio_evt_rx) = tokio::sync::mpsc::unbounded_channel();
    let (radio_cmd_tx, radio_cmd_rx) = mpsc::channel();
    let status = Arc::new(Mutex::new(Status::default()));
    let notify = Notify::new();
    let live = Arc::new(
        LiveConfig::new(cfg.live.clone(), cfg.config_file.clone()).with_overrides(cfg.overrides.clone()),
    );
    let cfg = Arc::new(cfg);

    let radio = match cfg.radio.is_some() || cfg.radio_builder.is_some() {
        true => {
            let (cfg, live, stop) = (cfg.clone(), live.clone(), stop.clone());
            Some(thread::Builder::new().name("radio".into()).spawn(move || {
                radio_thread(&cfg, &live, radio_cmd_rx, radio_evt_tx, &stop);
            })?)
        }
        false => None,
    };

    // The internet endpoint is bound here so its address is known on return.
    // If the node starts without internet, the coordinator can still open one
    // later when dial peers are added (no restart).
    let (addr_tx, addr_rx) = mpsc::channel::<io::Result<Option<SocketAddr>>>();
    let main = {
        let (cfg, store, status, live, notify) = (
            cfg.clone(),
            store.clone(),
            status.clone(),
            live.clone(),
            notify.clone(),
        );
        thread::Builder::new().name("node".into()).spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let (net_control_tx, net_control_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut net = match &cfg.internet {
                    None => None,
                    Some(ic) => {
                        match start_net(&cfg, &store, &live, &notify, ic.listen, net_control_tx.clone()) {
                            Ok(n) => Some(n),
                            Err(e) => {
                                let _ = addr_tx.send(Err(e));
                                return;
                            }
                        }
                    }
                };
                let modem = cfg.modem.clone().map(|mc| {
                    let (store, gate_live, me, key_call, gate_identity, gate_notify) = (
                        store.clone(),
                        live.clone(),
                        cfg.me,
                        cfg.key.call,
                        Identity::from_secret(cfg.key.identity.secret()),
                        notify.clone(),
                    );
                    let gate: hm_net::Accept = Arc::new(move |via, obj| {
                        let current = gate_live.get();
                        accept(
                            AcceptanceGate {
                                store: &store,
                                notify: &gate_notify,
                                trust: &current.trust,
                                me,
                                key_call,
                                identity: &gate_identity,
                                relay: &current.relay,
                            },
                            via,
                            &obj,
                            None,
                        )
                        .verdict()
                    });
                    let id = hm_ident::Identity::from_secret(cfg.key.identity.secret());
                    Arc::new(arq::Arq::start(mc, cfg.me, id, live.clone(), gate))
                });
                let _ = addr_tx.send(Ok(net.as_ref().and_then(|n| n.local_addr().ok())));
                if let Some(n) = &net {
                    status.lock().expect("lock").internet_listen = n.local_addr().ok();
                }
                let app = api::router(api::AppState {
                    store: store.clone(),
                    cfg: cfg.clone(),
                    status: status.clone(),
                    live: live.clone(),
                    notify: notify.clone(),
                    bulletin_publishes: std::sync::Arc::new(std::sync::Mutex::new(
                        std::collections::VecDeque::new(),
                    )),
                });
                let http_listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                let mut http_shutdown = shutdown_rx.clone();
                tokio::spawn(async move {
                    let _ = axum::serve(http_listener, app)
                        .with_graceful_shutdown(async move {
                            let _ = http_shutdown.changed().await;
                        })
                        .await;
                });
                coordinator(
                    &cfg,
                    &store,
                    &mut net,
                    modem,
                    radio_cmd_tx,
                    radio_evt_rx,
                    net_control_tx,
                    net_control_rx,
                    &status,
                    &live,
                    &notify,
                    shutdown_rx,
                )
                .await;
                if let Some(n) = net {
                    n.close();
                }
            });
        })?
    };
    let internet_addr = addr_rx
        .recv()
        .map_err(|_| io::Error::other("node thread failed to start"))??;
    log(format!(
        "node {} up: radio {}, internet {}, modem {}, web http://{http_addr}/",
        cfg.me,
        cfg.radio.as_ref().map_or("off".to_string(), |r| r.describe()),
        internet_addr.map_or("off".to_string(), |a| format!("on {a}")),
        cfg.modem.as_ref().map_or("off".to_string(), |m| m.describe()),
    ));
    Ok(NodeHandle {
        http_addr,
        internet_addr,
        stop,
        main,
        radio,
        shutdown,
    })
}

#[cfg(test)]
mod tests {
    use super::accept::{accept, Acceptance, AcceptanceGate};
    use super::{addressed_to_us, Notify};
    use crate::config::RelaySettings;
    use crate::files::{KeyFile, Trust};
    use crate::station::build_bundle;
    use hm_bundle::Precedence;
    use hm_store::{Direction, Store};
    use hm_wire::Callsign;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    #[test]
    fn every_ssid_is_a_station_of_its_own() {
        // A node with its own key for SA0KAM-1 takes mail for SA0KAM-1 only.
        let (me, key) = (call("SA0KAM-1"), call("SA0KAM-1"));
        assert!(addressed_to_us(call("SA0KAM-1"), me, key));
        assert!(!addressed_to_us(call("SA0KAM-2"), me, key));
        assert!(!addressed_to_us(call("SA0KAM"), me, key));
        // A node on a key for the bare callsign, run as -1 with --ssid, also
        // takes mail for the bare callsign, but still not for another SSID.
        let key = call("SA0KAM");
        assert!(addressed_to_us(call("SA0KAM-1"), me, key));
        assert!(addressed_to_us(call("SA0KAM"), me, key));
        assert!(!addressed_to_us(call("SA0KAM-2"), me, key));
    }

    #[test]
    fn relay_accepts_untrusted_destination_when_origin_verifies() {
        let sender = KeyFile::generate(call("SA0KAM")).unwrap();
        let relay = KeyFile::generate(call("SM0R1")).unwrap();
        let destination = call("SO5KM-1");
        let path = std::env::temp_dir().join(format!("hm-relay-trust-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let notify = Notify::new();
        let settings = RelaySettings {
            enabled: true,
            ..RelaySettings::default()
        };
        let bundle = build_bundle(
            &sender,
            sender.call,
            destination,
            "relay policy",
            None,
            Precedence::Routine,
            Some(1),
        )
        .unwrap()
        .to_vec();

        let mut trust = Trust::default();
        let rejected = accept(
            AcceptanceGate {
                store: &store,
                notify: &notify,
                trust: &trust,
                me: relay.call,
                key_call: relay.call,
                identity: &relay.identity,
                relay: &settings,
            },
            sender.call,
            &bundle,
            None,
        );
        assert!(matches!(
            rejected,
            Acceptance::Rejected(reason) if reason == "relay requires a verified sender"
        ));
        assert!(store.list(Direction::Relay, 10).unwrap().is_empty());

        trust.insert(sender.call, sender.identity.public());
        let accepted = accept(
            AcceptanceGate {
                store: &store,
                notify: &notify,
                trust: &trust,
                me: relay.call,
                key_call: relay.call,
                identity: &relay.identity,
                relay: &settings,
            },
            sender.call,
            &bundle,
            None,
        );
        assert!(matches!(accepted, Acceptance::Stored));
        assert_eq!(store.list(Direction::Relay, 10).unwrap().len(), 1);

        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unverified_final_delivery_stores_without_a_receipt() {
        let sender = KeyFile::generate(call("SA0KAM")).unwrap();
        let me = KeyFile::generate(call("SO5KM-1")).unwrap();
        let path = std::env::temp_dir().join(format!("hm-unverified-receipt-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let notify = Notify::new();
        let bundle = build_bundle(
            &sender,
            sender.call,
            me.call,
            "hello",
            None,
            Precedence::Routine,
            Some(1),
        )
        .unwrap()
        .to_vec();
        let trust = Trust::default();
        let accepted = accept(
            AcceptanceGate {
                store: &store,
                notify: &notify,
                trust: &trust,
                me: me.call,
                key_call: me.call,
                identity: &me.identity,
                relay: &RelaySettings::default(),
            },
            sender.call,
            &bundle,
            None,
        );
        assert!(matches!(accepted, Acceptance::Stored));
        assert_eq!(store.list(Direction::In, 10).unwrap().len(), 1);
        assert!(store.list(Direction::Out, 10).unwrap().is_empty());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bulletin_stores_without_a_receipt_even_when_verified() {
        let sender = KeyFile::generate(call("SA0KAM")).unwrap();
        let me = KeyFile::generate(call("SO5KM-1")).unwrap();
        let path = std::env::temp_dir().join(format!("hm-bulletin-accept-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let notify = Notify::new();
        let bundle =
            crate::station::build_bulletin(&sender, sender.call, "SK-EMCOMM", "net open", Some("check-in"))
                .unwrap()
                .to_vec();
        let mut trust = Trust::default();
        trust.insert(sender.call, sender.identity.public());
        let accepted = accept(
            AcceptanceGate {
                store: &store,
                notify: &notify,
                trust: &trust,
                me: me.call,
                key_call: me.call,
                identity: &me.identity,
                relay: &RelaySettings::default(),
            },
            sender.call,
            &bundle,
            None,
        );
        assert!(matches!(accepted, Acceptance::Stored));
        assert_eq!(store.list(Direction::In, 10).unwrap().len(), 1);
        // No kind-5 receipt queued.
        assert!(store.list(Direction::Out, 10).unwrap().is_empty());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
