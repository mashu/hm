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

mod common;
use common::fake_tnc;

const FAST: LinkTiming = LinkTiming {
    bitrate_bps: 9600,
    txdelay_ms: 50,
    guard_ms: 300,
    max_rounds: 3,
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

#[test]
fn queued_outbound_message_can_be_dropped() {
    let (alice, bob) = keys();
    let db = Tmp::new("cancel");
    let node = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: None,
        internet: None,
        store: &db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let (status, body) = http(
        node.http_addr,
        "POST",
        "/api/send",
        Some(&json!({"to": "SO5KM-1", "text": "cancel me"})),
        Some(TOKEN),
    );
    assert_eq!(status, 201, "{body}");
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let detail = get(node.http_addr, &format!("/api/messages/{id}"));
    assert_eq!(
        (detail["id"].as_str(), detail["state"].as_str()),
        (Some(id.as_str()), Some("Queued"))
    );
    assert!(detail["raw_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        detail["raw_hex"].as_str().unwrap().len(),
        detail["raw_bytes"].as_u64().unwrap() as usize * 2
    );
    assert_eq!(
        http(
            node.http_addr,
            "DELETE",
            &format!("/api/messages/{id}"),
            None,
            Some(TOKEN)
        )
        .0,
        204
    );
    assert_eq!(
        get(node.http_addr, "/api/messages?direction=out")[0]["state"],
        "Cancelled"
    );
    assert_eq!(
        http(
            node.http_addr,
            "DELETE",
            &format!("/api/messages/{id}"),
            None,
            Some(TOKEN)
        )
        .0,
        204
    );
    assert!(get(node.http_addr, "/api/messages?direction=out")
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        http(
            node.http_addr,
            "DELETE",
            &format!("/api/messages/{id}"),
            None,
            Some(TOKEN)
        )
        .0,
        404
    );

    send(node.http_addr, json!({"to": "SO5KM-1", "text": "keep pending"}));
    let (status, body) = http(
        node.http_addr,
        "DELETE",
        "/api/conversations/SO5KM-1",
        None,
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let cleared: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (cleared["deleted"].as_u64(), cleared["active"].as_u64()),
        (Some(0), Some(1))
    );
    assert_eq!(
        get(node.http_addr, "/api/messages?direction=out")[0]["state"],
        "Queued"
    );
    let pending_id = get(node.http_addr, "/api/messages?direction=out")[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        http(
            node.http_addr,
            "DELETE",
            &format!("/api/messages/{pending_id}"),
            None,
            Some(TOKEN)
        )
        .0,
        204
    );
    let (status, body) = http(
        node.http_addr,
        "DELETE",
        "/api/conversations/SO5KM-1",
        None,
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let cleared: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (cleared["deleted"].as_u64(), cleared["active"].as_u64()),
        (Some(1), Some(0))
    );
    assert!(get(node.http_addr, "/api/messages?direction=out")
        .as_array()
        .unwrap()
        .is_empty());
    node.stop().unwrap();
}

#[test]
fn four_internet_nodes_relay_end_to_end_without_flooding() {
    let alice = KeyFile::generate(call("SA0KAM")).unwrap();
    let relay_one = KeyFile::generate(call("SM0R1")).unwrap();
    let relay_two = KeyFile::generate(call("SM0R2")).unwrap();
    let bob = KeyFile::generate(call("SO5KM-1")).unwrap();
    let (a_db, r1_db, r2_db, b_db) = (
        Tmp::new("route-a"),
        Tmp::new("route-r1"),
        Tmp::new("route-r2"),
        Tmp::new("route-b"),
    );
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &relay_two,
        also: &[&alice, &relay_one],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &b_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let relay = RelaySettings {
        enabled: true,
        mailbox: true,
        ..RelaySettings::default()
    };
    let r2 = start_routed(
        Setup {
            key: &relay_two,
            me: "SM0R2",
            peer: &bob,
            also: &[&alice, &relay_one],
            tnc: None,
            internet: Some(InternetConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                peers: vec![(bob.call, b.internet_addr.unwrap())],
            }),
            store: &r2_db,
            retry: QUICK,
            beacon_every: None,
            trust_file: None,
        },
        relay.clone(),
        vec![],
    );
    let r1 = start_routed(
        Setup {
            key: &relay_one,
            me: "SM0R1",
            peer: &relay_two,
            also: &[&alice, &bob],
            tnc: None,
            internet: Some(InternetConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                peers: vec![(relay_two.call, r2.internet_addr.unwrap())],
            }),
            store: &r1_db,
            retry: QUICK,
            beacon_every: None,
            trust_file: None,
        },
        relay,
        vec![],
    );
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &relay_one,
        also: &[&relay_two, &bob],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(relay_one.call, r1.internet_addr.unwrap())],
        }),
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    wait_for(Duration::from_secs(20), "the three authenticated links", || {
        let links = [
            get(a.http_addr, "/api/status")["internet_peers"]
                .as_array()
                .unwrap()
                .len(),
            get(r1.http_addr, "/api/status")["internet_peers"]
                .as_array()
                .unwrap()
                .len(),
            get(r2.http_addr, "/api/status")["internet_peers"]
                .as_array()
                .unwrap()
                .len(),
            get(b.http_addr, "/api/status")["internet_peers"]
                .as_array()
                .unwrap()
                .len(),
        ];
        (links == [1, 2, 2, 1]).then_some(())
    });
    send(
        a.http_addr,
        json!({"to": "SO5KM-1", "subject": "Multi-hop", "text": "A-R1-R2-B"}),
    );
    let sent = delivered(a.http_addr, 1, "four-node end-to-end receipt");
    assert_eq!(sent["state"], "Delivered");
    assert_eq!(inbox(b.http_addr)[0]["text"], "A-R1-R2-B");

    a.stop().unwrap();
    r1.stop().unwrap();
    r2.stop().unwrap();
    b.stop().unwrap();
    for db in [&r1_db, &r2_db] {
        let records = Store::open(&db.0).unwrap().list(Direction::Relay, 10).unwrap();
        let original = records
            .iter()
            .find(|record| record.final_destination() == bob.call)
            .expect("relay retained one audit copy of the original");
        // Bob's receipt came back the same way and closed each holding, so
        // no relay resends the message when its suspect timer fires.
        assert_eq!(original.state, State::Delivered);
        assert!(original.e2e_receipt.is_some());
    }
}

#[test]
fn radio_nodes_exchange_mail_and_survive_a_restart() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("a1"), Tmp::new("b1"));
    let b_setup = || Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &alice,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &b_db,
        retry: RetryPolicy::default(),
        beacon_every: None,
        trust_file: None,
    };
    let b = start(b_setup());
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &a_db,
        retry: RetryPolicy::default(),
        beacon_every: None,
        trust_file: None,
    });

    // The page is public; the API wants the token.
    let (status, page) = http(a.http_addr, "GET", "/", None, None);
    assert!(status == 200 && page.contains("Queue message"));
    assert!(
        page.contains("Archive")
            && page.contains("Clear history")
            && page.contains("View message details and raw signed object")
            && page.contains("Drop this queued message")
    );
    let headers = raw_http(a.http_addr, "GET", "/", None, None).to_ascii_lowercase();
    assert!(headers.contains("content-security-policy:"));
    assert!(headers.contains("permissions-policy:"));
    assert!(headers.contains("x-frame-options: deny"));
    assert!(headers.contains("cache-control: no-store"));
    assert_eq!(http(a.http_addr, "GET", "/api/status", None, None).0, 401);
    assert_eq!(
        http(a.http_addr, "GET", "/api/status", None, Some("wrong")).0,
        401
    );
    let (status, err) = http(
        a.http_addr,
        "POST",
        "/api/send",
        Some(&json!({"to": "NOT A CALL", "text": "x"})),
        Some(TOKEN),
    );
    assert!(status == 400 && err.contains("to:"), "{status} {err}");
    let st = get(a.http_addr, "/api/status");
    assert_eq!(
        (st["call"].as_str(), st["internet_listen"].is_null()),
        (Some("SA0KAM"), true)
    );

    send(
        a.http_addr,
        json!({"to": "SO5KM-1", "subject": "Sked", "text": "40m 7.047 at 19Z?", "precedence": "priority"}),
    );
    let got = wait_for(Duration::from_secs(30), "the message at SO5KM-1", || {
        let i = inbox(b.http_addr);
        (i.len() == 1).then(|| i[0].clone())
    });
    assert_eq!(
        (got["from"].as_str(), got["subject"].as_str()),
        (Some("SA0KAM"), Some("Sked"))
    );
    assert_eq!(
        (got["verified"].as_bool(), got["state"].as_str()),
        (Some(true), Some("Unread"))
    );
    let sent = delivered(a.http_addr, 1, "delivery");
    assert_eq!(
        (sent["verified"].as_bool(), sent["delivered_by"].as_str()),
        (Some(true), Some("radio"))
    );

    let id = got["id"].as_str().unwrap();
    assert_eq!(
        http(b.http_addr, "POST", &format!("/api/read/{id}"), None, Some(TOKEN)).0,
        204
    );
    b.stop().unwrap();
    let b = start(b_setup());
    let i = inbox(b.http_addr);
    assert_eq!((i.len(), i[0]["state"].as_str()), (1, Some("Read")));
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn mail_waits_while_the_receiver_is_off_air() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("a2"), Tmp::new("b2"));
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    send(a.http_addr, json!({"to": "SO5KM-1", "text": "are you there?"}));
    let queued = wait_for(Duration::from_secs(60), "a failed attempt", || {
        let out = get(a.http_addr, "/api/messages?direction=out");
        (out[0]["attempts"].as_u64().unwrap() >= 1).then(|| out[0].clone())
    });
    assert_eq!(queued["state"], "Queued");
    assert!(
        queued["note"].as_str().unwrap().starts_with("NoAnswer"),
        "{queued}"
    );
    // The receiver comes on the air and says so: its beacon wakes the mail.
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &alice,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &b_db,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    });
    delivered(a.http_addr, 1, "delivery after the receiver came up");
    assert_eq!(inbox(b.http_addr)[0]["text"], "are you there?");
    a.stop().unwrap();
    b.stop().unwrap();
}

/// A station without a radio, as on an internet server, and one that dials it.
#[test]
fn internet_only_nodes_exchange_mail_both_ways() {
    let (alice, bob) = keys();
    let (a_db, c_db) = (Tmp::new("a3"), Tmp::new("c3"));
    let server = start(Setup {
        key: &bob,
        me: "SO5KM",
        peer: &alice,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &c_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let server_addr = server.internet_addr.unwrap();
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(call("SO5KM"), server_addr)],
        }),
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    assert!(get(a.http_addr, "/api/status")["radio"].is_null());
    // The server's page listens for changes; it hears of the message at once.
    let mut events = TcpStream::connect(server.http_addr).unwrap();
    write!(
        events,
        "GET /api/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    events.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    send(a.http_addr, json!({"to": "SO5KM", "text": "via the internet"}));
    let sent = delivered(a.http_addr, 1, "internet delivery");
    assert_eq!(
        (sent["verified"].as_bool(), sent["delivered_by"].as_str()),
        (Some(true), Some("internet"))
    );
    assert_eq!(inbox(server.http_addr)[0]["text"], "via the internet");
    let mut seen = String::new();
    let mut buf = [0u8; 1024];
    while !seen.contains("data: message") {
        let n = events.read(&mut buf).expect("an event within 30 s");
        assert!(n > 0, "event stream closed: {seen}");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(seen.contains("text/event-stream"), "{seen}");
    // The server never dialled; it answers over the link SA0KAM opened.
    send(server.http_addr, json!({"to": "SA0KAM", "text": "and back"}));
    delivered(server.http_addr, 1, "delivery back");
    assert_eq!(inbox(a.http_addr)[0]["text"], "and back");
    // One conversation, both ways, newest first; mail with a subject is not chat.
    send(
        a.http_addr,
        json!({"to": "SO5KM", "subject": "Sked", "text": "40 m at 19Z?"}),
    );
    let chat = get(a.http_addr, "/api/messages?direction=all&peer=SO5KM&kind=chat");
    let lines: Vec<(&str, &str)> = chat
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["direction"].as_str().unwrap(), m["text"].as_str().unwrap()))
        .collect();
    assert_eq!(lines, vec![("in", "and back"), ("out", "via the internet")]);
    let mail = get(a.http_addr, "/api/messages?direction=all&kind=mail");
    assert_eq!(mail.as_array().unwrap().len(), 1);
    assert!(get(a.http_addr, "/api/messages?direction=all&peer=SP5AAA")
        .as_array()
        .unwrap()
        .is_empty());
    a.stop().unwrap();
    server.stop().unwrap();
}

/// A radio-only station has never heard of the destination, which is on the
/// internet only. It hands the mail to a relaying gateway it hears (the
/// default route), the gateway carries it over the internet, and the
/// destination's receipt finds its way back through the gateway.
#[test]
fn a_radio_only_station_reaches_an_internet_station_through_a_gateway() {
    let tnc = fake_tnc(0);
    let alice = KeyFile::generate(call("SA0KAM")).unwrap();
    let gateway = KeyFile::generate(call("SM0GW")).unwrap();
    let bob = KeyFile::generate(call("SO5KM-1")).unwrap();
    let (a_db, g_db, b_db) = (Tmp::new("dr-a"), Tmp::new("dr-g"), Tmp::new("dr-b"));
    let g = start_routed(
        Setup {
            key: &gateway,
            me: "SM0GW",
            peer: &alice,
            also: &[&bob],
            tnc: Some(tnc.addr),
            internet: Some(InternetConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                peers: vec![],
            }),
            store: &g_db,
            retry: QUICK,
            beacon_every: Some(Duration::from_secs(2)),
            trust_file: None,
        },
        RelaySettings {
            enabled: true,
            ..RelaySettings::default()
        },
        vec![],
    );
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &gateway,
        also: &[&alice],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(gateway.call, g.internet_addr.unwrap())],
        }),
        store: &b_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &gateway,
        also: &[&bob],
        tnc: Some(tnc.addr),
        internet: None,
        store: &a_db,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    });
    wait_for(Duration::from_secs(30), "the gateway's link and beacons", || {
        let linked = get(g.http_addr, "/api/status")["internet_peers"] == json!(["SO5KM-1"]);
        // The gateway's beacon says it hears us: a way to it.
        let heard = get(a.http_addr, "/api/status")["heard"]
            .as_array()
            .is_some_and(|h| {
                h.iter().any(|s| {
                    s["station"] == "SM0GW"
                        && s["hears"]
                            .as_array()
                            .is_some_and(|l| l.iter().any(|c| c == "SA0KAM"))
                })
            });
        (linked && heard).then_some(())
    });
    send(
        a.http_addr,
        json!({"to": "SO5KM-1", "subject": "Via the gateway", "text": "A-G-B"}),
    );
    let sent = delivered(a.http_addr, 1, "delivery through the gateway");
    assert_eq!(sent["state"], "Delivered");
    assert_eq!(inbox(b.http_addr)[0]["text"], "A-G-B");
    a.stop().unwrap();
    g.stop().unwrap();
    b.stop().unwrap();
}

/// Both stations have radio and internet, and hear each other's beacons.
/// Mail goes by radio, which costs less; when the band dies, the failures and
/// the silence teach the node that the radio link is closed, and mail moves
/// to the internet by itself.
#[test]
fn radio_first_and_the_internet_when_radio_fails() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("a4"), Tmp::new("b4"));
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: Some(tnc.addr),
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &a_db,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    });
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &alice,
        also: &[],
        tnc: Some(tnc.addr),
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(call("SA0KAM"), a.internet_addr.unwrap())],
        }),
        store: &b_db,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    });
    wait_for(Duration::from_secs(20), "the internet link and the radio", || {
        let st = get(a.http_addr, "/api/status");
        (st["internet_peers"] == json!(["SO5KM"]) && st["radio"] == json!(true)).then_some(())
    });
    // Radio goes first once it is known to work: B's beacon says it hears A.
    wait_for(Duration::from_secs(30), "B's beacon, hearing A", || {
        let st = get(a.http_addr, "/api/status");
        st["heard"].as_array().unwrap().iter().find_map(|h| {
            let hears_a = h["hears"]
                .as_array()
                .is_some_and(|l| l.iter().any(|c| c == "SA0KAM"));
            (h["station"] == "SO5KM-1" && hears_a).then_some(())
        })
    });

    send(a.http_addr, json!({"to": "SO5KM-1", "text": "one"}));
    let first = delivered(a.http_addr, 1, "first delivery");
    assert_eq!(first["delivered_by"], "radio");

    tnc.blocked.store(true, Ordering::SeqCst);
    send(a.http_addr, json!({"to": "SO5KM-1", "text": "two"}));
    let second = delivered(a.http_addr, 2, "second delivery");
    assert_eq!(
        (second["delivered_by"].as_str(), second["verified"].as_bool()),
        (Some("internet"), Some(true))
    );
    assert!(
        second["attempts"].as_u64().unwrap() >= 2,
        "radio was tried first: {second}"
    );
    let texts: Vec<String> = inbox(b.http_addr)
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(texts, vec!["two", "one"]);
    let est = get(a.http_addr, "/api/status")["estimates"].clone();
    eprintln!("estimates after the band died: {est}");
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn radio_nodes_beacon_and_list_each_other() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("a-beacon"), Tmp::new("b-beacon"));
    let setup = |key, me, peer, store| Setup {
        key,
        me,
        peer,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    };
    let a = start(setup(&alice, "SA0KAM", &bob, &a_db));
    let b = start(setup(&bob, "SO5KM-1", &alice, &b_db));
    // Each lists the other as trusted, and the other's beacon says it hears us.
    for (node, me, other) in [(&a, "SA0KAM", "SO5KM-1"), (&b, "SO5KM-1", "SA0KAM")] {
        let seen = wait_for(Duration::from_secs(30), "a beacon listing us", || {
            let st = get(node.http_addr, "/api/status");
            st["heard"].as_array().unwrap().iter().find_map(|h| {
                let hears_us = h["hears"].as_array().is_some_and(|l| l.iter().any(|c| c == me));
                (h["station"] == other && hears_us).then(|| h.clone())
            })
        });
        assert_eq!(seen["key"], "trusted", "{seen}");
        assert_eq!(seen["offers"], serde_json::json!([]), "{seen}");
        assert!(seen["clock_offset"].as_i64().unwrap().abs() <= 5, "{seen}");
        // Both give JO89xi: the same square, no distance.
        assert_eq!(
            (seen["locator"].as_str(), seen["distance_km"].as_f64()),
            (Some("JO89xi"), Some(0.0)),
            "{seen}"
        );
    }
    a.stop().unwrap();
    b.stop().unwrap();
}

/// Two stations of one operator, SA0KAM-1 and SA0KAM-2, each with its own key:
/// mail to one reaches that one only, with a receipt from its own key.
#[test]
fn two_ssids_are_two_stations_with_their_own_keys() {
    let tnc = fake_tnc(0);
    let sender = KeyFile::generate(call("SO5KM")).unwrap();
    let home = KeyFile::generate(call("SA0KAM-1")).unwrap();
    let server = KeyFile::generate(call("SA0KAM-2")).unwrap();
    let (s_db, h_db, v_db) = (Tmp::new("ssid-s"), Tmp::new("ssid-1"), Tmp::new("ssid-2"));
    let s = start(Setup {
        key: &sender,
        me: "SO5KM",
        peer: &home,
        also: &[&server],
        tnc: Some(tnc.addr),
        internet: None,
        store: &s_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let station = |key, me, store| {
        start(Setup {
            key,
            me,
            peer: &sender,
            also: &[],
            tnc: Some(tnc.addr),
            internet: None,
            store,
            retry: QUICK,
            beacon_every: None,
            trust_file: None,
        })
    };
    let h = station(&home, "SA0KAM-1", &h_db);
    let v = station(&server, "SA0KAM-2", &v_db);

    send(s.http_addr, json!({"to": "SA0KAM-2", "text": "for the server"}));
    let got = wait_for(Duration::from_secs(60), "the message at SA0KAM-2", || {
        let i = inbox(v.http_addr);
        (i.len() == 1).then(|| i[0].clone())
    });
    assert_eq!(
        (got["text"].as_str(), got["verified"].as_bool()),
        (Some("for the server"), Some(true))
    );
    let sent = delivered(s.http_addr, 1, "delivery to SA0KAM-2");
    assert_eq!(
        sent["verified"].as_bool(),
        Some(true),
        "receipt from SA0KAM-2's own key"
    );

    send(s.http_addr, json!({"to": "SA0KAM-1", "text": "for home"}));
    let got = wait_for(Duration::from_secs(60), "the message at SA0KAM-1", || {
        let i = inbox(h.http_addr);
        (i.len() == 1).then(|| i[0].clone())
    });
    assert_eq!(got["text"].as_str(), Some("for home"));
    delivered(s.http_addr, 2, "delivery to SA0KAM-1");
    // Each station holds only its own mail.
    assert_eq!(inbox(v.http_addr).len(), 1);
    assert_eq!(inbox(h.http_addr).len(), 1);
    for n in [s, h, v] {
        n.stop().unwrap();
    }
}

/// Trust and settings change without a restart: through the API (saved to
/// the settings file) and by editing that file by hand.
#[test]
fn trusted_stations_change_while_the_node_runs() {
    let (alice, bob) = keys();
    let (a_db, h_db) = (Tmp::new("trust-a"), Tmp::new("trust-h"));
    let trust_file = std::env::temp_dir().join(format!("hm-node-trust-{}.toml", std::process::id()));
    std::fs::write(&trust_file, "# stations this hub trusts\n").unwrap();
    let hub = start(Setup {
        key: &bob,
        me: "SO5KM",
        peer: &alice,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &h_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: Some(trust_file.clone()),
    });
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(call("SO5KM"), hub.internet_addr.unwrap())],
        }),
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let linked = |n: &NodeHandle| {
        get(n.http_addr, "/api/status")["internet_peers"]
            .as_array()
            .is_some_and(|p| !p.is_empty())
    };
    // The hub trusts nobody yet: no link, the mail waits.
    send(a.http_addr, json!({"to": "SO5KM", "text": "first"}));
    thread::sleep(Duration::from_secs(3));
    assert!(!linked(&a) && inbox(hub.http_addr).is_empty());

    // Trusted through the API: saved, and the link comes up without a restart.
    let (status, body) = http(
        hub.http_addr,
        "POST",
        "/api/trust",
        Some(&json!({"line": alice.trust_line()})),
        Some(TOKEN),
    );
    assert_eq!(status, 201, "{body}");
    let list = get(hub.http_addr, "/api/trust");
    assert_eq!(list["stations"][0]["station"], "SA0KAM");
    let saved = std::fs::read_to_string(&trust_file).unwrap();
    assert!(saved.starts_with("# stations this hub trusts\n"), "{saved}");
    assert_eq!(Config::load(&trust_file).unwrap().trust[0].station, "SA0KAM");

    // Settings change through the API too: in use at once, and saved.
    let (status, body) = http(
        hub.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({
            "internet_cost": 0.5,
            "retry_attempts": 7,
            "locator": "ko02md",
            "custody_grace_secs": 3600,
            "custody_suspect_secs": 7200,
            "receipt_retry_attempts": 9,
            "relay": { "enabled": true, "mailbox": true, "max_hops": 4 },
            "radio": { "max_rounds": 5 },
        })),
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let now = get(hub.http_addr, "/api/settings");
    assert_eq!(now["live"]["locator"], "KO02md");
    assert_eq!(get(hub.http_addr, "/api/status")["locator"], "KO02md");
    assert_eq!(
        (
            now["live"]["internet_cost"].as_f64(),
            now["live"]["retry_attempts"].as_u64(),
            now["live"]["custody_grace_secs"].as_u64(),
            now["live"]["custody_suspect_secs"].as_u64(),
            now["live"]["receipt_retry_attempts"].as_u64(),
            now["live"]["relay"]["enabled"].as_bool(),
            now["live"]["relay"]["mailbox"].as_bool(),
            now["live"]["relay"]["max_hops"].as_u64(),
            now["live"]["radio"]["max_rounds"].as_u64(),
        ),
        (
            Some(0.5),
            Some(7),
            Some(3600),
            Some(7200),
            Some(9),
            Some(true),
            Some(true),
            Some(4),
            Some(5)
        )
    );
    let c = Config::load(&trust_file).unwrap();
    assert_eq!((c.delivery.internet_cost, c.delivery.retry_attempts), (0.5, 7));
    assert_eq!(
        (
            c.delivery.custody_grace_secs,
            c.delivery.custody_suspect_secs,
            c.delivery.receipt_retry_attempts,
            c.relay.enabled,
            c.relay.mailbox,
            c.relay.max_hops,
            c.radio.max_rounds,
        ),
        (3600, 7200, 9, true, true, 4, 5)
    );
    assert_eq!(c.station.locator.as_deref(), Some("KO02md"));
    let (status, body) = http(
        hub.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({
            "restart_to_change": {
                "internet_listen": "127.0.0.1:9443",
                "open_hub": true,
                "modem": { "enabled": true, "kind": "ardop", "host": "127.0.0.1", "port": 8515, "bandwidth": 0, "ptt": "none" },
            }
        })),
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let fixed = get(hub.http_addr, "/api/settings")["restart_to_change"].clone();
    assert_eq!(fixed["internet_listen"], "127.0.0.1:9443");
    assert_eq!(fixed["open_hub"], true);
    assert_eq!(fixed["modem"]["enabled"], true);
    assert_eq!(fixed["modem"]["kind"], "ardop");
    assert_eq!(fixed["modem"]["port"], 8515);
    let saved = Config::load(&trust_file).unwrap();
    assert_eq!(saved.internet.listen.as_deref(), Some("127.0.0.1:9443"));
    assert!(saved.internet.open_hub);
    assert!(saved.modem.enabled);
    assert_eq!(saved.modem.kind, "ardop");
    assert_eq!(saved.modem.port, 8515);
    let (status, _) = http(
        hub.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({"radio_cost": -1})),
        Some(TOKEN),
    );
    assert_eq!(status, 400);
    delivered(a.http_addr, 1, "delivery once trusted");
    assert_eq!(inbox(hub.http_addr)[0]["verified"], true);

    // Removed through the API: the link goes at once, and further mail waits.
    let (status, _) = http(hub.http_addr, "DELETE", "/api/trust/SA0KAM", None, Some(TOKEN));
    assert_eq!(status, 204);
    assert_eq!(
        http(hub.http_addr, "DELETE", "/api/trust/SA0KAM", None, Some(TOKEN)).0,
        404
    );
    wait_for(Duration::from_secs(10), "the link to drop", || {
        (!linked(&hub)).then_some(())
    });
    send(a.http_addr, json!({"to": "SO5KM", "text": "second"}));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(inbox(hub.http_addr).len(), 1);

    // Trusted again by editing the file by hand.
    let mut text = std::fs::read_to_string(&trust_file).unwrap();
    let (c, k) = alice
        .trust_line()
        .split_once(' ')
        .map(|(c, k)| (c.to_string(), k.to_string()))
        .unwrap();
    text.push_str(&format!(
        "\n[[trust]]\nstation = \"{c}\"\nkey = \"{k}\"\nnote = \"added by hand\"\n"
    ));
    std::fs::write(&trust_file, text).unwrap();
    wait_for(Duration::from_secs(30), "the second message", || {
        (inbox(hub.http_addr).len() == 2).then_some(())
    });
    // Bad requests are refused.
    let (status, _) = http(
        hub.http_addr,
        "POST",
        "/api/trust",
        Some(&json!({"line": "SA0KAM nothex"})),
        Some(TOKEN),
    );
    assert_eq!(status, 400);
    assert_eq!(http(hub.http_addr, "GET", "/api/trust", None, None).0, 401);
    a.stop().unwrap();
    hub.stop().unwrap();
    std::fs::remove_file(&trust_file).unwrap();
}

/// A radio-only node can gain an internet peer without a restart: the dial
/// stack starts when the first peer is added.
#[test]
fn internet_peer_added_while_radio_only_node_runs() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, h_db) = (Tmp::new("dial-later-a"), Tmp::new("dial-later-h"));
    let settings = std::env::temp_dir().join(format!("hm-node-dial-later-{}.toml", std::process::id()));
    std::fs::write(
        &settings,
        format!("[[trust]]\nstation = {:?}\nkey = {:?}\n", bob.call.to_string(), {
            let line = bob.trust_line();
            line.split_once(' ').unwrap().1.to_string()
        }),
    )
    .unwrap();
    let hub = start(Setup {
        key: &bob,
        me: "SO5KM",
        peer: &alice,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &h_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: Some(settings.clone()),
    });
    assert!(get(a.http_addr, "/api/status")["internet_listen"].is_null());
    let hub_addr = hub.internet_addr.unwrap();
    let (status, body) = http(
        a.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({
            "peers": [{"station": "SO5KM", "address": hub_addr.to_string()}]
        })),
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    wait_for(Duration::from_secs(15), "internet dial after peer add", || {
        let st = get(a.http_addr, "/api/status");
        (st["internet_peers"] == json!(["SO5KM"]) && !st["internet_listen"].is_null()).then_some(())
    });
    send(a.http_addr, json!({"to": "SO5KM", "text": "dialled after start"}));
    delivered(a.http_addr, 1, "internet delivery without restart");
    assert_eq!(inbox(hub.http_addr)[0]["text"], "dialled after start");
    a.stop().unwrap();
    hub.stop().unwrap();
    let _ = std::fs::remove_file(&settings);
}

/// Radio settings change without a restart: the node opens the link the new
/// settings describe, and mail waiting for the radio goes out on it.
#[test]
fn radio_settings_change_while_the_node_runs() {
    let (here, there) = (fake_tnc(0), fake_tnc(0));
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("radio-a"), Tmp::new("radio-b"));
    let setup = |key, me, peer, tnc, store| Setup {
        key,
        me,
        peer,
        also: &[],
        tnc: Some(tnc),
        internet: None,
        store,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    };
    // Bob listens on the other channel; nothing Alice sends reaches him.
    let b = start(setup(&bob, "SO5KM-1", &alice, there.addr, &b_db));
    let a = start(setup(&alice, "SA0KAM", &bob, here.addr, &a_db));
    let up_on = |n: &NodeHandle, addr: SocketAddr| {
        let st = get(n.http_addr, "/api/status");
        (st["radio"] == true
            && st["radio_via"]
                .as_str()
                .is_some_and(|v| v.contains(&addr.to_string())))
        .then_some(())
    };
    wait_for(Duration::from_secs(10), "the radio up", || up_on(&a, here.addr));
    send(
        a.http_addr,
        json!({"to": "SO5KM-1", "text": "over the other channel"}),
    );
    thread::sleep(Duration::from_secs(2));
    assert!(inbox(b.http_addr).is_empty());

    let (status, body) = http(
        a.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({"radio": {"kiss": there.addr.to_string()}})),
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let now: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(now["live"]["radio"]["kiss"], there.addr.to_string());
    assert_eq!(now["radio_applies_now"], true);
    wait_for(Duration::from_secs(10), "the radio on the new TNC", || {
        up_on(&a, there.addr)
    });
    delivered(a.http_addr, 1, "delivery on the new channel");
    assert_eq!(inbox(b.http_addr)[0]["text"], "over the other channel");

    // Settings that cannot work are refused and change nothing.
    let (status, _) = http(
        a.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({"radio": {"kiss": "serial:"}})),
        Some(TOKEN),
    );
    assert_eq!(status, 400);
    // Switched off: the status says no radio.
    let (status, _) = http(
        a.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({"radio": {"enabled": false}})),
        Some(TOKEN),
    );
    assert_eq!(status, 200);
    wait_for(Duration::from_secs(10), "the radio off", || {
        get(a.http_addr, "/api/status")["radio"].is_null().then_some(())
    });
    a.stop().unwrap();
    b.stop().unwrap();
}
