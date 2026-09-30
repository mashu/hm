//! Station nodes on a fake radio channel and/or localhost internet links,
//! driven only through the HTTP API.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hm_cli::config::{Config, RadioSettings, RelaySettings};
use hm_cli::files::{KeyFile, Trust};
use hm_cli::kiss_link::{KissTarget, TncParams};
use hm_cli::node::live::Costs;
use hm_cli::node::live::Live;
use hm_cli::node::{self, NodeConfig, NodeHandle, RadioConfig, RadioLink};
use hm_cli::station::LinkTiming;
use hm_route::ScheduledContact;
use hm_store::{Direction, RetryPolicy, State, Store};
use hm_wire::{Callsign, Locator};
use serde_json::{json, Value};

#[path = "../common/mod.rs"]
mod common;
use common::fake_tnc;

mod api;
mod gateway;
mod internet;
mod live;
mod radio;

const FAST: LinkTiming = LinkTiming {
    bitrate_bps: 9600,
    txdelay_ms: 50,
    guard_ms: 300,
    max_rounds: 3,
    max_keyup_ms: 20_000,
};
const TOKEN: &str = "test-token";
const QUICK: RetryPolicy = RetryPolicy {
    first_delay_secs: 1,
    max_delay_secs: 2,
    max_attempts: 100,
};

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

/// Minimal HTTP/1.1 client.
fn raw_http(addr: SocketAddr, method: &str, path: &str, body: Option<&Value>, token: Option<&str>) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    resp
}

fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> (u16, String) {
    let resp = raw_http(addr, method, path, body, token);
    let status = resp[9..12].parse().unwrap();
    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn get(addr: SocketAddr, path: &str) -> Value {
    let (status, body) = http(addr, "GET", path, None, Some(TOKEN));
    assert_eq!(status, 200, "{path}: {body}");
    serde_json::from_str(&body).unwrap()
}

fn send(addr: SocketAddr, msg: Value) {
    let (status, body) = http(addr, "POST", "/api/send", Some(&msg), Some(TOKEN));
    assert_eq!(status, 201, "{body}");
}

fn wait_for<T>(limit: Duration, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

struct Tmp(PathBuf);
impl Tmp {
    fn new(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("hm-node-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn keys() -> (KeyFile, KeyFile) {
    (
        KeyFile::generate(call("SA0KAM")).unwrap(),
        KeyFile::generate(call("SO5KM")).unwrap(),
    )
}

/// A node's internet side: where it listens, and whom it keeps a link to.
struct InternetConfig {
    listen: SocketAddr,
    peers: Vec<(Callsign, SocketAddr)>,
}

struct Setup<'a> {
    key: &'a KeyFile,
    me: &'a str,
    peer: &'a KeyFile,
    /// More stations to trust, besides `peer`.
    also: &'a [&'a KeyFile],
    tnc: Option<SocketAddr>,
    internet: Option<InternetConfig>,
    store: &'a Tmp,
    retry: RetryPolicy,
    beacon_every: Option<Duration>,
    /// Trust exactly what this station.toml lists (instead of `peer` and
    /// `also`), and keep it as the node's settings file.
    trust_file: Option<PathBuf>,
}

fn start(s: Setup) -> NodeHandle {
    start_routed(s, Default::default(), vec![])
}

fn start_routed(s: Setup, relay: RelaySettings, schedules: Vec<ScheduledContact>) -> NodeHandle {
    let mut trust = Trust::default();
    trust.insert(s.peer.call, s.peer.identity.public());
    for k in s.also {
        trust.insert(k.call, k.identity.public());
    }
    if let Some(f) = &s.trust_file {
        trust = Config::load(f).unwrap().trust().unwrap();
    }
    let peers = s.internet.as_ref().map_or(vec![], |i| {
        i.peers.iter().map(|(c, a)| (*c, a.to_string())).collect()
    });
    node::start(NodeConfig {
        key: KeyFile::parse(&s.key.to_text()).unwrap(),
        live: Live {
            trust,
            notes: vec![],
            costs: Costs::default(),
            retry: s.retry,
            receipt_retry: RetryPolicy {
                first_delay_secs: s.retry.first_delay_secs,
                max_delay_secs: s.retry.max_delay_secs,
                max_attempts: s.retry.max_attempts.saturating_mul(2).max(24),
            },
            custody_grace_secs: 6 * 3600,
            custody_suspect_secs: 24 * 3600,
            relay,
            beacon_secs: s.beacon_every.map_or(0, |d| d.as_secs()),
            peers,
            locator: Locator::parse("JO89xi").ok(),
            radio: RadioSettings {
                enabled: s.tnc.is_some(),
                kiss: s.tnc.map_or(RadioSettings::default().kiss, |t| t.to_string()),
                bitrate: FAST.bitrate_bps,
                txdelay_ms: FAST.txdelay_ms,
                guard_ms: FAST.guard_ms,
                ..RadioSettings::default()
            },
        },
        config_file: s.trust_file,
        overrides: None,
        overridden: vec![],
        me: call(s.me),
        radio: s.tnc.map(|t| RadioConfig {
            link: RadioLink::Kiss {
                target: KissTarget::Tcp(t.to_string()),
                tnc_port: 0,
                params: TncParams::default(),
            },
            timing: FAST,
        }),
        radio_builder: Some(Arc::new(node::radio_config)),
        internet: s.internet.map(|i| node::InternetConfig {
            listen: i.listen,
            open_hub: false,
        }),
        modem: None,
        schedules,
        store: s.store.0.clone(),
        http: "127.0.0.1:0".parse().unwrap(),
        token: TOKEN.into(),
        seed: Some(1),
    })
    .unwrap()
}

fn inbox(addr: SocketAddr) -> Vec<Value> {
    get(addr, "/api/messages?direction=in")
        .as_array()
        .unwrap()
        .clone()
}

/// Waits for `n` of the operator's own messages to be delivered, and returns
/// the newest of them. Receipts this station sends are its own business:
/// they are delivered once handed on, and are not counted.
fn delivered(addr: SocketAddr, n: usize, what: &str) -> Value {
    wait_for(Duration::from_secs(120), what, || {
        let out = get(addr, "/api/messages?direction=out");
        let mine: Vec<&Value> = out
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["kind"] != "Receipt" && m["kind"] != "CustodyFail")
            .collect();
        let done = mine.iter().filter(|m| m["state"] == "Delivered").count();
        (done >= n).then(|| mine[0].clone())
    })
}
