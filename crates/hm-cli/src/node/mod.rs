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
pub mod heard;
pub mod live;

use std::collections::BTreeMap;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hm_bundle::Address;
use hm_core::{DetRng, Input, Machine, Millis, Output};
use hm_net::{Net, NetConfig, NetError, Verdict};
use hm_store::{Retry, Store};
use hm_wire::{Callsign, FrameHeader, ObjectId, FLAG_INTERNET};
use hm_xfer::beacon::{beacon_frame, read_beacon};
use hm_xfer::{Command, Event, Failure, Receipt};

use crate::config::RadioSettings;
use crate::driver::Link;
use crate::files::{KeyFile, Trust};
use crate::kiss_link::{KissLink, KissTarget, TncParams};
use crate::sound_link::{AudioFactory, Csma, PttFactory, SoundLink};
use crate::station::{open_message, unix_now, LinkTiming, Station, Verification};
use choose::{Bearer, Chooser};
use live::{Live, LiveConfig};

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
            let describe = format!("sound card {device}, PTT {spec}");
            RadioLink::Modem {
                audio: Arc::new(move || {
                    Ok(Box::new(hm_rig::soundcard::SoundCard::open(&device)?) as Box<dyn hm_rig::AudioPort>)
                }),
                ptt: Arc::new(move || hm_rig::ptt::open(&spec)),
                csma: Csma {
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
}

enum RadioEvt {
    /// The radio link now in use (`None`: the radio is off).
    Using(Option<String>),
    Up,
    Down(String),
    Received {
        from: Callsign,
        object: Vec<u8>,
    },
    Delivered {
        xfer_id: ObjectId,
        receipt: Receipt,
    },
    Failed {
        xfer_id: ObjectId,
        reason: Failure,
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

/// The one gate for everything that arrives: a bundle, addressed to us, not
/// failing its signature check, stored once.
fn accept(
    store: &Store,
    notify: &Notify,
    trust: &Trust,
    me: Callsign,
    key_call: Callsign,
    via: Callsign,
    object: &[u8],
) -> Verdict {
    let m = open_message(via, object, trust);
    let Some(bundle) = &m.bundle else {
        return Verdict::Rejected(format!("not a bundle: {}", m.error.unwrap_or_default()));
    };
    let ours = bundle
        .to
        .iter()
        .any(|a| matches!(a, Address::Station(c) if addressed_to_us(*c, me, key_call)));
    if !ours {
        return Verdict::Rejected(format!("not addressed to {me}"));
    }
    if m.verification == Verification::BadSignature {
        log(format!(
            "REJECTED a message claiming to be from {}: bad signature",
            bundle.from
        ));
        return Verdict::Rejected("signature does not verify".into());
    }
    let verified = m.verification == Verification::Verified;
    match store.put_received(m.id, object, via, verified, unix_now()) {
        Ok(true) => {
            log(format!(
                "received {} from {} via {via} ({})",
                short(&m.id),
                bundle.from,
                if verified { "verified" } else { "unverified" }
            ));
            notify.send("message");
            Verdict::Stored
        }
        Ok(false) => Verdict::Duplicate,
        Err(e) => Verdict::Rejected(format!("store: {e}")),
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
                let net = match &cfg.internet {
                    None => None,
                    Some(ic) => {
                        let (store, gate_live, me, key_call, gate_notify) =
                            (store.clone(), live.clone(), cfg.me, cfg.key.call, notify.clone());
                        let gate: hm_net::Accept = Arc::new(move |via, obj| {
                            accept(
                                &store,
                                &gate_notify,
                                &gate_live.get().trust,
                                me,
                                key_call,
                                via,
                                &obj,
                            )
                        });
                        let nc = NetConfig {
                            me: cfg.me,
                            secret: cfg.key.identity.secret(),
                            trust: live.get().trust.iter().collect(),
                            listen: ic.listen,
                            dial: live.get().peers,
                        };
                        match Net::start(nc, gate) {
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
}

type NetResult = (ObjectId, Callsign, Result<(), NetError>);

#[allow(clippy::too_many_arguments)]
async fn coordinator(
    cfg: &NodeConfig,
    store: &Store,
    net: Option<Arc<Net>>,
    radio_cmd: mpsc::Sender<RadioCmd>,
    mut radio_evt: tokio::sync::mpsc::UnboundedReceiver<RadioEvt>,
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
    let mut radio_up = false;
    let mut last_down: Option<String> = None;
    let mut radio_via = cfg.radio.as_ref().map(|r| r.describe());
    let mut in_flight: BTreeMap<ObjectId, InFlight> = BTreeMap::new();
    let mut radio_ids: BTreeMap<ObjectId, ObjectId> = BTreeMap::new(); // transfer id -> bundle id
    let (net_tx, mut net_rx) = tokio::sync::mpsc::unbounded_channel::<NetResult>();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    let finish = |id: ObjectId,
                  peer: Callsign,
                  bearer: Bearer,
                  outcome: Result<bool, (String, bool)>,
                  chooser: &mut Chooser| {
        let now = unix_now();
        match outcome {
            Ok(verified) => {
                chooser.record(peer, bearer, true, now);
                if let Err(e) = store.delivered(id, verified, bearer.name(), now) {
                    log(format!("store: {e}"));
                }
                log(format!(
                    "delivered {} to {peer} by {}, receipt {}",
                    short(&id),
                    bearer.name(),
                    if verified { "verified" } else { "unverified" }
                ));
            }
            Err((reason, permanent)) => {
                chooser.record(peer, bearer, false, now);
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
                    let lost: Vec<ObjectId> =
                        in_flight.iter().filter(|(_, f)| f.bearer == Bearer::Radio).map(|(id, _)| *id).collect();
                    radio_ids.clear();
                    for id in lost {
                        let f = in_flight.remove(&id).expect("listed");
                        finish(id, f.peer, Bearer::Radio, Err(("radio went down".into(), false)), &mut chooser);
                    }
                }
                RadioEvt::Received { from, object } => {
                    accept(store, notify, &live.get().trust, cfg.me, cfg.key.call, from, &object);
                }
                RadioEvt::Heard(list) => {
                    status.lock().expect("lock").heard = list;
                    notify.send("status");
                }
                RadioEvt::Delivered { xfer_id, receipt } => {
                    if let Some(id) = radio_ids.remove(&xfer_id) {
                        if let Some(f) = in_flight.remove(&id) {
                            finish(id, f.peer, Bearer::Radio, Ok(receipt == Receipt::Verified), &mut chooser);
                        }
                    }
                }
                RadioEvt::Failed { xfer_id, reason } => {
                    if let Some(id) = radio_ids.remove(&xfer_id) {
                        if let Some(f) = in_flight.remove(&id) {
                            let permanent = !matches!(reason, Failure::NoAnswer);
                            finish(id, f.peer, Bearer::Radio, Err((format!("{reason:?}"), permanent)), &mut chooser);
                        }
                    }
                }
            },
            Some((id, peer, result)) = net_rx.recv() => {
                in_flight.remove(&id);
                let outcome = match result {
                    Ok(()) => Ok(true),
                    Err(NetError::Rejected(r)) => Err((format!("rejected: {r}"), true)),
                    Err(e) => Err((e.to_string(), false)),
                };
                finish(id, peer, Bearer::Internet, outcome, &mut chooser);
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
                let due = match store.due(now) {
                    Ok(d) => d,
                    Err(e) => {
                        log(format!("store: {e}"));
                        continue;
                    }
                };
                for r in due {
                    if in_flight.contains_key(&r.id) {
                        continue;
                    }
                    let mut available = Vec::with_capacity(2);
                    if radio_up {
                        available.push(Bearer::Radio);
                    }
                    if net.as_ref().is_some_and(|n| n.is_connected(r.peer)) {
                        available.push(Bearer::Internet);
                    }
                    let Some(bearer) = chooser.choose(r.peer, &available, now) else { continue };
                    let Ok(Some(object)) = store.object(r.id) else { continue };
                    log(format!("sending {} to {} by {} (attempt {})", short(&r.id), r.peer, bearer.name(), r.attempts + 1));
                    in_flight.insert(r.id, InFlight { bearer, peer: r.peer });
                    match bearer {
                        Bearer::Radio => {
                            radio_ids.insert(hm_xfer::object_id(&object), r.id);
                            let _ = radio_cmd.send(RadioCmd::Send { object, to: r.peer, precedence: r.precedence });
                        }
                        Bearer::Internet => {
                            let (net, tx, peer, id) = (net.clone().expect("available"), net_tx.clone(), r.peer, r.id);
                            tokio::spawn(async move {
                                let result = net.deliver(peer, &object).await;
                                let _ = tx.send((id, peer, result));
                            });
                        }
                    }
                }
                let mut st = status.lock().expect("lock");
                let links = net.as_ref().map(|n| n.connected()).unwrap_or_default();
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
    let port = x.config().port;
    let start = Instant::now();
    let now = || Millis(start.elapsed().as_millis() as u64);
    let mut out: Vec<Output<Event>> = Vec::new();
    let mut rng = DetRng::from_seed(getrandom::u64().unwrap_or(0xBEAC));
    let flags = if cfg.internet.is_some() { FLAG_INTERNET } else { 0 };
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
        while let Ok(RadioCmd::Send {
            object,
            to,
            precedence,
        }) = cmds.try_recv()
        {
            x.handle(
                now(),
                Input::Command(Command::Send {
                    to,
                    object,
                    precedence,
                }),
                &mut out,
            );
        }
        for o in out.drain(..) {
            match o {
                Output::Transmit { port: p, data } if p == port => link.send(&data)?,
                Output::Transmit { .. } => {}
                Output::Event(Event::Received { from, object, .. }) => {
                    let _ = events.send(RadioEvt::Received { from, object });
                }
                Output::Event(Event::Delivered { id, receipt, .. }) => {
                    let _ = events.send(RadioEvt::Delivered { xfer_id: id, receipt });
                }
                Output::Event(Event::Failed { id, reason, .. }) => {
                    let _ = events.send(RadioEvt::Failed { xfer_id: id, reason });
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
