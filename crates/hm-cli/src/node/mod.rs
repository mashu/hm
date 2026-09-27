//! The station daemon (`hm node`).
//!
//! One process owns the store and the bearers:
//!
//! - **radio** (optional): a thread running the transfer engine over a KISS
//!   TNC or the built-in modem, reconnecting if the link goes away. It also
//!   sends a signed BEACON now and then, and keeps the table of stations heard;
//! - **internet** (optional): QUIC links to other stations (`hm-net`).
//!
//! A coordinator hands every due outbound message to one bearer, chosen per
//! delivery by [`choose::Chooser`] from the bearers that can reach the
//! destination right now. With the default costs radio carries the traffic
//! whenever it delivers, and the internet takes over only while it does not.
//! Everything that arrives, over either bearer, goes through one check
//! (addressed to us, signature against the trusted keys) into the store, once.
//!
//! The HTTP thread serves the JSON API and web page behind an access token.

mod api;
pub mod choose;
mod control;
pub mod heard;
pub mod live;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hm_bundle::{Address, Bundle, Kind, Opened};
use hm_core::{DetRng, Input, Machine, Millis, Output};
use hm_ident::Identity;
use hm_net::{Net, NetConfig, NetError, Verdict};
use hm_route::{
    plan_routes, BeaconObservation, Bearer as RouteBearer, ContactGraph, ContactKey, GraphConfig,
    LiveContact, Route, RouteRequest, RoutingPolicy, ScheduledContact,
};
use hm_store::{AdmissionLimits, Direction, QueuedMessage, ReceivedMessage, RelayMetadata, Retry, Store};
use hm_wire::{
    unwrap_routed, wrap_routed, Callsign, ContactAdvert, ContactBearer, Dest, FrameHeader, FrameType,
    ObjectId, FEATURE_MAILBOX, FEATURE_RELAY, FLAG_INTERNET, FLAG_MAILBOX, FLAG_RELAY,
};
use hm_xfer::beacon::{beacon_frame, read_beacon};
use hm_xfer::{Command, Event, Failure, Receipt};

use crate::config::{RadioSettings, RelaySettings};
use crate::driver::Link;
use crate::files::{KeyFile, Trust};
use crate::kiss_link::{KissLink, KissTarget, TncParams};
use crate::sound_link::{AudioFactory, Csma, Framing, PttFactory, SoundLink};
use crate::station::{open_message, unix_now, LinkTiming, Station, Verification};
use choose::{Bearer, Chooser};
use control::{sign_contact, ControlAction, ControlBudget, ControlPlane, CONTROL_BUDGET_WINDOW_MS};
use live::{Live, LiveConfig};

fn route_bearer(bearer: Bearer) -> RouteBearer {
    match bearer {
        Bearer::Radio => RouteBearer::Radio,
        Bearer::Internet => RouteBearer::Internet,
    }
}

/// How the node reaches its radio.
pub enum RadioLink {
    /// An external KISS TNC: Direwolf over TCP, or a hardware TNC on a serial
    /// port (which is given `params` for its channel access).
    Kiss {
        target: KissTarget,
        tnc_port: u8,
        params: TncParams,
    },
    /// The built-in modem on a sound card, with its own PTT and channel access.
    Modem {
        audio: AudioFactory,
        ptt: PttFactory,
        csma: Csma,
        describe: String,
    },
}

pub struct RadioConfig {
    pub link: RadioLink,
    pub timing: LinkTiming,
}

impl RadioConfig {
    pub fn describe(&self) -> String {
        match &self.link {
            RadioLink::Kiss { target, tnc_port, .. } => format!("{} port {tnc_port}", target.describe()),
            RadioLink::Modem { describe, .. } => format!("built-in modem, {describe}"),
        }
    }
}

/// Builds the radio link from `[radio]` settings: `None` when the radio is
/// off. With one, the node follows changes to `[radio]` without a restart.
pub type RadioBuilder = Arc<dyn Fn(&RadioSettings) -> Result<Option<RadioConfig>, String> + Send + Sync>;

/// The radio link `[radio]` describes: a KISS TNC, or the built-in modem on a
/// sound card; `None` when the radio is off.
pub fn radio_config(r: &RadioSettings) -> Result<Option<RadioConfig>, String> {
    r.check()?;
    let link = match (r.enabled, &r.audio) {
        (false, _) => return Ok(None),
        (true, None) => RadioLink::Kiss {
            target: KissTarget::parse(&r.kiss)?,
            tnc_port: r.tnc_port,
            params: r.tnc_params(),
        },
        (true, Some(device)) => {
            let (device, spec) = (device.clone(), r.ptt.clone());
            let describe = format!("sound card {device}, PTT {spec}, {}", r.framing);
            RadioLink::Modem {
                audio: Arc::new(move || {
                    Ok(Box::new(hm_rig::soundcard::SoundCard::open(&device)?) as Box<dyn hm_rig::AudioPort>)
                }),
                ptt: Arc::new(move || hm_rig::ptt::open(&spec)),
                csma: Csma {
                    framing: Framing::parse(&r.framing)?,
                    persist: r.persist,
                    slot: Duration::from_millis(r.slottime_ms),
                    txdelay_ms: r.txdelay_ms as u32,
                    ..Default::default()
                },
                describe,
            }
        }
    };
    Ok(Some(RadioConfig {
        link,
        timing: r.timing(),
    }))
}

/// The node's internet endpoint. The stations it keeps links to are live
/// settings ([`Live::peers`]).
pub struct InternetConfig {
    pub listen: SocketAddr,
}

pub struct NodeConfig {
    pub key: KeyFile,
    /// Settings applied while running: trust, costs, retries, beacons, peers.
    pub live: Live,
    /// `station.toml`: watched for edits, and where changes made through the
    /// API are saved. Without one, API changes last until restart.
    pub config_file: Option<PathBuf>,
    /// Command-line settings for this run, kept over edits to the file.
    pub overrides: Option<live::Overrides>,
    /// Names of the settings `overrides` sets, for the settings page.
    pub overridden: Vec<String>,
    /// Callsign on air (the key's base call, possibly with an SSID).
    pub me: Callsign,
    /// The radio link to start with.
    pub radio: Option<RadioConfig>,
    /// Rebuilds the radio link when `[radio]` changes; without one, radio
    /// changes take a restart.
    pub radio_builder: Option<RadioBuilder>,
    pub internet: Option<InternetConfig>,
    pub relay: RelaySettings,
    /// Planned directed contacts loaded from `[[contact]]`.
    pub schedules: Vec<ScheduledContact>,
    pub store: PathBuf,
    pub http: SocketAddr,
    /// Bearer token the API requires.
    pub token: String,
    /// Fixed seed for bearer choice (tests); random when `None`.
    pub seed: Option<u64>,
}

/// What the node is doing, for the status API.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub radio: Option<bool>,
    /// How the radio is reached, while the node has one.
    pub radio_via: Option<String>,
    pub internet_listen: Option<SocketAddr>,
    pub internet_peers: Vec<Callsign>,
    /// (station, bearer, estimated success rate)
    pub estimates: Vec<(Callsign, &'static str, f64)>,
    /// Stations heard on the radio, most recent first.
    pub heard: Vec<heard::Station>,
}

/// A running node.
pub struct NodeHandle {
    pub http_addr: SocketAddr,
    pub internet_addr: Option<SocketAddr>,
    stop: Arc<AtomicBool>,
    main: JoinHandle<()>,
    radio: Option<JoinHandle<()>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl NodeHandle {
    pub fn stop(self) -> io::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.shutdown.send(true);
        self.main
            .join()
            .map_err(|_| io::Error::other("node thread panicked"))?;
        if let Some(r) = self.radio {
            r.join().map_err(|_| io::Error::other("radio thread panicked"))?;
        }
        Ok(())
    }

    /// Run until the process is interrupted.
    pub fn wait(self) -> io::Result<()> {
        self.main
            .join()
            .map_err(|_| io::Error::other("node thread panicked"))
    }
}

pub fn log(msg: impl AsRef<str>) {
    eprintln!("{} {}", crate::station::utc_clock(unix_now()), msg.as_ref());
}

fn short(id: &ObjectId) -> String {
    id.to_string()[..12].to_string()
}

enum RadioCmd {
    Send {
        object: Vec<u8>,
        to: Callsign,
        precedence: u8,
    },
    Accept {
        from: Callsign,
        xfer_id: ObjectId,
        accepted: bool,
        retry_after: u16,
    },
    Sync {
        to: Dest,
        payload: Vec<u8>,
    },
}

enum RadioEvt {
    /// The radio link now in use (`None`: the radio is off).
    Using(Option<String>),
    Up,
    Down(String),
    Received {
        from: Callsign,
        xfer_id: ObjectId,
        object: Vec<u8>,
    },
    Delivered {
        xfer_id: ObjectId,
        to: Callsign,
        receipt: Receipt,
    },
    Failed {
        xfer_id: ObjectId,
        to: Callsign,
        reason: Failure,
    },
    Sync {
        from: Callsign,
        payload: Vec<u8>,
    },
    Heard(Vec<heard::Station>),
}

/// Tells open web pages what changed, so they fetch it again at once:
/// `"message"` (one arrived, was queued, delivered or read), `"status"`
/// (radio, links, stations heard) or `"settings"` (settings or trust).
#[derive(Clone)]
pub struct Notify(tokio::sync::broadcast::Sender<&'static str>);

impl Notify {
    pub fn new() -> Notify {
        Notify(tokio::sync::broadcast::channel(64).0)
    }

    pub fn send(&self, what: &'static str) {
        // Nobody listening is fine.
        let _ = self.0.send(what);
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<&'static str> {
        self.0.subscribe()
    }
}

impl Default for Notify {
    fn default() -> Notify {
        Notify::new()
    }
}

/// Whether mail to station `to` is ours. Every SSID is a station of its own,
/// so SA0KAM-1 does not take mail for SA0KAM-2. A node whose key file names
/// the bare callsign (and picked its SSID with `--ssid`) also takes mail for
/// the bare callsign.
pub fn addressed_to_us(to: Callsign, me: Callsign, key_call: Callsign) -> bool {
    to == me || (to == key_call && key_call == key_call.base())
}

enum Acceptance {
    Stored,
    Duplicate,
    Busy(String),
    Rejected(String),
}

impl Acceptance {
    fn verdict(self) -> Verdict {
        match self {
            Self::Stored => Verdict::Stored,
            Self::Duplicate => Verdict::Duplicate,
            Self::Busy(reason) => Verdict::Busy {
                retry_after: 60,
                reason,
            },
            Self::Rejected(reason) => Verdict::Rejected(reason),
        }
    }

    fn custody_accepted(&self) -> bool {
        matches!(self, Self::Stored | Self::Duplicate)
    }
}

struct AcceptanceGate<'a> {
    store: &'a Store,
    notify: &'a Notify,
    trust: &'a Trust,
    me: Callsign,
    key_call: Callsign,
    identity: &'a Identity,
    relay: &'a RelaySettings,
}

/// The one gate for final delivery and relay custody.
fn accept(gate: AcceptanceGate<'_>, via: Callsign, object: &[u8]) -> Acceptance {
    let AcceptanceGate {
        store,
        notify,
        trust,
        me,
        key_call,
        identity,
        relay,
    } = gate;
    let routed = match unwrap_routed(object) {
        Ok(route) => route,
        Err(error) => return Acceptance::Rejected(format!("invalid routing wrapper: {error}")),
    };
    let (inner, hop_count, visited) = match &routed {
        Some(route) => (route.bundle, route.hop_count, route.visited.as_slice()),
        None => (object, 0, &[][..]),
    };
    let m = open_message(via, inner, trust);
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
    let now = unix_now();
    if bundle.is_expired(now) {
        return Acceptance::Rejected("bundle expired".into());
    }
    if bundle.kind == Kind::Receipt {
        if let Err(error) = bundle.validate_receipt() {
            return Acceptance::Rejected(error.to_string());
        }
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
        let max_hops = bundle.max_hops().min(relay.max_hops);
        if hop_count >= max_hops || visited.contains(&me) {
            return Acceptance::Rejected("hop limit or routing loop".into());
        }
        match store.record(m.id) {
            Ok(Some(_)) => return Acceptance::Duplicate,
            Ok(None) => {}
            Err(error) => return Acceptance::Rejected(format!("store: {error}")),
        }
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
        };
        return match store.enqueue_relay(m.id, inner, metadata, now) {
            Ok(true) => {
                log(format!(
                    "accepted custody of {} from {via} for {destination}",
                    short(&m.id)
                ));
                notify.send("message");
                Acceptance::Stored
            }
            Ok(false) => Acceptance::Duplicate,
            Err(error) => Acceptance::Rejected(format!("store: {error}")),
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
    let receipt_ttl = bundle
        .expires_at()
        .saturating_sub(now)
        .max(24 * 3600)
        .min(u64::from(u32::MAX)) as u32;
    let receipt = match Bundle::receipt(
        our_recipient.expect("checked above"),
        bundle.from,
        m.id,
        now,
        receipt_ttl,
    )
    .with_precedence(bundle.precedence())
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
        verified,
    };
    let reply = QueuedMessage {
        id: receipt.id(),
        object: &receipt_bytes,
        to: bundle.from,
        precedence: bundle.precedence().rank(),
        expires_at: now.saturating_add(u64::from(receipt_ttl)),
        max_hops: bundle.max_hops(),
    };
    match store.receive_with_reply(received, reply, now) {
        Ok(true) => {
            log(format!(
                "received {} from {} via {via} ({})",
                short(&m.id),
                bundle.from,
                if verified { "verified" } else { "unverified" }
            ));
            notify.send("message");
            Acceptance::Stored
        }
        Ok(false) => Acceptance::Duplicate,
        Err(e) => Acceptance::Rejected(format!("store: {e}")),
    }
}

/// Open the store, bind HTTP, start the bearers and the coordinator.
pub fn start(cfg: NodeConfig) -> io::Result<NodeHandle> {
    let store = Arc::new(Store::open(&cfg.store).map_err(|e| io::Error::other(e.to_string()))?);
    let listener = TcpListener::bind(cfg.http)?;
    listener.set_nonblocking(true)?;
    let http_addr = listener.local_addr()?;
    if !http_addr.ip().is_loopback() {
        log(format!(
            "API on {http_addr} is reachable from the network; it requires the access token"
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
                let net = match &cfg.internet {
                    None => None,
                    Some(ic) => {
                        let (store, gate_live, me, key_call, gate_identity, gate_notify, gate_relay) = (
                            store.clone(),
                            live.clone(),
                            cfg.me,
                            cfg.key.call,
                            Identity::from_secret(cfg.key.identity.secret()),
                            notify.clone(),
                            cfg.relay.clone(),
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
                                    relay: &gate_relay,
                                },
                                via,
                                &obj,
                            )
                            .verdict()
                        });
                        let control: hm_net::Control = Arc::new(move |from, payload| {
                            let _ = net_control_tx.send((from, payload));
                        });
                        let nc = NetConfig {
                            me: cfg.me,
                            secret: cfg.key.identity.secret(),
                            trust: live.get().trust.iter().collect(),
                            listen: ic.listen,
                            dial: live.get().peers,
                        };
                        match Net::start_with_control(nc, gate, control) {
                            Ok(n) => Some(n),
                            Err(e) => {
                                let _ = addr_tx.send(Err(e));
                                return;
                            }
                        }
                    }
                };
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
                    net.clone(),
                    radio_cmd_tx,
                    radio_evt_rx,
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
        "node {} up: radio {}, internet {}, web http://{http_addr}/",
        cfg.me,
        cfg.radio.as_ref().map_or("off".to_string(), |r| r.describe()),
        internet_addr.map_or("off".to_string(), |a| format!("on {a}")),
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

struct InFlight {
    bearer: Bearer,
    peer: Callsign,
    route: Route,
    object_bytes: u64,
}

type NetResult = (ObjectId, Callsign, Result<(), NetError>);

fn send_sync(
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
    }
}

fn broadcast_sync(
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

fn receive_sync(
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

fn apply_sync_actions(
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

fn advertised_flags(cfg: &NodeConfig) -> u8 {
    let mut flags = if cfg.internet.is_some() { FLAG_INTERNET } else { 0 };
    if cfg.relay.enabled {
        flags |= FLAG_RELAY;
    }
    if cfg.relay.mailbox {
        flags |= FLAG_MAILBOX;
    }
    flags
}

fn scheduled_advert(
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

fn live_advert(
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

#[allow(clippy::too_many_arguments)]
async fn coordinator(
    cfg: &NodeConfig,
    store: &Store,
    net: Option<Arc<Net>>,
    radio_cmd: mpsc::Sender<RadioCmd>,
    mut radio_evt: tokio::sync::mpsc::UnboundedReceiver<RadioEvt>,
    mut net_control: tokio::sync::mpsc::UnboundedReceiver<(Callsign, Vec<u8>)>,
    status: &Mutex<Status>,
    live: &LiveConfig,
    notify: &Notify,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut live_version = live.version();
    let seed = cfg
        .seed
        .unwrap_or_else(|| getrandom::u64().unwrap_or_else(|_| unix_now()));
    let mut chooser = Chooser::new(live.get().costs, 3600, DetRng::from_seed(seed));
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
    let mut advertised_live: BTreeMap<(Callsign, RouteBearer), u64> = BTreeMap::new();
    let (net_tx, mut net_rx) = tokio::sync::mpsc::unbounded_channel::<NetResult>();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    let finish = |id: ObjectId,
                  flight: InFlight,
                  outcome: Result<bool, (String, bool)>,
                  chooser: &mut Chooser,
                  graph: &mut ContactGraph,
                  failed_contacts: &mut BTreeMap<ObjectId, BTreeMap<ContactKey, u64>>| {
        let now = unix_now();
        let peer = flight.peer;
        let bearer = flight.bearer;
        match outcome {
            Ok(verified) => {
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
                if let Err(e) = store.custody_transferred(id, peer, verified, bearer.name(), now) {
                    log(format!("store: {e}"));
                }
                log(format!(
                    "custody of {} transferred to {peer} by {}, receipt {}",
                    short(&id),
                    bearer.name(),
                    if verified { "verified" } else { "unverified" }
                ));
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
                let r = if permanent {
                    store.abandon(id, &reason).map(|_| Retry::GaveUp)
                } else {
                    store.attempt_failed(
                        id,
                        &format!("{reason} ({})", bearer.name()),
                        live.get().retry,
                        now,
                    )
                };
                match r {
                    Ok(Retry::At(t)) => log(format!(
                        "{} to {peer} by {} failed: {reason}; next try in {} s",
                        short(&id),
                        bearer.name(),
                        t.saturating_sub(now)
                    )),
                    Ok(Retry::GaveUp) => log(format!("gave up on {} to {peer}: {reason}", short(&id))),
                    Ok(Retry::Inactive) => {}
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
                        log(format!("radio now {}", via.as_deref().unwrap_or("off")));
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
                            relay: &cfg.relay,
                        },
                        from,
                        &object,
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
                        from,
                        &payload,
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
                                cfg.relay.enabled || cfg.relay.mailbox,
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
                        if let Some(flight) = in_flight.remove(&(id, to)) {
                            control.clear_request(id, to);
                            finish(
                                id,
                                flight,
                                Ok(receipt == Receipt::Verified),
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
                    from,
                    &payload,
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
                let outcome = match result {
                    Ok(()) => Ok(true),
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
            _ = tick.tick() => {
                match live.reload_if_changed() {
                    Some(Ok(())) => log("settings file changed; applied"),
                    Some(Err(e)) => log(format!("settings file not applied, keeping the settings in use: {e}")),
                    None => {}
                }
                if live.version() != live_version {
                    live_version = live.version();
                    notify.send("settings");
                    let live = live.get();
                    chooser.set_costs(live.costs);
                    if let Some(n) = &net {
                        n.set_trust(&live.trust.iter().collect::<Vec<_>>());
                        n.set_dial(live.peers.clone());
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
                            cfg.relay.enabled || cfg.relay.mailbox,
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
                            cfg.relay.enabled || cfg.relay.mailbox,
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
                        let _ = store.abandon(r.id, "stored object is not a bundle");
                        continue;
                    };
                    let bundle = opened.bundle;
                    if bundle.is_expired(now) {
                        let _ = store.abandon(r.id, "bundle expired");
                        continue;
                    }
                    let destination = r.final_destination();
                    let requested_peer = control.target_for(r.id, now);
                    let route_destination = requested_peer.unwrap_or(destination);
                    let visited = r.visited.as_deref().unwrap_or(&[]);
                    let mut max_hops = r.max_hops.unwrap_or_else(|| bundle.max_hops());
                    max_hops = max_hops.min(bundle.max_hops()).min(cfg.relay.max_hops);
                    if r.direction == Direction::Relay && !cfg.relay.enabled {
                        max_hops = max_hops.min((visited.len() + 1) as u8);
                    }
                    let excluded: Vec<ContactKey> = failed_contacts
                        .entry(r.id)
                        .or_default()
                        .iter()
                        .filter_map(|(contact, until)| (*until > now).then_some(*contact))
                        .collect();
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
                        airtime_budget_millis: cfg.relay.airtime_budget_secs.saturating_mul(1_000),
                        visited,
                        excluded_contacts: &excluded,
                        urgent: r.precedence >= 2,
                    };
                    let policy = RoutingPolicy {
                        urgent_min_gain: cfg.relay.urgent_min_gain,
                        ..RoutingPolicy::default()
                    };
                    let mut plan = plan_routes(&graph, &make_request(), policy);
                    if plan.is_err() && radio_up {
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
                            RouteBearer::Radio => (Bearer::Radio, radio_up),
                            RouteBearer::Internet => (
                                Bearer::Internet,
                                net.as_ref().is_some_and(|network| network.is_connected(first.contact.to)),
                            ),
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
                        }
                    }
                }
                let mut st = status.lock().expect("lock");
                let radio = radio_via.as_ref().map(|_| radio_up);
                if (st.radio, &st.radio_via, &st.internet_peers) != (radio, &radio_via, &links) {
                    notify.send("status");
                }
                st.radio = radio;
                st.radio_via = radio_via.clone();
                st.internet_peers = links;
                st.estimates = chooser
                    .peers()
                    .into_iter()
                    .flat_map(|p| {
                        [Bearer::Radio, Bearer::Internet].map(|b| (p, b.name(), chooser.estimate(p, b, now)))
                    })
                    .collect();
            }
        }
    }
}

const RECONNECT: Duration = Duration::from_secs(5);
/// The first beacon goes out at a random moment in this window after the radio
/// comes up (or between half and one beacon interval, if that is shorter), so
/// stations started together do not all beacon at once.
const FIRST_BEACON_MS: (u64, u64) = (5_000, 30_000);
/// Heard-table updates reach the status API at least this often.
const HEARD_REPORT: Duration = Duration::from_secs(30);
const MAX_WAIT: Duration = Duration::from_millis(200);

/// How a radio session ended.
enum Ended {
    Stopped,
    /// `[radio]` changed: open the link it describes now.
    Reconfigure,
}

/// Keep the radio link up and run the transfer engine on it; open a new link
/// when `[radio]` changes, if the node has a [`RadioBuilder`].
fn radio_thread(
    cfg: &NodeConfig,
    live: &LiveConfig,
    cmds: mpsc::Receiver<RadioCmd>,
    events: tokio::sync::mpsc::UnboundedSender<RadioEvt>,
    stop: &AtomicBool,
) {
    // The link in use: the one the node started with until [radio] changes.
    let mut rebuilt: Option<Option<RadioConfig>> = None;
    let mut settings = live.get().radio.link_settings();
    let changed = |settings: &RadioSettings| {
        cfg.radio_builder.is_some() && live.get().radio.link_settings() != *settings
    };
    while !stop.load(Ordering::Relaxed) {
        let rc = match &rebuilt {
            Some(r) => r.as_ref(),
            None => cfg.radio.as_ref(),
        };
        let _ = events.send(RadioEvt::Using(rc.map(|r| r.describe())));
        let session = |rc: &RadioConfig, link: &mut dyn Link| {
            let _ = events.send(RadioEvt::Up);
            radio_session(cfg, rc, live, &settings, link, &cmds, &events, stop)
        };
        let result = match rc {
            None => Err("the radio is off".to_string()),
            Some(rc) => match &rc.link {
                RadioLink::Kiss {
                    target,
                    tnc_port,
                    params,
                } => KissLink::open(target, cfg.me, *tnc_port, *params)
                    .map(|mut l| session(rc, &mut l))
                    .map_err(|e| format!("cannot open {}: {e}", rc.describe())),
                RadioLink::Modem { audio, ptt, csma, .. } => {
                    SoundLink::start(cfg.me, audio.clone(), ptt.clone(), *csma)
                        .map(|mut l| session(rc, &mut l))
                        .map_err(|e| format!("cannot open {}: {e}", rc.describe()))
                }
            },
        };
        let off = rc.is_none();
        let wait = match result {
            Ok(Ok(Ended::Stopped)) => return,
            Ok(Ok(Ended::Reconfigure)) => {
                let _ = events.send(RadioEvt::Down("the radio settings changed".into()));
                false
            }
            Ok(Err(e)) => {
                let _ = events.send(RadioEvt::Down(e.to_string()));
                true
            }
            Err(e) => {
                let _ = events.send(RadioEvt::Down(e));
                true
            }
        };
        // Wait to reconnect (while off: until switched on), unless [radio] changes.
        let until = Instant::now() + RECONNECT;
        while wait && (off || Instant::now() < until) && !changed(&settings) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            // Commands sent while the radio is down are answered by RadioEvt::Down.
            while cmds.try_recv().is_ok() {}
            thread::sleep(Duration::from_millis(100));
        }
        if changed(&settings) {
            let now = live.get().radio;
            settings = now.link_settings();
            match cfg.radio_builder.as_ref().expect("checked by changed")(&now) {
                Ok(r) => rebuilt = Some(r),
                Err(e) => log(format!(
                    "radio settings not applied, keeping the link in use: {e}"
                )),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn radio_session(
    cfg: &NodeConfig,
    rc: &RadioConfig,
    live: &LiveConfig,
    settings: &RadioSettings,
    link: &mut dyn Link,
    cmds: &mpsc::Receiver<RadioCmd>,
    events: &tokio::sync::mpsc::UnboundedSender<RadioEvt>,
    stop: &AtomicBool,
) -> io::Result<Ended> {
    let mut live_version = live.version();
    let station = Station {
        key: &cfg.key,
        trust: &live.get().trust,
        me: cfg.me,
        timing: rc.timing,
    };
    let mut x = station.engine()?;
    let mut features = link.features();
    if cfg.relay.enabled {
        features |= FEATURE_RELAY;
    }
    if cfg.relay.mailbox {
        features |= FEATURE_MAILBOX;
    }
    x.set_features(features);
    x.set_application_ack(true);
    let port = x.config().port;
    let start = Instant::now();
    let now = || Millis(start.elapsed().as_millis() as u64);
    let mut out: Vec<Output<Event>> = Vec::new();
    let mut rng = DetRng::from_seed(getrandom::u64().unwrap_or(0xBEAC));
    let mut flags = if cfg.internet.is_some() { FLAG_INTERNET } else { 0 };
    if cfg.relay.enabled {
        flags |= FLAG_RELAY;
    }
    if cfg.relay.mailbox {
        flags |= FLAG_MAILBOX;
    }
    let mut heard = heard::HeardTable::default();
    let beacon_every = |secs: u64| (secs > 0).then(|| Duration::from_secs(secs));
    let mut beacon_secs = live.get().beacon_secs;
    let mut next_beacon = beacon_every(beacon_secs).map(|every| {
        let every = every.as_millis() as u64;
        let (lo, hi) = (FIRST_BEACON_MS.0.min(every / 2), FIRST_BEACON_MS.1.min(every));
        start + Duration::from_millis(lo + rng.below(hi - lo + 1))
    });
    let mut heard_changed = false;
    let mut last_report = Instant::now();
    let control_permyriad = (cfg.relay.control_airtime_fraction * 10_000.0)
        .round()
        .clamp(0.0, 10_000.0) as u16;
    let mut control_budget =
        ControlBudget::new(CONTROL_BUDGET_WINDOW_MS, control_permyriad).map_err(io::Error::other)?;
    let mut sync_queue = VecDeque::<(Dest, Vec<u8>)>::new();
    loop {
        if live.version() != live_version {
            live_version = live.version();
            let live = live.get();
            if cfg.radio_builder.is_some() && live.radio.link_settings() != *settings {
                return Ok(Ended::Reconfigure);
            }
            x.set_trust(live.trust.iter());
            if live.beacon_secs != beacon_secs {
                // A new interval: the next beacon one (new) interval from now, or none.
                beacon_secs = live.beacon_secs;
                next_beacon = beacon_every(beacon_secs).map(|e| Instant::now() + e);
            }
        }
        if let (Some(at), Some(every)) = (next_beacon, beacon_every(beacon_secs)) {
            if Instant::now() >= at {
                let t = unix_now();
                let locator = live.get().locator;
                let frame = beacon_frame(
                    &cfg.key.identity,
                    cfg.me,
                    flags,
                    t as u32,
                    locator,
                    heard.for_beacon(t),
                )
                .map_err(|e| io::Error::other(format!("beacon: {e}")))?;
                link.send(&frame)?;
                let jitter = every.as_millis() as u64 / 10;
                let ms = every.as_millis() as u64 - jitter + rng.below(2 * jitter + 1);
                next_beacon = Some(Instant::now() + Duration::from_millis(ms));
            }
        }
        if heard_changed || last_report.elapsed() >= HEARD_REPORT {
            heard.expire(unix_now());
            let _ = events.send(RadioEvt::Heard(heard.list()));
            heard_changed = false;
            last_report = Instant::now();
        }
        while let Ok(command) = cmds.try_recv() {
            match command {
                RadioCmd::Send {
                    object,
                    to,
                    precedence,
                } => x.handle(
                    now(),
                    Input::Command(Command::Send {
                        to,
                        object,
                        precedence,
                    }),
                    &mut out,
                ),
                RadioCmd::Accept {
                    from,
                    xfer_id,
                    accepted,
                    retry_after,
                } => x.handle(
                    now(),
                    Input::Command(Command::Accept {
                        from,
                        id: xfer_id,
                        accepted,
                        retry_after,
                    }),
                    &mut out,
                ),
                RadioCmd::Sync { to, payload } if sync_queue.len() < 64 => {
                    sync_queue.push_back((to, payload));
                }
                RadioCmd::Sync { .. } => {}
            }
        }
        if let Some((to, payload)) = sync_queue.front() {
            let frame = FrameHeader {
                ftype: FrameType::Sync,
                src: cfg.me,
                dst: *to,
                session: 0,
                index: 0,
            }
            .frame(payload)
            .map_err(|error| io::Error::other(error.to_string()))?;
            let airtime_ms = rc.timing.txdelay_ms.saturating_add(
                ((frame.len() as u64 + 24) * 8 * 1_000).div_ceil(u64::from(rc.timing.bitrate_bps)),
            );
            if control_budget.admit(now().0, airtime_ms) {
                link.send(&frame)?;
                sync_queue.pop_front();
            }
        }
        for o in out.drain(..) {
            match o {
                Output::Transmit { port: p, data } if p == port => {
                    // Tell the link what the destination decodes, so it can frame to suit.
                    if let Ok((
                        FrameHeader {
                            dst: Dest::Station(to),
                            ..
                        },
                        _,
                    )) = FrameHeader::decode(&data)
                    {
                        if let Some(open) = x.peer(to) {
                            link.peer_features(to, open.features);
                        }
                    }
                    link.send(&data)?
                }
                Output::Transmit { .. } => {}
                Output::Event(Event::Received { from, id, object }) => {
                    let _ = events.send(RadioEvt::Received {
                        from,
                        xfer_id: id,
                        object,
                    });
                }
                Output::Event(Event::Delivered { to, id, receipt, .. }) => {
                    let _ = events.send(RadioEvt::Delivered {
                        xfer_id: id,
                        to,
                        receipt,
                    });
                }
                Output::Event(Event::Failed { to, id, reason }) => {
                    let _ = events.send(RadioEvt::Failed {
                        xfer_id: id,
                        to,
                        reason,
                    });
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(Ended::Stopped);
        }
        let t = now();
        match x.next_deadline() {
            Some(d) if d <= t => x.on_deadline(t, &mut out),
            next => {
                let wait = next
                    .map_or(MAX_WAIT, |d| Duration::from_millis(d.0 - t.0))
                    .min(MAX_WAIT);
                let wait = match next_beacon {
                    Some(at) => wait.min(at.saturating_duration_since(Instant::now())),
                    None => wait,
                };
                if let Some(frame) = link.recv_timeout(wait)? {
                    heard_changed |= hear(&mut heard, &live.get().trust, cfg.me, &frame);
                    if let Ok((header, payload)) = FrameHeader::decode(&frame) {
                        if header.ftype == FrameType::Sync
                            && (matches!(header.dst, Dest::Broadcast) || header.dst == Dest::Station(cfg.me))
                            && header.src != cfg.me
                        {
                            let _ = events.send(RadioEvt::Sync {
                                from: header.src,
                                payload: payload.to_vec(),
                            });
                        }
                    }
                    x.handle(now(), Input::Frame { port, data: frame }, &mut out);
                }
            }
        }
    }
}

/// Note the station a frame came from, and check its beacon if it is one;
/// true when the table changed in a way worth reporting.
fn hear(table: &mut heard::HeardTable, trust: &Trust, me: Callsign, frame: &[u8]) -> bool {
    let Ok((h, _)) = FrameHeader::decode(frame) else {
        return false;
    };
    if h.src == me {
        return false;
    }
    let t = unix_now();
    let new = table.frame(t, h.src);
    match read_beacon(frame) {
        Some(b) => {
            if table.beacon(t, &b, trust) == heard::KeyCheck::Mismatch {
                log(format!(
                    "beacon from {} carries a different key than station.toml trusts for {}",
                    b.from, b.from
                ));
            }
            true
        }
        None => new,
    }
}

#[cfg(test)]
mod tests {
    use super::addressed_to_us;
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
}
