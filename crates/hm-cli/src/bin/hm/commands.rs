//! The commands that run here: keys, trust, sending and listening over a
//! KISS link, and the station daemon.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use hm_bundle::Precedence;
use hm_cli::config::{self, Config};
use hm_cli::driver::Flow;
use hm_cli::files::{KeyFile, Trust};
use hm_cli::kiss_link::{KissLink, KissTarget, TncParams};
use hm_cli::station::{self, LinkTiming, SendOutcome, Station, Verification};
use hm_wire::Callsign;
use hm_xfer::Receipt;

use crate::client::api_token;
use crate::TrustCmd;

/// station.toml with relative paths resolved against its directory.
pub(crate) fn load_config(path: &Path) -> Result<Config, String> {
    let mut c = Config::load(path).map_err(|e| e.to_string())?;
    c.resolve_paths(&config::dir_of(path));
    Ok(c)
}

/// The key and the callsign this station runs as.
pub(crate) fn open_station(c: &Config) -> Result<(KeyFile, Callsign), String> {
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

pub(crate) fn timing(c: &Config) -> LinkTiming {
    c.radio.timing()
}

pub(crate) fn tnc_params(c: &Config) -> TncParams {
    c.radio.tnc_params()
}

pub(crate) fn open_kiss(c: &Config, me: Callsign) -> Result<(KissLink, KissTarget), String> {
    let target = KissTarget::parse(&c.radio.kiss)?;
    let link = KissLink::open(&target, me, c.radio.tnc_port, tnc_params(c))
        .map_err(|e| format!("{}: {e}", target.describe()))?;
    Ok((link, target))
}

pub(crate) fn trusted(c: &Config) -> Result<Trust, String> {
    c.trust().map_err(|e| format!("station.toml: {e}"))
}

#[derive(Copy, Clone, ValueEnum)]
pub(crate) enum Prec {
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

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn keygen(config_path: &Path, call: &str, out: Option<&Path>) -> Result<(), String> {
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

pub(crate) fn whoami(config_path: &Path, key: Option<&Path>) -> Result<(), String> {
    let path = match key {
        Some(k) => k.to_path_buf(),
        None => load_config(config_path)?.station.key,
    };
    let k = KeyFile::load(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("{}", k.trust_line());
    Ok(())
}

pub(crate) fn trust_cmd(config_path: &Path, cmd: &TrustCmd) -> Result<(), String> {
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

pub(crate) fn send(
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
    let seq = if subject.is_none() {
        hm_store::Store::open(&c.station.store)
            .ok()
            .and_then(|store| store.next_peer_seq(to).ok())
            .or(Some(1))
    } else {
        None
    };
    let bundle =
        station::build_bundle(&key, me, to, text, subject, prec.into(), seq).map_err(|e| e.to_string())?;
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

pub(crate) fn listen(c: &Config) -> Result<(), String> {
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
                let text = m.text();
                println!(
                    "{when} {} via {} ({check}) {:?}{subject}: {}",
                    b.from,
                    m.via,
                    b.kind,
                    text.as_deref().unwrap_or("<no text>")
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

/// The ARQ modem `[modem]` describes, if enabled.
pub(crate) fn modem_config(c: &Config) -> Result<Option<hm_cli::node::arq::ArqConfig>, String> {
    let m = &c.modem;
    if !m.enabled {
        return Ok(None);
    }
    let ptt: Option<hm_cli::sound_link::PttFactory> = match m.ptt.as_str() {
        "none" | "" => None,
        spec => {
            let spec = spec.to_string();
            Some(std::sync::Arc::new(move || hm_rig::ptt::open(&spec)))
        }
    };
    Ok(Some(hm_cli::node::arq::ArqConfig {
        kind: hm_cli::node::arq::Kind::parse(&m.kind)?,
        host: m.host.clone(),
        port: m.port,
        bandwidth: m.bandwidth,
        ptt,
    }))
}

pub(crate) fn node(
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
        open_hub: c.internet.open_hub,
    });
    let radio = hm_cli::node::radio_config(&c.radio)?;
    let modem = modem_config(c)?;
    if radio.is_none() && internet.is_none() && modem.is_none() {
        return Err(
            "no way to reach other stations: with the radio off, set internet.listen, add [[internet.peers]] or enable [modem]"
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
        modem,
        schedules: c.contacts()?,
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

pub(crate) fn audio_devices() -> Result<(), String> {
    let (inputs, outputs) = hm_rig::soundcard::devices().map_err(|e| e.to_string())?;
    println!("Capture:");
    inputs.iter().for_each(|d| println!("  {d}"));
    println!("Playback:");
    outputs.iter().for_each(|d| println!("  {d}"));
    println!("Use part of a name with `audio = \"NAME\"` under [radio], or `audio = \"default\"`.");
    Ok(())
}
