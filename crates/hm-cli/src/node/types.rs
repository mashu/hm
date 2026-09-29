//! Public node configuration types, status, and shared helpers.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use hm_net::Verdict;
use hm_node::Acceptance;
use hm_route::ScheduledContact;
use hm_wire::Callsign;

use super::live::{Live, Overrides};
use crate::config::RadioSettings;
use crate::files::KeyFile;
use crate::kiss_link::{KissTarget, TncParams};
use crate::sound_link::{AudioFactory, Csma, Framing, PttFactory};
use crate::station::LinkTiming;

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
    /// Accept any inbound station certificate (core hub).
    pub open_hub: bool,
}

pub struct NodeConfig {
    pub key: KeyFile,
    /// Settings applied while running: trust, costs, retries, beacons, peers.
    pub live: Live,
    /// `station.toml`: watched for edits, and where changes made through the
    /// API are saved. Without one, API changes last until restart.
    pub config_file: Option<PathBuf>,
    /// Command-line settings for this run, kept over edits to the file.
    pub overrides: Option<Overrides>,
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
    /// An ARQ modem program (VARA, Mercury, ARDOP) as a third bearer.
    pub modem: Option<super::arq::ArqConfig>,
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
    /// `None` without a modem; otherwise whether the modem program is reachable.
    pub modem: Option<bool>,
    /// The station the modem is connected to.
    pub modem_peer: Option<Callsign>,
    pub radio: Option<bool>,
    /// How the radio is reached, while the node has one.
    pub radio_via: Option<String>,
    pub internet_listen: Option<SocketAddr>,
    pub internet_peers: Vec<Callsign>,
    /// (station, bearer, estimated success rate)
    pub estimates: Vec<(Callsign, &'static str, f64)>,
    /// Stations heard on the radio, most recent first.
    pub heard: Vec<super::heard::Station>,
}

/// A running node.
pub struct NodeHandle {
    pub http_addr: SocketAddr,
    pub internet_addr: Option<SocketAddr>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) main: JoinHandle<()>,
    pub(crate) radio: Option<JoinHandle<()>>,
    pub(crate) shutdown: tokio::sync::watch::Sender<bool>,
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

pub use hm_node::{addressed_to_us, log};

/// What an internet or modem peer is told about an object it sent.
pub(crate) fn verdict(acceptance: Acceptance) -> Verdict {
    match acceptance {
        Acceptance::Stored => Verdict::Stored,
        Acceptance::Duplicate => Verdict::Duplicate,
        Acceptance::Busy(reason) => Verdict::Busy {
            retry_after: 60,
            reason,
        },
        Acceptance::Rejected(reason) => Verdict::Rejected(reason),
    }
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

    /// The same notifications, for [`hm_node`], which knows no channels.
    pub fn observer(&self) -> hm_node::Notify {
        let sender = self.0.clone();
        hm_node::Notify::new(move |what| {
            let _ = sender.send(what);
        })
    }
}

impl Default for Notify {
    fn default() -> Notify {
        Notify::new()
    }
}
