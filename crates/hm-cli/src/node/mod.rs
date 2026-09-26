//! The station daemon (`hm node`).
//!
//! One process owns the store and the bearers:
//!
//! - **radio** (optional): a thread running the transfer engine over a KISS
//!   TNC, reconnecting if the TNC goes away;
//! - **internet** (optional): QUIC links to other stations (`hm-net`).
//!
//! A coordinator hands every due outbound message to one bearer, chosen per
//! delivery by [`choose::Chooser`] from the bearers that can reach the
//! destination right now. With the default costs radio carries the traffic
//! whenever it delivers, and the internet takes over only while it does not.
//! Everything that arrives, over either bearer, goes through one check
//! (addressed to us, signature per the trust file) into the store, once.
//!
//! The HTTP thread serves the JSON API and web page behind an access token.

mod api;
pub mod choose;

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
use hm_store::{Retry, RetryPolicy, Store};
use hm_wire::{Callsign, ObjectId};
use hm_xfer::{Command, Event, Failure, Receipt};

use crate::driver::Link;
use crate::files::{KeyFile, Trust};
use crate::kiss_tcp::KissLink;
use crate::sound_link::{AudioFactory, Csma, PttFactory, SoundLink};
use crate::station::{open_message, unix_now, LinkTiming, Station, Verification};
use choose::{Bearer, Chooser, Costs};

/// How the node reaches its radio.
pub enum RadioLink {
    /// An external KISS TNC over TCP (Direwolf, or a hardware TNC behind a bridge).
    Kiss { addr: String, tnc_port: u8 },
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
            RadioLink::Kiss { addr, tnc_port } => format!("KISS TNC {addr} port {tnc_port}"),
            RadioLink::Modem { describe, .. } => format!("built-in modem, {describe}"),
        }
    }
}

pub struct InternetConfig {
    pub listen: SocketAddr,
    /// Stations to keep a connection to.
    pub peers: Vec<(Callsign, SocketAddr)>,
}

pub struct NodeConfig {
    pub key: KeyFile,
    pub trust: Trust,
    /// Callsign on air (the key's base call, possibly with an SSID).
    pub me: Callsign,
    pub radio: Option<RadioConfig>,
    pub internet: Option<InternetConfig>,
    pub costs: Costs,
    pub store: PathBuf,
    pub http: SocketAddr,
    pub retry: RetryPolicy,
    /// Bearer token the API requires.
    pub token: String,
    /// Fixed seed for bearer choice (tests); random when `None`.
    pub seed: Option<u64>,
}

/// What the node is doing, for the status API.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub radio: Option<bool>,
    pub internet_listen: Option<SocketAddr>,
    pub internet_peers: Vec<Callsign>,
    /// (station, bearer, estimated success rate)
    pub estimates: Vec<(Callsign, &'static str, f64)>,
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
    Up,
    Down(String),
    Received { from: Callsign, object: Vec<u8> },
    Delivered { xfer_id: ObjectId, receipt: Receipt },
    Failed { xfer_id: ObjectId, reason: Failure },
}

/// The one gate for everything that arrives: a bundle, addressed to us, not
/// failing its signature check, stored once.
fn accept(store: &Store, trust: &Trust, me: Callsign, via: Callsign, object: &[u8]) -> Verdict {
    let m = open_message(via, object, trust);
    let Some(bundle) = &m.bundle else {
        return Verdict::Rejected(format!("not a bundle: {}", m.error.unwrap_or_default()));
    };
    let ours = bundle
        .to
        .iter()
        .any(|a| matches!(a, Address::Station(c) if c.base() == me.base()));
    if !ours {
        return Verdict::Rejected(format!("not addressed to {}", me.base()));
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
    let cfg = Arc::new(cfg);

    let radio = match &cfg.radio {
        Some(_) => {
            let (cfg, stop) = (cfg.clone(), stop.clone());
            Some(thread::Builder::new().name("radio".into()).spawn(move || {
                radio_thread(&cfg, radio_cmd_rx, radio_evt_tx, &stop);
            })?)
        }
        None => None,
    };

    // The internet endpoint is bound here so its address is known on return.
    let (addr_tx, addr_rx) = mpsc::channel::<io::Result<Option<SocketAddr>>>();
    let main = {
        let (cfg, store, status) = (cfg.clone(), store.clone(), status.clone());
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
                        let (store, trust, me) = (store.clone(), cfg.trust.clone(), cfg.me);
                        let gate: hm_net::Accept =
                            Arc::new(move |via, obj| accept(&store, &trust, me, via, &obj));
                        let nc = NetConfig {
                            me: cfg.me,
                            secret: cfg.key.identity.secret(),
                            trust: cfg.trust.iter().collect(),
                            listen: ic.listen,
                            dial: ic.peers.clone(),
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

async fn coordinator(
    cfg: &NodeConfig,
    store: &Store,
    net: Option<Arc<Net>>,
    radio_cmd: mpsc::Sender<RadioCmd>,
    mut radio_evt: tokio::sync::mpsc::UnboundedReceiver<RadioEvt>,
    status: &Mutex<Status>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let seed = cfg
        .seed
        .unwrap_or_else(|| getrandom::u64().unwrap_or_else(|_| unix_now()));
    let mut chooser = Chooser::new(cfg.costs, 3600, DetRng::from_seed(seed));
    let mut radio_up = false;
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
                    store.attempt_failed(id, &format!("{reason} ({})", bearer.name()), cfg.retry, now)
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
    };

    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            Some(ev) = radio_evt.recv() => match ev {
                RadioEvt::Up => {
                    radio_up = true;
                    log("radio up");
                }
                RadioEvt::Down(why) => {
                    radio_up = false;
                    log(format!("radio down: {why}"));
                    let lost: Vec<ObjectId> =
                        in_flight.iter().filter(|(_, f)| f.bearer == Bearer::Radio).map(|(id, _)| *id).collect();
                    radio_ids.clear();
                    for id in lost {
                        let f = in_flight.remove(&id).expect("listed");
                        finish(id, f.peer, Bearer::Radio, Err(("radio went down".into(), false)), &mut chooser);
                    }
                }
                RadioEvt::Received { from, object } => {
                    accept(store, &cfg.trust, cfg.me, from, &object);
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
                st.radio = cfg.radio.as_ref().map(|_| radio_up);
                st.internet_peers = net.as_ref().map(|n| n.connected()).unwrap_or_default();
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
const MAX_WAIT: Duration = Duration::from_millis(200);

/// Keep a KISS link up and run the transfer engine on it.
fn radio_thread(
    cfg: &NodeConfig,
    cmds: mpsc::Receiver<RadioCmd>,
    events: tokio::sync::mpsc::UnboundedSender<RadioEvt>,
    stop: &AtomicBool,
) {
    let rc = cfg.radio.as_ref().expect("radio configured");
    while !stop.load(Ordering::Relaxed) {
        let session = |link: &mut dyn Link| {
            let _ = events.send(RadioEvt::Up);
            radio_session(cfg, rc, link, &cmds, &events, stop)
        };
        let result = match &rc.link {
            RadioLink::Kiss { addr, tnc_port } => {
                KissLink::connect(addr, cfg.me, *tnc_port).map(|mut l| session(&mut l))
            }
            RadioLink::Modem { audio, ptt, csma, .. } => {
                SoundLink::start(cfg.me, audio.clone(), ptt.clone(), *csma).map(|mut l| session(&mut l))
            }
        };
        match result {
            Ok(Ok(())) => return, // stopped
            Ok(Err(e)) => {
                let _ = events.send(RadioEvt::Down(e.to_string()));
            }
            Err(e) => {
                let _ = events.send(RadioEvt::Down(format!("cannot open {}: {e}", rc.describe())));
            }
        }
        let until = Instant::now() + RECONNECT;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            // Commands sent while the radio is down are answered by RadioEvt::Down.
            while cmds.try_recv().is_ok() {}
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn radio_session(
    cfg: &NodeConfig,
    rc: &RadioConfig,
    link: &mut dyn Link,
    cmds: &mpsc::Receiver<RadioCmd>,
    events: &tokio::sync::mpsc::UnboundedSender<RadioEvt>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let station = Station {
        key: &cfg.key,
        trust: &cfg.trust,
        me: cfg.me,
        timing: rc.timing,
    };
    let mut x = station.engine()?;
    let port = x.config().port;
    let start = Instant::now();
    let now = || Millis(start.elapsed().as_millis() as u64);
    let mut out: Vec<Output<Event>> = Vec::new();
    loop {
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
            return Ok(());
        }
        let t = now();
        match x.next_deadline() {
            Some(d) if d <= t => x.on_deadline(t, &mut out),
            next => {
                let wait = next
                    .map_or(MAX_WAIT, |d| Duration::from_millis(d.0 - t.0))
                    .min(MAX_WAIT);
                if let Some(frame) = link.recv_timeout(wait)? {
                    x.handle(now(), Input::Frame { port, data: frame }, &mut out);
                }
            }
        }
    }
}
