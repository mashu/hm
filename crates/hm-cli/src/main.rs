//! `hm`: station keys, trusted stations, sending and receiving messages, and
//! the station daemon.
//!
//! Settings and trusted stations live in `station.toml` (see
//! `hm_cli::config`); every command reads it, and a flag given on the command
//! line overrides the file for that run.
//!
//! ```text
//! hm keygen --call SA0KAM-1          # station.key, and a starter station.toml
//! hm whoami                          # the line to give other stations
//! hm trust add "SO5KM-1 8a1e…"       # trust a station (the line from its hm whoami)
//! hm send --to SO5KM-1 --text "73 de SA0KAM"
//! hm node                            # the station: radio, internet, web page
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand, ValueEnum};
use hm_bundle::Precedence;
use hm_cli::config::{self, Config};
use hm_cli::driver::Flow;
use hm_cli::files::{KeyFile, Trust};
use hm_cli::kiss_link::{KissLink, KissTarget, TncParams};
use hm_cli::station::{self, LinkTiming, SendOutcome, Station, Verification};
use hm_wire::Callsign;
use hm_xfer::Receipt;

#[derive(Parser)]
#[command(name = "hm", version, about = "hm-net: mail and chat over packet radio")]
struct Cli {
    /// Station settings and trusted stations.
    #[arg(long, global = true, default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a station key file, and a starter station.toml if there is none.
    Keygen {
        /// Your callsign. With an SSID (SA0KAM-2) the key is for that station only;
        /// without one it can run as any SSID set with `ssid` in station.toml.
        #[arg(long)]
        call: String,
        /// Where to write the key [default: `station.key` in station.toml].
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print the line other stations use to trust you (`hm trust add`).
    Whoami {
        /// Key file [default: `station.key` in station.toml].
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Trusted stations: whose messages and receipts are verified.
    Trust {
        #[command(subcommand)]
        cmd: TrustCmd,
    },
    /// Send a message and wait until the receiving station confirms it.
    Send {
        #[command(flatten)]
        station: StationArgs,
        /// Destination station, e.g. SO5KM-1.
        #[arg(long)]
        to: String,
        /// Message text.
        #[arg(long)]
        text: String,
        /// Makes it mail instead of chat.
        #[arg(long)]
        subject: Option<String>,
        /// Handling precedence; higher goes first.
        #[arg(long, value_enum, default_value_t = Prec::Routine)]
        precedence: Prec,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Run the station: message store, radio and/or internet links, web page.
    Node {
        #[command(flatten)]
        station: StationArgs,
        #[command(flatten)]
        node: NodeArgs,
    },
    /// List the sound cards the built-in modem can use.
    AudioDevices,
    /// Print messages addressed to this station until interrupted.
    Listen {
        #[command(flatten)]
        station: StationArgs,
    },
}

#[derive(Subcommand)]
enum TrustCmd {
    /// Trust a station: give the line its `hm whoami` prints.
    Add {
        /// `CALL KEY`, e.g. "SO5KM-1 8a1e…".
        line: String,
        /// A note kept beside it, e.g. who it is.
        #[arg(long)]
        note: Option<String>,
    },
    /// Stop trusting a station.
    Remove { station: String },
    /// List trusted stations.
    List,
}

/// Overrides for this run; anything not given comes from station.toml.
#[derive(Args, Clone)]
struct StationArgs {
    /// Station key file [station.key].
    #[arg(long)]
    key: Option<PathBuf>,
    /// SSID to run as, when the key file names a callsign without one [station.ssid].
    #[arg(long)]
    ssid: Option<u8>,
    /// KISS TNC: `HOST:PORT` (Direwolf) or `serial:DEVICE[:BAUD]` [radio.kiss].
    #[arg(long)]
    kiss: Option<String>,
    /// TNC port (Direwolf channel) [radio.tnc_port].
    #[arg(long)]
    tnc_port: Option<u8>,
    /// Built-in modem on this sound card instead of a KISS TNC; `hm node` only [radio.audio].
    #[arg(long)]
    audio: Option<String>,
    /// PTT with the built-in modem [radio.ptt].
    #[arg(long)]
    ptt: Option<String>,
    /// CSMA persistence: transmit chance per clear slot is (p+1)/256 [radio.persist].
    #[arg(long)]
    persist: Option<u8>,
    /// CSMA slot time, ms [radio.slottime_ms].
    #[arg(long)]
    slottime: Option<u64>,
    /// Link bitrate, for predicting when transmissions end [radio.bitrate].
    #[arg(long)]
    bitrate: Option<u32>,
    /// Key-up delay, ms [radio.txdelay_ms].
    #[arg(long)]
    txdelay: Option<u64>,
    /// Slack around predicted ends of transmissions, ms [radio.guard_ms].
    #[arg(long)]
    guard: Option<u64>,
}

/// `hm node` overrides for this run.
#[derive(Args, Clone)]
struct NodeArgs {
    /// Message store [station.store].
    #[arg(long)]
    store: Option<PathBuf>,
    /// Address of the web page and API [station.http].
    #[arg(long)]
    http: Option<String>,
    /// Run without a radio [radio.enabled = false].
    #[arg(long)]
    no_radio: bool,
    /// Accept internet links from trusted stations here [internet.listen].
    #[arg(long)]
    listen: Option<String>,
    /// Keep an internet link to a station, as CALL=HOST:PORT; repeatable,
    /// replaces [[internet.peers]] for this run.
    #[arg(long = "peer", value_name = "CALL=HOST:PORT")]
    peers: Vec<String>,
    /// Relative cost of a delivery attempt by radio [delivery.radio_cost].
    #[arg(long)]
    radio_cost: Option<f64>,
    /// Relative cost of a delivery attempt over the internet [delivery.internet_cost].
    #[arg(long)]
    internet_cost: Option<f64>,
    /// Beacon interval in minutes, 0 for none [radio.beacon_minutes].
    #[arg(long)]
    beacon_minutes: Option<u64>,
    /// Grid locator sent in beacons, like JO89xi [station.locator].
    #[arg(long)]
    locator: Option<String>,
}

/// Put `v` in `slot` if given, noting which setting was overridden.
fn set<T>(slot: &mut T, v: Option<T>, name: &str, overridden: &mut Vec<String>) {
    if let Some(v) = v {
        *slot = v;
        overridden.push(name.to_string());
    }
}

impl StationArgs {
    fn apply(&self, c: &mut Config, o: &mut Vec<String>) {
        set(&mut c.station.key, self.key.clone(), "station.key", o);
        set(&mut c.station.ssid, self.ssid, "station.ssid", o);
        set(&mut c.radio.kiss, self.kiss.clone(), "radio.kiss", o);
        set(&mut c.radio.tnc_port, self.tnc_port, "radio.tnc_port", o);
        set(&mut c.radio.audio, self.audio.clone().map(Some), "radio.audio", o);
        set(&mut c.radio.ptt, self.ptt.clone(), "radio.ptt", o);
        set(&mut c.radio.persist, self.persist, "radio.persist", o);
        set(&mut c.radio.slottime_ms, self.slottime, "radio.slottime_ms", o);
        set(&mut c.radio.bitrate, self.bitrate, "radio.bitrate", o);
        set(&mut c.radio.txdelay_ms, self.txdelay, "radio.txdelay_ms", o);
        set(&mut c.radio.guard_ms, self.guard, "radio.guard_ms", o);
    }
}

impl NodeArgs {
    fn apply(&self, c: &mut Config, o: &mut Vec<String>) -> Result<(), String> {
        set(&mut c.station.store, self.store.clone(), "station.store", o);
        set(&mut c.station.http, self.http.clone(), "station.http", o);
        set(
            &mut c.radio.enabled,
            self.no_radio.then_some(false),
            "radio.enabled",
            o,
        );
        set(
            &mut c.internet.listen,
            self.listen.clone().map(Some),
            "internet.listen",
            o,
        );
        if !self.peers.is_empty() {
            let peers = self
                .peers
                .iter()
                .map(|spec| {
                    let (station, address) = spec
                        .split_once('=')
                        .ok_or_else(|| format!("--peer {spec}: expected CALL=HOST:PORT"))?;
                    Ok(config::PeerEntry {
                        station: station.to_string(),
                        address: address.to_string(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            set(&mut c.internet.peers, Some(peers), "internet.peers", o);
        }
        set(
            &mut c.delivery.radio_cost,
            self.radio_cost,
            "delivery.radio_cost",
            o,
        );
        set(
            &mut c.delivery.internet_cost,
            self.internet_cost,
            "delivery.internet_cost",
            o,
        );
        set(
            &mut c.radio.beacon_minutes,
            self.beacon_minutes,
            "radio.beacon_minutes",
            o,
        );
        set(
            &mut c.station.locator,
            self.locator.clone().map(Some),
            "station.locator",
            o,
        );
        c.locator()?;
        Ok(())
    }
}

/// station.toml with relative paths resolved against its directory.
fn load_config(path: &Path) -> Result<Config, String> {
    let mut c = Config::load(path).map_err(|e| e.to_string())?;
    c.resolve_paths(&config::dir_of(path));
    Ok(c)
}

/// The key and the callsign this station runs as.
fn open_station(c: &Config) -> Result<(KeyFile, Callsign), String> {
    let path = &c.station.key;
    let key = KeyFile::load(path).map_err(|e| {
        format!(
            "{}: {e}{}",
            path.display(),
            if e.kind() == std::io::ErrorKind::NotFound {
                " (make one with `hm keygen --call YOURCALL`)"
            } else {
                ""
            }
        )
    })?;
    let ssid = c.station.ssid;
    let me = if ssid == 0 {
        key.call
    } else if key.call != key.call.base() {
        return Err(format!(
            "the key file is for {} only; leave out the SSID setting, or make a key for another SSID",
            key.call
        ));
    } else {
        if ssid > 15 {
            return Err("SSID must be 0-15".into());
        }
        Callsign::parse(&format!("{}-{ssid}", key.call)).map_err(|e| e.to_string())?
    };
    Ok((key, me))
}

fn timing(c: &Config) -> LinkTiming {
    c.radio.timing()
}

fn tnc_params(c: &Config) -> TncParams {
    c.radio.tnc_params()
}

fn open_kiss(c: &Config, me: Callsign) -> Result<(KissLink, KissTarget), String> {
    let target = KissTarget::parse(&c.radio.kiss)?;
    let link = KissLink::open(&target, me, c.radio.tnc_port, tnc_params(c))
        .map_err(|e| format!("{}: {e}", target.describe()))?;
    Ok((link, target))
}

fn trusted(c: &Config) -> Result<Trust, String> {
    c.trust().map_err(|e| format!("station.toml: {e}"))
}

#[derive(Copy, Clone, ValueEnum)]
enum Prec {
    Routine,
    Priority,
    Immediate,
    Flash,
}

impl From<Prec> for Precedence {
    fn from(p: Prec) -> Precedence {
        match p {
            Prec::Routine => Precedence::Routine,
            Prec::Priority => Precedence::Priority,
            Prec::Immediate => Precedence::Immediate,
            Prec::Flash => Precedence::Flash,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn keygen(config_path: &Path, call: &str, out: Option<&Path>) -> Result<(), String> {
    let call = Callsign::parse(call).map_err(|e| format!("{call}: {e}"))?;
    let c = load_config(config_path)?;
    let out = out.map(Path::to_path_buf).unwrap_or(c.station.key.clone());
    let key = KeyFile::generate(call).map_err(|e| e.to_string())?;
    key.save(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("Wrote {} for {}.", out.display(), key.call);
    if !config_path.exists() {
        // Name the key relative to the settings file when it sits beside it.
        let rel = out
            .strip_prefix(config::dir_of(config_path))
            .map(Path::to_path_buf)
            .unwrap_or(out.clone());
        std::fs::write(config_path, config::starter(call, &rel))
            .map_err(|e| format!("{}: {e}", config_path.display()))?;
        println!(
            "Wrote {} with the settings, all commented out at their defaults.",
            config_path.display()
        );
    }
    println!("Give this line to stations that should verify your messages (`hm trust add`):");
    println!("{}", key.trust_line());
    Ok(())
}

fn whoami(config_path: &Path, key: Option<&Path>) -> Result<(), String> {
    let path = match key {
        Some(k) => k.to_path_buf(),
        None => load_config(config_path)?.station.key,
    };
    let k = KeyFile::load(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("{}", k.trust_line());
    Ok(())
}

fn trust_cmd(config_path: &Path, cmd: &TrustCmd) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", config_path.display());
    match cmd {
        TrustCmd::Add { line, note } => {
            let (call, key) = Trust::parse_line(line)
                .map_err(|e| format!("{line:?}: {e}"))?
                .ok_or("give the line `hm whoami` prints: CALL KEY")?;
            config::set_trust(config_path, call, &key, note.as_deref()).map_err(at)?;
            println!("Trusting {call}; saved to {}.", config_path.display());
        }
        TrustCmd::Remove { station } => {
            let call = Callsign::parse(station).map_err(|e| format!("{station}: {e}"))?;
            if !config::remove_trust(config_path, call).map_err(at)? {
                return Err(format!("{call} is not among the trusted stations"));
            }
            println!("No longer trusting {call}.");
        }
        TrustCmd::List => {
            let c = load_config(config_path)?;
            if c.trust.is_empty() {
                println!("No trusted stations. Add one with `hm trust add \"CALL KEY\"`.");
            }
            for t in &c.trust {
                let note = t.note.as_deref().map(|n| format!("  # {n}")).unwrap_or_default();
                println!("{} {}{note}", t.station, t.key);
            }
        }
    }
    Ok(())
}

fn send(
    c: &Config,
    to: &str,
    text: &str,
    subject: Option<&str>,
    prec: Prec,
    timeout: u64,
) -> Result<(), String> {
    let (key, me) = open_station(c)?;
    let trust = trusted(c)?;
    let to = Callsign::parse(to).map_err(|e| format!("{to}: {e}"))?;
    let bundle =
        station::build_bundle(&key, me, to, text, subject, prec.into()).map_err(|e| e.to_string())?;
    let object = bundle.to_vec();
    let (mut link, _) = open_kiss(c, me)?;
    eprintln!("{me} -> {to}: {} bytes, bundle {}", object.len(), bundle.id());
    let prec_u8 = Precedence::from(prec).to_u8();
    let st = Station {
        key: &key,
        trust: &trust,
        me,
        timing: timing(c),
    };
    let outcome = st
        .send_object(&mut link, to, object, prec_u8, Duration::from_secs(timeout))
        .map_err(|e| e.to_string())?;
    match outcome {
        SendOutcome::Delivered {
            rounds,
            after,
            receipt,
        } => {
            let check = match receipt {
                Receipt::Verified => "receipt verified".to_string(),
                Receipt::Unverified => format!("receipt NOT verified: {to} is not a trusted station"),
            };
            println!(
                "Delivered to {to} after {:.1} s in {rounds} over(s), {check}.",
                after.0 as f64 / 1000.0
            );
            Ok(())
        }
        SendOutcome::Failed(reason) => Err(format!("not delivered: {reason:?}")),
        SendOutcome::TimedOut => Err(format!("no confirmation within {timeout} s")),
    }
}

fn listen(c: &Config) -> Result<(), String> {
    let (key, me) = open_station(c)?;
    let trust = trusted(c)?;
    let (mut link, target) = open_kiss(c, me)?;
    eprintln!(
        "Listening as {me} on {} (TNC port {}). Ctrl-C to stop.",
        target.describe(),
        c.radio.tnc_port
    );
    let st = Station {
        key: &key,
        trust: &trust,
        me,
        timing: timing(c),
    };
    st.listen(&mut link, None, None, |m| {
        let when = station::utc_clock(unix_now());
        match (&m.bundle, &m.error) {
            (Some(b), _) => {
                let check = match m.verification {
                    Verification::Verified => "verified",
                    Verification::Unverified => "UNVERIFIED",
                    Verification::BadSignature => "BAD SIGNATURE",
                };
                let subject = b
                    .subject
                    .as_deref()
                    .map(|s| format!(" [{s}]"))
                    .unwrap_or_default();
                println!(
                    "{when} {} via {} ({check}) {:?}{subject}: {}",
                    b.from,
                    m.via,
                    b.kind,
                    m.text().unwrap_or("<no text>")
                );
            }
            (None, Some(e)) => println!("{when} object from {} is not a bundle: {e}", m.via),
            (None, None) => {}
        }
        Flow::Continue
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// The API token beside the store, created on first start (owner-only on Unix).
fn api_token(store: &Path) -> Result<String, String> {
    let path = store.with_extension("token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        return Ok(t.trim().to_string());
    }
    let mut raw = [0u8; 24];
    getrandom::fill(&mut raw).map_err(|e| format!("no system randomness: {e}"))?;
    let token = hm_cli::hex::encode(&raw);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write as _;
    opts.open(&path)
        .and_then(|mut f| f.write_all(format!("{token}\n").as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(token)
}

fn node(
    config_path: &Path,
    c: &Config,
    overridden: &[String],
    overrides: hm_cli::node::live::Overrides,
) -> Result<(), String> {
    let (key, me) = open_station(c)?;
    let live =
        hm_cli::node::live::Live::from_config(c).map_err(|e| format!("{}: {e}", config_path.display()))?;
    let listen = c.listen()?;
    let internet = (listen.is_some() || !live.peers.is_empty()).then(|| hm_cli::node::InternetConfig {
        listen: listen.unwrap_or_else(|| "0.0.0.0:0".parse().expect("valid")),
    });
    let radio = hm_cli::node::radio_config(&c.radio)?;
    if radio.is_none() && internet.is_none() {
        return Err(
            "no way to reach other stations: with the radio off, set internet.listen or add [[internet.peers]]"
                .into(),
        );
    }
    let store = c.station.store.clone();
    let token = api_token(&store)?;
    let cfg = hm_cli::node::NodeConfig {
        key,
        live,
        config_file: Some(config_path.to_path_buf()),
        overrides: Some(overrides),
        overridden: overridden.to_vec(),
        me,
        radio,
        radio_builder: Some(std::sync::Arc::new(hm_cli::node::radio_config)),
        internet,
        store: store.clone(),
        http: c.http()?,
        token: token.clone(),
        seed: None,
    };
    if !overridden.is_empty() {
        hm_cli::node::log(format!(
            "for this run, the command line overrides {} from {}",
            overridden.join(", "),
            config_path.display()
        ));
    }
    let handle = hm_cli::node::start(cfg).map_err(|e| e.to_string())?;
    println!("Web interface: http://{}/#token={token}", handle.http_addr);
    println!(
        "(The token is also in {}.)",
        store.with_extension("token").display()
    );
    handle.wait().map_err(|e| e.to_string())
}

fn audio_devices() -> Result<(), String> {
    let (inputs, outputs) = hm_rig::soundcard::devices().map_err(|e| e.to_string())?;
    println!("Capture:");
    inputs.iter().for_each(|d| println!("  {d}"));
    println!("Playback:");
    outputs.iter().for_each(|d| println!("  {d}"));
    println!("Use part of a name with `audio = \"NAME\"` under [radio], or `audio = \"default\"`.");
    Ok(())
}

/// station.toml with this run's overrides applied.
fn with_overrides(
    path: &Path,
    station: &StationArgs,
    node: Option<&NodeArgs>,
) -> Result<(Config, Vec<String>), String> {
    let mut c = load_config(path)?;
    let mut o = Vec::new();
    station.apply(&mut c, &mut o);
    if let Some(n) = node {
        n.apply(&mut c, &mut o)?;
    }
    Ok((c, o))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let path = cli.config.as_path();
    let result = match &cli.cmd {
        Cmd::Keygen { call, out } => keygen(path, call, out.as_deref()),
        Cmd::Whoami { key } => whoami(path, key.as_deref()),
        Cmd::Trust { cmd } => trust_cmd(path, cmd),
        Cmd::Send {
            station,
            to,
            text,
            subject,
            precedence,
            timeout,
        } => with_overrides(path, station, None)
            .and_then(|(c, _)| send(&c, to, text, subject.as_deref(), *precedence, *timeout)),
        Cmd::Listen { station } => with_overrides(path, station, None).and_then(|(c, _)| listen(&c)),
        Cmd::AudioDevices => audio_devices(),
        Cmd::Node { station, node: n } => {
            let (s2, n2) = (station.clone(), n.clone());
            let overrides: hm_cli::node::live::Overrides = std::sync::Arc::new(move |c: &mut Config| {
                let mut o = Vec::new();
                s2.apply(c, &mut o);
                let _ = n2.apply(c, &mut o);
            });
            with_overrides(path, station, Some(n)).and_then(|(c, o)| node(path, &c, &o, overrides))
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hm: {e}");
            ExitCode::FAILURE
        }
    }
}
