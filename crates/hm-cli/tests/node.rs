//! Station nodes on a fake radio channel and/or localhost internet links,
//! driven only through the HTTP API.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use hm_cli::files::{KeyFile, Trust};
use hm_cli::kiss_link::{KissTarget, TncParams};
use hm_cli::node::choose::Costs;
use hm_cli::node::{self, InternetConfig, NodeConfig, NodeHandle, RadioConfig, RadioLink};
use hm_cli::station::LinkTiming;
use hm_store::RetryPolicy;
use hm_wire::Callsign;
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

/// Minimal HTTP/1.1 client: returns status and body.
fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> (u16, String) {
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
    /// Trust exactly what this file lists (instead of `peer` and `also`), and
    /// keep it as the node's trust file.
    trust_file: Option<PathBuf>,
}

fn start(s: Setup) -> NodeHandle {
    let mut trust = Trust::default();
    trust.insert(s.peer.call, s.peer.identity.public());
    for k in s.also {
        trust.insert(k.call, k.identity.public());
    }
    if let Some(f) = &s.trust_file {
        trust = Trust::load(f).unwrap();
    }
    node::start(NodeConfig {
        key: KeyFile::parse(&s.key.to_text()).unwrap(),
        trust,
        trust_file: s.trust_file,
        me: call(s.me),
        radio: s.tnc.map(|t| RadioConfig {
            link: RadioLink::Kiss {
                target: KissTarget::Tcp(t.to_string()),
                tnc_port: 0,
                params: TncParams::default(),
            },
            timing: FAST,
            beacon_every: s.beacon_every,
        }),
        internet: s.internet,
        costs: Costs::default(),
        store: s.store.0.clone(),
        http: "127.0.0.1:0".parse().unwrap(),
        retry: s.retry,
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

fn delivered(addr: SocketAddr, n: usize, what: &str) -> Value {
    wait_for(Duration::from_secs(120), what, || {
        let out = get(addr, "/api/messages?direction=out");
        let done: Vec<&Value> = out
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["state"] == "Delivered")
            .collect();
        (done.len() >= n).then(|| out[0].clone())
    })
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
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &alice,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &b_db,
        retry: QUICK,
        beacon_every: None,
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
    send(a.http_addr, json!({"to": "SO5KM", "text": "via the internet"}));
    let sent = delivered(a.http_addr, 1, "internet delivery");
    assert_eq!(
        (sent["verified"].as_bool(), sent["delivered_by"].as_str()),
        (Some(true), Some("internet"))
    );
    assert_eq!(inbox(server.http_addr)[0]["text"], "via the internet");
    // The server never dialled; it answers over the link SA0KAM opened.
    send(server.http_addr, json!({"to": "SA0KAM", "text": "and back"}));
    delivered(server.http_addr, 1, "delivery back");
    assert_eq!(inbox(a.http_addr)[0]["text"], "and back");
    a.stop().unwrap();
    server.stop().unwrap();
}

/// Both stations have radio and internet. Mail goes by radio; when the band
/// dies it moves to the internet by itself.
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
        beacon_every: None,
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
        beacon_every: None,
        trust_file: None,
    });
    wait_for(Duration::from_secs(20), "the internet link and the radio", || {
        let st = get(a.http_addr, "/api/status");
        (st["internet_peers"] == json!(["SO5KM"]) && st["radio"] == json!(true)).then_some(())
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

/// Trust changes without a restart: through the API (saved to the trust
/// file) and by editing the trust file by hand.
#[test]
fn trusted_stations_change_while_the_node_runs() {
    let (alice, bob) = keys();
    let (a_db, h_db) = (Tmp::new("trust-a"), Tmp::new("trust-h"));
    let trust_file = std::env::temp_dir().join(format!("hm-node-trust-{}.txt", std::process::id()));
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
    assert!(std::fs::read_to_string(&trust_file)
        .unwrap()
        .starts_with("# stations this hub trusts\nSA0KAM "));
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
    text.push_str(&format!("{}\n", alice.trust_line()));
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
