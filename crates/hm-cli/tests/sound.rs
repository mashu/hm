//! The built-in modem as a radio link, on the virtual ether (real-time audio
//! between stations, no hardware): channel access, PTT, and whole nodes.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hm_cli::config::RadioSettings;
use hm_cli::driver::Link;
use hm_cli::files::{KeyFile, Trust};
use hm_cli::node::live::Costs;
use hm_cli::node::live::Live;
use hm_cli::node::{self, NodeConfig, RadioConfig, RadioLink};
use hm_cli::sound_link::{AudioFactory, Csma, Framing, PttFactory, SoundLink};
use hm_cli::station::LinkTiming;
use hm_rig::ether::Ether;
use hm_rig::ptt::Ptt;
use hm_rig::AudioPort;
use hm_store::RetryPolicy;
use hm_wire::Callsign;

const FS: u32 = 12_000;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn audio(ether: &Ether) -> AudioFactory {
    let e = ether.clone();
    Arc::new(move || Ok(Box::new(e.port()) as Box<dyn AudioPort>))
}

/// Records every key and unkey with its time.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<(Instant, bool)>>>);

impl Ptt for Recorder {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.0.lock().unwrap().push((Instant::now(), on));
        Ok(())
    }
    fn describe(&self) -> String {
        "recorder".into()
    }
}

fn ptt(r: &Recorder) -> PttFactory {
    let r = r.clone();
    Arc::new(move || Ok(Box::new(r.clone()) as Box<dyn Ptt>))
}

fn link(ether: &Ether, me: &str, rec: &Recorder) -> SoundLink {
    SoundLink::start(call(me), audio(ether), ptt(rec), Csma::default()).unwrap()
}

fn collect(l: &mut SoundLink, until: Duration) -> Vec<Vec<u8>> {
    let end = Instant::now() + until;
    let mut got = Vec::new();
    while Instant::now() < end {
        if let Some(f) = l.recv_timeout(Duration::from_millis(50)).unwrap() {
            got.push(f);
        }
    }
    got
}

#[test]
fn carrier_sense_waits_for_a_busy_channel() {
    let ether = Ether::new(FS, 0.02, 1);
    let (ra, rb, rc) = (Recorder::default(), Recorder::default(), Recorder::default());
    let mut a = link(&ether, "SA0KAM", &ra);
    let mut b = link(&ether, "SO5KM", &rb);
    let mut c = link(&ether, "SP5AAA", &rc);
    let long: Vec<u8> = (0..500u32).map(|i| i as u8).collect(); // about 4 s on air
    a.send(&long).unwrap();
    // A keys up on its own schedule (p-persistent: 25% per 100 ms slot, so a
    // fixed sleep can end before A is on the air). Wait until it is, then some.
    let keyed = |r: &Recorder| r.0.lock().unwrap().iter().any(|(_, on)| *on);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !keyed(&ra) {
        assert!(Instant::now() < deadline, "A never keyed up");
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(500)); // A is mid-transmission
    assert!(!keyed(&rb), "B has nothing to send yet");
    b.send(b"short frame from B, sent while A is on the air").unwrap();
    let got = collect(&mut c, Duration::from_secs(10));
    assert!(got.contains(&long), "C decoded A's frame");
    assert!(
        got.iter().any(|f| f.starts_with(b"short frame from B")),
        "C decoded B's frame"
    );
    // B keyed only after A had unkeyed.
    let a_off = ra.0.lock().unwrap().iter().rev().find(|(_, on)| !on).unwrap().0;
    let b_on = rb.0.lock().unwrap().iter().find(|(_, on)| *on).unwrap().0;
    assert!(b_on >= a_off, "B waited for the channel");
    drop((a, b));
}

#[test]
fn ptt_is_keyed_only_around_transmissions_and_always_released() {
    let ether = Ether::new(FS, 0.0, 2);
    let rec = Recorder::default();
    let mut a = link(&ether, "SA0KAM", &rec);
    let mut b = link(&ether, "SO5KM", &Recorder::default());
    for i in 0..3u8 {
        a.send(&[i; 40]).unwrap();
        thread::sleep(Duration::from_millis(900));
    }
    let got = collect(&mut b, Duration::from_secs(2));
    assert_eq!(got.len(), 3);
    drop(a);
    let events: Vec<bool> = rec.0.lock().unwrap().iter().map(|(_, on)| *on).collect();
    // Starts unkeyed, then alternates key/unkey, and ends unkeyed.
    assert!(!events[0], "starts unkeyed");
    let keyed: Vec<bool> = events[1..].to_vec();
    assert!(keyed.chunks(2).all(|p| p == [true, false]), "{events:?}");
    // CSMA may hold a frame until the next arrives and send both in one key-up.
    let keyups = keyed.iter().filter(|x| **x).count();
    assert!((1..=3).contains(&keyups), "{keyups} key-ups for 3 frames");
}

#[test]
fn a_burst_goes_out_in_one_key_up() {
    let ether = Ether::new(FS, 0.0, 3);
    let rec = Recorder::default();
    let mut a = link(&ether, "SA0KAM", &rec);
    let mut b = link(&ether, "SO5KM", &Recorder::default());
    for i in 0..5u8 {
        a.send(&[i; 60]).unwrap();
    }
    assert_eq!(collect(&mut b, Duration::from_secs(5)).len(), 5);
    assert_eq!(rec.0.lock().unwrap().iter().filter(|(_, on)| *on).count(), 1);
}

// ---- whole nodes on the built-in modem ------------------------------------

fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&serde_json::Value>) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
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

fn get(addr: SocketAddr, path: &str) -> serde_json::Value {
    serde_json::from_str(&http(addr, "GET", path, None).1).unwrap()
}

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn start(ether: &Ether, key: &KeyFile, me: &str, peer: &KeyFile, db: &Tmp) -> node::NodeHandle {
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
            receipt_retry: RetryPolicy {
                first_delay_secs: 2,
                max_delay_secs: 5,
                max_attempts: 24,
            },
            custody_grace_secs: 6 * 3600,
            custody_suspect_secs: 24 * 3600,
            relay: Default::default(),
            beacon_secs: 0,
            peers: vec![],
            locator: None,
            radio: RadioSettings::default(),
        },
        config_file: None,
        overrides: None,
        overridden: vec![],
        me: call(me),
        radio: Some(RadioConfig {
            link: RadioLink::Modem {
                audio: audio(ether),
                ptt: Arc::new(|| Ok(Box::new(hm_rig::ptt::Vox) as Box<dyn Ptt>)),
                csma: Csma::default(),
                describe: "virtual ether".into(),
            },
            timing: LinkTiming {
                bitrate_bps: 1200,
                txdelay_ms: 300,
                guard_ms: 1500,
                max_rounds: 12,
                max_keyup_ms: 20_000,
            },
        }),
        radio_builder: None,
        internet: None,
        modem: None,
        schedules: vec![],
        store: db.0.clone(),
        http: "127.0.0.1:0".parse().unwrap(),
        token: "t".into(),
        seed: Some(3),
    })
    .unwrap()
}

#[test]
fn two_nodes_exchange_mail_over_the_built_in_modem() {
    let ether = Ether::new(FS, 0.02, 4);
    let (alice, bob) = (
        KeyFile::generate(call("SA0KAM")).unwrap(),
        KeyFile::generate(call("SO5KM")).unwrap(),
    );
    let tmp = |n: &str| Tmp(std::env::temp_dir().join(format!("hm-sound-{n}-{}.db", std::process::id())));
    let (a_db, b_db) = (tmp("a"), tmp("b"));
    let b = start(&ether, &bob, "SO5KM-1", &alice, &b_db);
    let a = start(&ether, &alice, "SA0KAM", &bob, &a_db);
    let msg = serde_json::json!({"to": "SO5KM-1", "subject": "Modem", "text": "73 via the built-in modem"});
    assert_eq!(http(a.http_addr, "POST", "/api/send", Some(&msg)).0, 201);
    let start = Instant::now();
    let sent = loop {
        let out = get(a.http_addr, "/api/messages?direction=out");
        if out[0]["state"] == "Delivered" {
            break out[0].clone();
        }
        assert!(start.elapsed() < Duration::from_secs(90), "not delivered: {out}");
        thread::sleep(Duration::from_millis(200));
    };
    eprintln!(
        "delivered over the built-in modem in {:.1} s",
        start.elapsed().as_secs_f64()
    );
    assert_eq!(
        (sent["delivered_by"].as_str(), sent["verified"].as_bool()),
        (Some("radio"), Some(true))
    );
    let inbox = get(b.http_addr, "/api/messages?direction=in");
    assert_eq!(
        (inbox[0]["text"].as_str(), inbox[0]["verified"].as_bool()),
        (Some("73 via the built-in modem"), Some(true))
    );
    assert!(get(a.http_addr, "/api/status")["radio_via"]
        .as_str()
        .unwrap()
        .contains("built-in modem"));
    a.stop().unwrap();
    b.stop().unwrap();
}

fn framed_link(ether: &Ether, me: &str, framing: Framing) -> SoundLink {
    let csma = Csma {
        framing,
        ..Csma::default()
    };
    SoundLink::start(call(me), audio(ether), ptt(&Recorder::default()), csma).unwrap()
}

fn hm_frame(i: u8) -> Vec<u8> {
    hm_wire::FrameHeader {
        ftype: hm_wire::FrameType::Data,
        src: call("SA0KAM"),
        dst: hm_wire::Dest::Station(call("SO5KM")),
        session: 1,
        index: i as u32,
    }
    .frame(&[i; 120])
    .unwrap()
}

/// In heavy noise, frames sent in IL2P arrive where the same frames in AX.25
/// are lost.
#[test]
fn il2p_gets_through_noise_that_stops_ax25() {
    let noise: f32 = std::env::var("HM_NOISE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.25);
    let delivered = |framing: Framing| {
        let ether = Ether::new(FS, noise, 7);
        let mut a = framed_link(&ether, "SA0KAM", framing);
        let mut b = framed_link(&ether, "SO5KM", Framing::Ax25);
        let frames: Vec<Vec<u8>> = (0..12).map(hm_frame).collect();
        for f in &frames {
            a.send(f).unwrap();
            thread::sleep(Duration::from_millis(1200));
        }
        let got = collect(&mut b, Duration::from_secs(3));
        frames.iter().filter(|f| got.contains(f)).count()
    };
    let (ax25, il2p) = (delivered(Framing::Ax25), delivered(Framing::Il2p));
    eprintln!("noise {noise}: AX.25 {ax25}/12, IL2P {il2p}/12");
    // Real-time CSMA on a loaded CI runner makes absolute margins noisy; require
    // IL2P ahead and still delivering most of the burst.
    assert!(
        il2p > ax25 && il2p >= 6,
        "IL2P should out-deliver AX.25 under noise: IL2P {il2p} vs AX.25 {ax25}"
    );
}

/// `auto` sends IL2P to a station that said it decodes it, and AX.25 to others.
#[test]
fn auto_framing_follows_what_the_peer_decodes() {
    let ether = Ether::new(FS, 0.0, 8);
    let mut a = framed_link(&ether, "SA0KAM", Framing::Auto);
    let mut tap = ether.port();
    let mut demod = hm_modem_afsk::Demodulator::new(hm_modem_afsk::DemodulatorConfig::new(FS));
    // Frames decoded so far and how many of them were IL2P, once `n` have
    // been heard: the audio runs in real time, so a slow machine gets a
    // generous deadline rather than a fixed window.
    let mut heard = |n: u64| {
        let end = Instant::now() + Duration::from_secs(20);
        let (mut audio, mut frames) = (Vec::new(), Vec::new());
        while demod.frames() < n && Instant::now() < end {
            audio.clear();
            tap.capture(&mut audio, Duration::from_millis(20)).unwrap();
            demod.process(&audio, &mut frames);
        }
        (demod.frames(), demod.il2p_frames())
    };
    a.send(&hm_frame(1)).unwrap();
    assert_eq!(heard(1), (1, 0), "not told: AX.25");
    a.peer_features(call("SO5KM"), a.features());
    a.send(&hm_frame(2)).unwrap();
    assert_eq!(heard(2), (2, 1), "told: IL2P");
    a.peer_features(call("SO5KM"), 0);
    a.send(&hm_frame(3)).unwrap();
    assert_eq!(heard(3), (3, 1), "told otherwise: AX.25 again");
}
