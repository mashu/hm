//! `hm`: station keys, and sending and receiving messages through a KISS TNC.
//!
//! ```text
//! hm keygen --call SA0KAM
//! hm whoami
//! hm listen --kiss 127.0.0.1:8001 --trust trusted.txt
//! hm send --kiss 127.0.0.1:8001 --to SO5KM-1 --text "73 de SA0KAM"
//! hm send --kiss serial:/dev/ttyUSB0:57600 --to SO5KM-1 --text "via a hardware TNC"
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand, ValueEnum};
use hm_bundle::Precedence;
use hm_cli::driver::Flow;
use hm_cli::files::{KeyFile, Trust};
use hm_cli::kiss_link::{KissLink, KissTarget, TncParams};
use hm_cli::station::{self, LinkTiming, SendOutcome, Station, Verification};
use hm_wire::Callsign;
use hm_xfer::Receipt;

#[derive(Parser)]
#[command(name = "hm", version, about = "hm-net: mail and chat over packet radio")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a station key file.
    Keygen {
        /// Your callsign. With an SSID (SA0KAM-2) the key is for that station only;
        /// without one it can run as any SSID chosen with `--ssid`.
        #[arg(long)]
        call: String,
        #[arg(long, default_value = "station.key")]
        out: PathBuf,
    },
    /// Print the line other stations add to their trust file to verify you.
    Whoami {
        #[arg(long, default_value = "station.key")]
        key: PathBuf,
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
        /// Trust file; the receiver's delivery receipt is verified when its key is listed.
        #[arg(long)]
        trust: Option<PathBuf>,
    },
    /// Run the station: message store, radio and/or internet links, web interface.
    Node {
        #[command(flatten)]
        station: StationArgs,
        /// File of `CALL PUBLICKEY` lines (from `hm whoami`) for verifying other stations.
        #[arg(long)]
        trust: Option<PathBuf>,
        /// Message store file (created if missing). The API token is kept beside it.
        #[arg(long, default_value = "station.db")]
        store: PathBuf,
        /// Address of the web interface and API.
        #[arg(long, default_value = "127.0.0.1:8080")]
        http: std::net::SocketAddr,
        /// Run without a radio, for example on an internet server.
        #[arg(long)]
        no_radio: bool,
        /// Accept internet links from trusted stations on this address, e.g. 0.0.0.0:4433.
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Keep an internet link to a trusted station, as CALL=host:port. Repeatable.
        #[arg(long = "peer", value_name = "CALL=HOST:PORT")]
        peers: Vec<String>,
        /// Relative cost of a delivery attempt by radio.
        #[arg(long, default_value_t = 1.0)]
        radio_cost: f64,
        /// Relative cost of a delivery attempt over the internet.
        #[arg(long, default_value_t = 2.0)]
        internet_cost: f64,
        /// Send a signed BEACON (presence and identification) about this often
        /// on the radio, in minutes; 0 sends none.
        #[arg(long, default_value_t = 10)]
        beacon_minutes: u64,
    },
    /// List the sound cards the built-in modem can use.
    AudioDevices,
    /// Print messages addressed to this station until interrupted.
    Listen {
        #[command(flatten)]
        station: StationArgs,
        /// File of `CALL PUBLICKEY` lines (from `hm whoami`) for verifying senders.
        #[arg(long)]
        trust: Option<PathBuf>,
    },
}

#[derive(Args)]
struct StationArgs {
    /// Station key file, from `hm keygen`.
    #[arg(long, default_value = "station.key")]
    key: PathBuf,
    /// SSID to operate as (0-15), when the key file has none.
    #[arg(long, default_value_t = 0)]
    ssid: u8,
    /// KISS TNC: a TCP server such as Direwolf's KISSPORT (`HOST:PORT`), or a
    /// hardware TNC on a serial port (`serial:DEVICE[:BAUD]`, or just `/dev/ttyUSB0`
    /// or `COM3` at 9600 Bd).
    #[arg(long, default_value = "127.0.0.1:8001")]
    kiss: String,
    /// TNC port (Direwolf channel) to use.
    #[arg(long, default_value_t = 0)]
    tnc_port: u8,
    /// Use the built-in modem on this sound card (`default` or part of its name)
    /// instead of a KISS TNC. `hm node` only.
    #[arg(long)]
    audio: Option<String>,
    /// How to key the transmitter with the built-in modem: `vox`, `rigctld[:HOST:PORT]`,
    /// `rts:DEVICE`, `dtr:DEVICE` or `cm108:HIDRAW[:GPIO]`.
    #[arg(long, default_value = "vox")]
    ptt: String,
    /// CSMA persistence with the built-in modem or a serial TNC: transmit chance
    /// per clear slot is (p+1)/256.
    #[arg(long, default_value_t = 63)]
    persist: u8,
    /// CSMA slot time in milliseconds (built-in modem or serial TNC).
    #[arg(long, default_value_t = 100)]
    slottime: u64,
    /// Link bitrate, for predicting when overs end.
    #[arg(long, default_value_t = 1200)]
    bitrate: u32,
    /// Key-up delay in milliseconds; also sent to a serial TNC.
    #[arg(long, default_value_t = 300)]
    txdelay: u64,
    /// Extra slack around predicted over ends, in milliseconds.
    #[arg(long, default_value_t = 1500)]
    guard: u64,
}

impl StationArgs {
    fn timing(&self) -> LinkTiming {
        LinkTiming {
            bitrate_bps: self.bitrate,
            txdelay_ms: self.txdelay,
            guard_ms: self.guard,
            max_rounds: 12,
        }
    }

    fn kiss_target(&self) -> Result<KissTarget, String> {
        KissTarget::parse(&self.kiss)
    }

    fn tnc_params(&self) -> TncParams {
        TncParams {
            txdelay_ms: self.txdelay as u32,
            persist: self.persist,
            slot_ms: self.slottime as u32,
        }
    }

    fn open_kiss(&self, me: Callsign) -> Result<KissLink, String> {
        let target = self.kiss_target()?;
        KissLink::open(&target, me, self.tnc_port, self.tnc_params())
            .map_err(|e| format!("{}: {e}", target.describe()))
    }

    fn open(&self) -> Result<(KeyFile, Callsign), String> {
        let key = KeyFile::load(&self.key).map_err(|e| format!("{}: {e}", self.key.display()))?;
        let me = if self.ssid == 0 {
            key.call
        } else if key.call != key.call.base() {
            return Err(format!(
                "the key file is for {} only; leave out --ssid, or make a key for another SSID",
                key.call
            ));
        } else {
            if self.ssid > 15 {
                return Err("SSID must be 0-15".into());
            }
            Callsign::parse(&format!("{}-{}", key.call, self.ssid)).map_err(|e| e.to_string())?
        };
        Ok((key, me))
    }
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

fn keygen(call: &str, out: &Path) -> Result<(), String> {
    let call = Callsign::parse(call).map_err(|e| format!("{call}: {e}"))?;
    let key = KeyFile::generate(call).map_err(|e| e.to_string())?;
    key.save(out).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("Wrote {} for {}.", out.display(), key.call);
    println!("Give this line to stations that should verify your messages:");
    println!("{}", key.trust_line());
    Ok(())
}

fn load_trust(path: Option<&Path>) -> Result<Trust, String> {
    match path {
        Some(p) => Trust::load(p).map_err(|e| format!("{}: {e}", p.display())),
        None => Ok(Trust::default()),
    }
}

fn send(
    s: &StationArgs,
    to: &str,
    text: &str,
    subject: Option<&str>,
    prec: Prec,
    timeout: u64,
    trust: Option<&Path>,
) -> Result<(), String> {
    let (key, me) = s.open()?;
    let trust = load_trust(trust)?;
    let to = Callsign::parse(to).map_err(|e| format!("{to}: {e}"))?;
    let bundle =
        station::build_bundle(&key, me, to, text, subject, prec.into()).map_err(|e| e.to_string())?;
    let object = bundle.to_vec();
    let mut link = s.open_kiss(me)?;
    eprintln!("{me} -> {to}: {} bytes, bundle {}", object.len(), bundle.id());
    let prec_u8 = Precedence::from(prec).to_u8();
    let st = Station {
        key: &key,
        trust: &trust,
        me,
        timing: s.timing(),
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
                Receipt::Unverified => {
                    format!("receipt NOT verified: no key for {to} in the trust file")
                }
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

fn listen(s: &StationArgs, trust: Option<&Path>) -> Result<(), String> {
    let (key, me) = s.open()?;
    let trust = load_trust(trust)?;
    let mut link = s.open_kiss(me)?;
    eprintln!(
        "Listening as {me} on {} (TNC port {}). Ctrl-C to stop.",
        s.kiss_target()?.describe(),
        s.tnc_port
    );
    let st = Station {
        key: &key,
        trust: &trust,
        me,
        timing: s.timing(),
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

fn parse_peer(spec: &str) -> Result<(Callsign, std::net::SocketAddr), String> {
    use std::net::ToSocketAddrs;
    let (call, addr) = spec
        .split_once('=')
        .ok_or_else(|| format!("--peer {spec}: expected CALL=HOST:PORT"))?;
    let call = Callsign::parse(call).map_err(|e| format!("--peer {spec}: {e}"))?;
    let addr = addr
        .to_socket_addrs()
        .map_err(|e| format!("--peer {spec}: {e}"))?
        .next()
        .ok_or_else(|| format!("--peer {spec}: no address"))?;
    Ok((call, addr))
}

#[allow(clippy::too_many_arguments)]
fn node(
    s: &StationArgs,
    trust: Option<&Path>,
    store: &Path,
    http: std::net::SocketAddr,
    no_radio: bool,
    listen: Option<std::net::SocketAddr>,
    peers: &[String],
    costs: hm_cli::node::choose::Costs,
    beacon_minutes: u64,
) -> Result<(), String> {
    let (key, me) = s.open()?;
    let trust_file = trust.map(Path::to_path_buf);
    let trust = load_trust(trust)?;
    let peers = peers
        .iter()
        .map(|p| parse_peer(p))
        .collect::<Result<Vec<_>, _>>()?;
    let internet = (listen.is_some() || !peers.is_empty()).then(|| hm_cli::node::InternetConfig {
        listen: listen.unwrap_or_else(|| "0.0.0.0:0".parse().expect("valid")),
        peers,
    });
    let kiss_target = match no_radio || s.audio.is_some() {
        true => None,
        false => Some(s.kiss_target()?),
    };
    let radio = (!no_radio).then(|| {
        let link = match (&s.audio, kiss_target) {
            (None, Some(target)) => hm_cli::node::RadioLink::Kiss {
                target,
                tnc_port: s.tnc_port,
                params: s.tnc_params(),
            },
            (None, None) => unreachable!("a KISS target is parsed whenever there is radio without --audio"),
            (Some(device), _) => {
                let (device, spec) = (device.clone(), s.ptt.clone());
                let describe = format!("sound card {device}, PTT {spec}");
                hm_cli::node::RadioLink::Modem {
                    audio: std::sync::Arc::new(move || {
                        Ok(Box::new(hm_rig::soundcard::SoundCard::open(&device)?)
                            as Box<dyn hm_rig::AudioPort>)
                    }),
                    ptt: std::sync::Arc::new(move || hm_rig::ptt::open(&spec)),
                    csma: hm_cli::sound_link::Csma {
                        persist: s.persist,
                        slot: Duration::from_millis(s.slottime),
                        txdelay_ms: s.txdelay as u32,
                        ..Default::default()
                    },
                    describe,
                }
            }
        };
        hm_cli::node::RadioConfig {
            link,
            timing: s.timing(),
            beacon_every: (beacon_minutes > 0).then(|| Duration::from_secs(60 * beacon_minutes)),
        }
    });
    if radio.is_none() && internet.is_none() {
        return Err("with --no-radio, give --listen or --peer so the node has a way to reach others".into());
    }
    let token = api_token(store)?;
    let cfg = hm_cli::node::NodeConfig {
        key,
        trust,
        trust_file,
        me,
        radio,
        internet,
        costs,
        store: store.to_path_buf(),
        http,
        retry: hm_store::RetryPolicy::default(),
        token: token.clone(),
        seed: None,
    };
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
    println!("Use part of a name with `hm node --audio NAME`, or `--audio default`.");
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.cmd {
        Cmd::Keygen { call, out } => keygen(call, out),
        Cmd::Whoami { key } => KeyFile::load(key)
            .map(|k| println!("{}", k.trust_line()))
            .map_err(|e| format!("{}: {e}", key.display())),
        Cmd::Send {
            station,
            to,
            text,
            subject,
            precedence,
            timeout,
            trust,
        } => send(
            station,
            to,
            text,
            subject.as_deref(),
            *precedence,
            *timeout,
            trust.as_deref(),
        ),
        Cmd::Listen { station, trust } => listen(station, trust.as_deref()),
        Cmd::AudioDevices => audio_devices(),
        Cmd::Node {
            station,
            trust,
            store,
            http,
            no_radio,
            listen,
            peers,
            radio_cost,
            internet_cost,
            beacon_minutes,
        } => node(
            station,
            trust.as_deref(),
            store,
            *http,
            *no_radio,
            *listen,
            peers,
            hm_cli::node::choose::Costs {
                radio: *radio_cost,
                internet: *internet_cost,
            },
            *beacon_minutes,
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hm: {e}");
            ExitCode::FAILURE
        }
    }
}
