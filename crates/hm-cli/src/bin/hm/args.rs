//! Flags that override `station.toml` for one run.

use std::path::PathBuf;

use clap::Args;
use hm_cli::config::{self, Config};

/// Overrides for this run; anything not given comes from station.toml.
#[derive(Args, Clone)]
pub(crate) struct StationArgs {
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
    /// Built-in modem framing: ax25, il2p or auto [radio.framing].
    #[arg(long)]
    framing: Option<String>,
    /// Radio handoff attempts before giving up [radio.max_rounds].
    #[arg(long)]
    max_rounds: Option<u32>,
}

/// `hm node` overrides for this run.
#[derive(Args, Clone)]
pub(crate) struct NodeArgs {
    /// Message store [station.store].
    #[arg(long)]
    store: Option<PathBuf>,
    /// Address of the web page and API [station.http].
    #[arg(long)]
    http: Option<String>,
    /// Run without a radio [radio.enabled = false].
    #[arg(long)]
    no_radio: bool,
    /// Run without internet listen or dial peers [internet.listen cleared, peers empty].
    #[arg(long)]
    no_internet: bool,
    /// Accept internet links from trusted stations here [internet.listen].
    #[arg(long)]
    listen: Option<String>,
    /// Keep an internet link to a station, as CALL=HOST:PORT; repeatable,
    /// replaces [[internet.peers]] for this run.
    #[arg(long = "peer", value_name = "CALL=HOST:PORT")]
    peers: Vec<String>,
    /// Accept inbound internet links from any valid certificate [internet.open_hub].
    #[arg(long)]
    open_hub: Option<bool>,
    /// Cost of a minute of radio airtime, in hundredths of a message's value [delivery.radio_cost].
    #[arg(long)]
    radio_cost: Option<f64>,
    /// Cost of an attempt over the internet, in hundredths of a message's value [delivery.internet_cost].
    #[arg(long)]
    internet_cost: Option<f64>,
    /// Cost of a minute of ARQ modem airtime, in hundredths of a message's value [delivery.modem_cost].
    #[arg(long)]
    modem_cost: Option<f64>,
    /// First retry delay in seconds [delivery.retry_first_secs].
    #[arg(long)]
    retry_first_secs: Option<u64>,
    /// Cap on retry backoff in seconds [delivery.retry_max_secs].
    #[arg(long)]
    retry_max_secs: Option<u64>,
    /// Give up after this many delivery attempts [delivery.retry_attempts].
    #[arg(long)]
    retry_attempts: Option<u32>,
    /// Shadow retain after hop handoff, seconds [delivery.custody_grace_secs].
    #[arg(long)]
    custody_grace_secs: Option<u64>,
    /// Reclaim in-transit custody after this many seconds [delivery.custody_suspect_secs].
    #[arg(long)]
    custody_suspect_secs: Option<u64>,
    /// Retry budget for end-to-end receipts [delivery.receipt_retry_attempts].
    #[arg(long)]
    receipt_retry_attempts: Option<u32>,
    /// Accept multi-hop relay custody [relay.enabled].
    #[arg(long)]
    relay: Option<bool>,
    /// Hold mail for intermittently connected stations [relay.mailbox].
    #[arg(long)]
    mailbox: Option<bool>,
    /// Max relay holdings count [relay.max_holdings].
    #[arg(long)]
    relay_max_holdings: Option<usize>,
    /// Max relay holdings bytes [relay.max_bytes].
    #[arg(long)]
    relay_max_bytes: Option<u64>,
    /// Max route hops for relayed traffic [relay.max_hops].
    #[arg(long)]
    relay_max_hops: Option<u8>,
    /// Per-bundle radio airtime budget, seconds [relay.airtime_budget_secs].
    #[arg(long)]
    relay_airtime_budget_secs: Option<u64>,
    /// Fraction of radio airtime for control [relay.control_airtime_fraction].
    #[arg(long)]
    relay_control_airtime_fraction: Option<f64>,
    /// Enable an ARQ modem program [modem.enabled].
    #[arg(long)]
    modem: Option<bool>,
    /// ARQ modem kind: vara or ardop [modem.kind].
    #[arg(long)]
    modem_kind: Option<String>,
    /// ARQ modem host [modem.host].
    #[arg(long)]
    modem_host: Option<String>,
    /// ARQ modem command port [modem.port].
    #[arg(long)]
    modem_port: Option<u16>,
    /// ARQ modem bandwidth, Hz; 0 leaves the modem's setting [modem.bandwidth].
    #[arg(long)]
    modem_bandwidth: Option<u32>,
    /// ARQ modem PTT: none, or the same forms as --ptt [modem.ptt].
    #[arg(long)]
    modem_ptt: Option<String>,
    /// Beacon interval in minutes, 0 for none [radio.beacon_minutes].
    #[arg(long)]
    beacon_minutes: Option<u64>,
    /// Grid locator sent in beacons, like JO89xi [station.locator].
    #[arg(long)]
    locator: Option<String>,
}

/// Put `v` in `slot` if given, noting which setting was overridden.
pub(crate) fn set<T>(slot: &mut T, v: Option<T>, name: &str, overridden: &mut Vec<String>) {
    if let Some(v) = v {
        *slot = v;
        overridden.push(name.to_string());
    }
}

impl StationArgs {
    pub(crate) fn apply(&self, c: &mut Config, o: &mut Vec<String>) {
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
        set(&mut c.radio.framing, self.framing.clone(), "radio.framing", o);
        set(&mut c.radio.max_rounds, self.max_rounds, "radio.max_rounds", o);
    }
}

impl NodeArgs {
    pub(crate) fn apply(&self, c: &mut Config, o: &mut Vec<String>) -> Result<(), String> {
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
        set(&mut c.internet.open_hub, self.open_hub, "internet.open_hub", o);
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
        if self.no_internet {
            if self.listen.is_some() || !self.peers.is_empty() {
                return Err("--no-internet cannot be combined with --listen or --peer".into());
            }
            set(&mut c.internet.listen, Some(None), "internet.listen", o);
            set(&mut c.internet.peers, Some(Vec::new()), "internet.peers", o);
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
            &mut c.delivery.modem_cost,
            self.modem_cost,
            "delivery.modem_cost",
            o,
        );
        set(
            &mut c.delivery.retry_first_secs,
            self.retry_first_secs,
            "delivery.retry_first_secs",
            o,
        );
        set(
            &mut c.delivery.retry_max_secs,
            self.retry_max_secs,
            "delivery.retry_max_secs",
            o,
        );
        set(
            &mut c.delivery.retry_attempts,
            self.retry_attempts,
            "delivery.retry_attempts",
            o,
        );
        set(
            &mut c.delivery.custody_grace_secs,
            self.custody_grace_secs,
            "delivery.custody_grace_secs",
            o,
        );
        set(
            &mut c.delivery.custody_suspect_secs,
            self.custody_suspect_secs,
            "delivery.custody_suspect_secs",
            o,
        );
        set(
            &mut c.delivery.receipt_retry_attempts,
            self.receipt_retry_attempts,
            "delivery.receipt_retry_attempts",
            o,
        );
        set(&mut c.relay.enabled, self.relay, "relay.enabled", o);
        set(&mut c.relay.mailbox, self.mailbox, "relay.mailbox", o);
        set(
            &mut c.relay.max_holdings,
            self.relay_max_holdings,
            "relay.max_holdings",
            o,
        );
        set(&mut c.relay.max_bytes, self.relay_max_bytes, "relay.max_bytes", o);
        set(&mut c.relay.max_hops, self.relay_max_hops, "relay.max_hops", o);
        set(
            &mut c.relay.airtime_budget_secs,
            self.relay_airtime_budget_secs,
            "relay.airtime_budget_secs",
            o,
        );
        set(
            &mut c.relay.control_airtime_fraction,
            self.relay_control_airtime_fraction,
            "relay.control_airtime_fraction",
            o,
        );
        set(&mut c.modem.enabled, self.modem, "modem.enabled", o);
        set(&mut c.modem.kind, self.modem_kind.clone(), "modem.kind", o);
        set(&mut c.modem.host, self.modem_host.clone(), "modem.host", o);
        set(&mut c.modem.port, self.modem_port, "modem.port", o);
        set(&mut c.modem.bandwidth, self.modem_bandwidth, "modem.bandwidth", o);
        set(&mut c.modem.ptt, self.modem_ptt.clone(), "modem.ptt", o);
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
        c.modem.check()?;
        Ok(())
    }
}
