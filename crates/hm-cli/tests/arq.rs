//! Nodes reaching each other through an ARQ modem (VARA, Mercury, ARDOP),
//! played here by a fake modem program: it speaks the host interface on a
//! command port and a data port, and connects to other fake modems through a
//! shared "air" when asked to call.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hm_cli::config::RadioSettings;
use hm_cli::files::{KeyFile, Trust};
use hm_cli::node::arq::{ArqConfig, Kind};
use hm_cli::node::choose::Costs;
use hm_cli::node::live::Live;
use hm_cli::node::{self, NodeConfig, NodeHandle};
use hm_cli::sound_link::PttFactory;
use hm_rig::ptt::Ptt;
use hm_store::RetryPolicy;
use hm_wire::Callsign;
use serde_json::{json, Value};

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

// ---- the fake modem program ---------------------------------------------------

#[derive(Default)]
struct Station {
    call: String,
    listening: bool,
    cmd: Option<TcpStream>,
    data: Option<TcpStream>,
    peer: Option<usize>,
}

/// Every fake modem, by index; calls go between them.
#[derive(Default)]
struct Air {
    stations: Mutex<Vec<Station>>,
}

impl Air {
    fn say(&self, i: usize, line: &str) {
        let mut st = self.stations.lock().unwrap();
        if let Some(c) = st[i].cmd.as_mut() {
            let _ = c.write_all(format!("{line}\r").as_bytes());
        }
    }
}

/// A fake modem on two consecutive ports; returns the command port.
fn fake_modem(air: &Arc<Air>, kind: Kind) -> u16 {
    let (cmd, data) = loop {
        let cmd = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = cmd.local_addr().unwrap().port();
        if let Ok(data) = TcpListener::bind(("127.0.0.1", port + 1)) {
            break (cmd, data);
        }
    };
    let port = cmd.local_addr().unwrap().port();
    let me = {
        let mut st = air.stations.lock().unwrap();
        st.push(Station::default());
        st.len() - 1
    };
    let air = air.clone();
    thread::spawn(move || {
        // The node connects again after a failure; serve each pair in turn.
        loop {
            let (c, _) = cmd.accept().unwrap();
            let (d, _) = data.accept().unwrap();
            {
                let mut st = air.stations.lock().unwrap();
                st[me].cmd = Some(c.try_clone().unwrap());
                st[me].data = Some(d.try_clone().unwrap());
            }
            let air2 = air.clone();
            let data_thread = thread::spawn(move || relay_data(&air2, me, d, kind));
            serve_commands(&air, me, c, kind);
            let _ = data_thread.join();
        }
    });
    port
}

fn connected_line(kind: Kind, caller: &str, called: &str, remote: &str) -> String {
    match kind {
        Kind::Vara => format!("CONNECTED {caller} {called} 2300"),
        Kind::Ardop => format!("CONNECTED {remote} 2000"),
    }
}

fn serve_commands(air: &Arc<Air>, me: usize, c: TcpStream, kind: Kind) {
    let mut r = BufReader::new(c);
    let mut buf = Vec::new();
    while r.read_until(b'\r', &mut buf).unwrap_or(0) > 0 {
        let line = String::from_utf8_lossy(&buf).trim().to_string();
        buf.clear();
        let words: Vec<&str> = line.split_whitespace().collect();
        let target = match (kind, words.as_slice()) {
            (_, ["MYCALL", call, ..]) => {
                air.stations.lock().unwrap()[me].call = call.to_string();
                None
            }
            (_, ["LISTEN", "ON" | "TRUE"]) => {
                air.stations.lock().unwrap()[me].listening = true;
                None
            }
            (Kind::Vara, ["CONNECT", _, to]) | (Kind::Ardop, ["ARQCALL", to, _]) => Some(to.to_string()),
            (_, ["DISCONNECT"]) => {
                let peer = air.stations.lock().unwrap()[me].peer.take();
                if let Some(p) = peer {
                    air.stations.lock().unwrap()[p].peer = None;
                    air.say(p, "DISCONNECTED");
                }
                air.say(me, "DISCONNECTED");
                None
            }
            _ => None,
        };
        if target.is_some() && air.stations.lock().unwrap()[me].peer.is_some() {
            air.say(me, "WRONG"); // already connected
            continue;
        }
        air.say(me, "OK");
        let Some(to) = target else { continue };
        // Key up for the call, as a modem driving the host's PTT would.
        let (on, off) = match kind {
            Kind::Vara => ("PTT ON", "PTT OFF"),
            Kind::Ardop => ("PTT TRUE", "PTT FALSE"),
        };
        air.say(me, on);
        thread::sleep(Duration::from_millis(100));
        air.say(me, off);
        let found = {
            let st = air.stations.lock().unwrap();
            st.iter()
                .position(|s| s.call == to && s.listening && s.peer.is_none())
        };
        match found {
            Some(other) if other != me => {
                let mine = {
                    let mut st = air.stations.lock().unwrap();
                    st[me].peer = Some(other);
                    st[other].peer = Some(me);
                    st[me].call.clone()
                };
                air.say(me, &connected_line(kind, &mine, &to, &to));
                air.say(other, &connected_line(kind, &mine, &to, &mine));
            }
            _ => {
                thread::sleep(Duration::from_millis(500));
                air.say(me, "DISCONNECTED");
            }
        }
    }
}

/// Data from this modem's host goes to the connected peer's host.
fn relay_data(air: &Arc<Air>, me: usize, mut d: TcpStream, kind: Kind) {
    let mut buf = [0u8; 4096];
    let mut pending = Vec::new();
    loop {
        let n = match d.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        let payload: Vec<u8> = match kind {
            Kind::Vara => std::mem::take(&mut pending),
            Kind::Ardop => {
                // Host to modem: length (2 bytes) + data.
                let mut out = Vec::new();
                while pending.len() >= 2 {
                    let len = u16::from_be_bytes([pending[0], pending[1]]) as usize;
                    if pending.len() < 2 + len {
                        break;
                    }
                    out.extend_from_slice(&pending[2..2 + len]);
                    pending.drain(..2 + len);
                }
                out
            }
        };
        if payload.is_empty() {
            continue;
        }
        let mut st = air.stations.lock().unwrap();
        let Some(peer) = st[me].peer else { continue };
        if let Some(pd) = st[peer].data.as_mut() {
            let framed = match kind {
                Kind::Vara => payload,
                // Modem to host: length (tag included) + "ARQ" + data, in small frames.
                Kind::Ardop => payload
                    .chunks(200)
                    .flat_map(|c| {
                        let mut f = ((c.len() + 3) as u16).to_be_bytes().to_vec();
                        f.extend_from_slice(b"ARQ");
                        f.extend_from_slice(c);
                        f
                    })
                    .collect(),
            };
            let _ = pd.write_all(&framed);
        }
    }
}

// ---- nodes on the modem -------------------------------------------------------

/// Records keying.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<bool>>>);

impl Ptt for Recorder {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.0.lock().unwrap().push(on);
        Ok(())
    }
    fn describe(&self) -> String {
        "recorder".into()
    }
}

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn start(
    key: &KeyFile,
    me: &str,
    peer: &KeyFile,
    port: u16,
    kind: Kind,
    db: &Tmp,
    ptt: Option<PttFactory>,
) -> NodeHandle {
    let mut trust = Trust::default();
    trust.insert(peer.call, peer.identity.public());
    node::start(NodeConfig {
        key: KeyFile::parse(&key.to_text()).unwrap(),
        live: Live {
            trust,
            notes: vec![],
            costs: Costs::default(),
            retry: RetryPolicy {
                first_delay_secs: 2,
                max_delay_secs: 5,
                max_attempts: 20,
            },
            beacon_secs: 0,
            peers: vec![],
            locator: None,
            radio: RadioSettings {
                enabled: false,
                ..RadioSettings::default()
            },
        },
        config_file: None,
        overrides: None,
        overridden: vec![],
        me: call(me),
        radio: None,
        radio_builder: None,
        internet: None,
        modem: Some(ArqConfig {
            kind,
            host: "127.0.0.1".into(),
            port,
            bandwidth: 2300,
            ptt,
        }),
        store: db.0.clone(),
        http: "127.0.0.1:0".parse().unwrap(),
        token: "t".into(),
        seed: Some(5),
    })
    .unwrap()
}

fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&Value>) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut r = String::new();
    s.read_to_string(&mut r).unwrap();
    (
        r[9..12].parse().unwrap(),
        r.split_once("\r\n\r\n")
            .map(|x| x.1.to_string())
            .unwrap_or_default(),
    )
}

fn get(addr: SocketAddr, path: &str) -> Value {
    serde_json::from_str(&http(addr, "GET", path, None).1).unwrap()
}

fn wait_for<T>(limit: Duration, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn exchange_through(kind: Kind, tag: &str) {
    let air = Arc::new(Air::default());
    let (pa, pb) = (fake_modem(&air, kind), fake_modem(&air, kind));
    let alice = KeyFile::generate(call("SA0KAM")).unwrap();
    let bob = KeyFile::generate(call("SO5KM-1")).unwrap();
    let dir = std::env::temp_dir();
    let (da, db) = (
        Tmp(dir.join(format!("hm-arq-{tag}-a-{}.db", std::process::id()))),
        Tmp(dir.join(format!("hm-arq-{tag}-b-{}.db", std::process::id()))),
    );
    let keyed = Recorder::default();
    let k = keyed.clone();
    let ptt: PttFactory = Arc::new(move || Ok(Box::new(k.clone()) as Box<dyn Ptt>));
    let a = start(&alice, "SA0KAM", &bob, pa, kind, &da, Some(ptt));
    let b = start(&bob, "SO5KM-1", &alice, pb, kind, &db, None);
    wait_for(Duration::from_secs(10), "the modems up", || {
        (get(a.http_addr, "/api/status")["modem"] == true && get(b.http_addr, "/api/status")["modem"] == true)
            .then_some(())
    });

    for (from, to, text) in [(&a, "SO5KM-1", "over the modem"), (&b, "SA0KAM", "and back")] {
        let (st, body) = http(
            from.http_addr,
            "POST",
            "/api/send",
            Some(&json!({"to": to, "text": text})),
        );
        assert_eq!(st, 201, "{body}");
    }
    for (node, text) in [(&b, "over the modem"), (&a, "and back")] {
        let got = wait_for(Duration::from_secs(60), text, || {
            let inbox = get(node.http_addr, "/api/messages?direction=in");
            inbox
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["text"] == text)
                .cloned()
        });
        assert_eq!(got["verified"], true);
    }
    for node in [&a, &b] {
        let sent = wait_for(Duration::from_secs(60), "the receipt", || {
            let out = get(node.http_addr, "/api/messages?direction=out");
            out.as_array()
                .unwrap()
                .iter()
                .find(|m| m["state"] == "Delivered")
                .cloned()
        });
        assert_eq!(
            (sent["delivered_by"].as_str(), sent["verified"].as_bool()),
            (Some("modem"), Some(true))
        );
    }
    // A keyed the radio when its modem asked, and let go again.
    let k = keyed.0.lock().unwrap().clone();
    assert!(k.contains(&true) && k.last() == Some(&false), "{k:?}");
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn mail_goes_through_vara() {
    exchange_through(Kind::Vara, "vara");
}

#[test]
fn mail_goes_through_ardop() {
    exchange_through(Kind::Ardop, "ardop");
}

#[test]
fn a_call_nobody_answers_fails_and_is_retried() {
    let air = Arc::new(Air::default());
    let port = fake_modem(&air, Kind::Vara);
    let alice = KeyFile::generate(call("SA0KAM")).unwrap();
    let bob = KeyFile::generate(call("SO5KM")).unwrap();
    let da = Tmp(std::env::temp_dir().join(format!("hm-arq-none-{}.db", std::process::id())));
    let a = start(&alice, "SA0KAM", &bob, port, Kind::Vara, &da, None);
    let (st, _) = http(
        a.http_addr,
        "POST",
        "/api/send",
        Some(&json!({"to": "SO5KM", "text": "anyone?"})),
    );
    assert_eq!(st, 201);
    let m = wait_for(Duration::from_secs(30), "a failed attempt", || {
        let out = get(a.http_addr, "/api/messages?direction=out");
        let m = out[0].clone();
        (m["attempts"].as_u64() >= Some(1)).then_some(m)
    });
    assert_eq!(m["state"], "Queued", "{m}");
    assert!(m["note"].as_str().unwrap_or("").contains("modem"), "{m}");
    a.stop().unwrap();
}
