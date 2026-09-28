//! End to end over real TCP sockets, through a fake KISS TNC that plays the
//! radio channel: every data frame from one client goes to all the others,
//! optionally dropping every n-th one.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hm_bearer::{ax25, kiss};
use hm_bundle::Precedence;
use hm_cli::driver::Flow;
use hm_cli::files::{KeyFile, Trust};
use hm_cli::kiss_link::KissLink;
use hm_cli::station::{self, LinkTiming, Message, SendOutcome, Station, Verification};
use hm_wire::Callsign;

mod common;
use common::fake_tnc;
use hm_xfer::Receipt;

/// Fast link so the test runs in seconds: overs are predicted at 9600 bd.
const FAST: LinkTiming = LinkTiming {
    bitrate_bps: 9600,
    txdelay_ms: 50,
    guard_ms: 300,
    max_rounds: 12,
};

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

/// A station on the channel sending APRS position reports: noise hm must ignore.
fn aprs_noise(addr: SocketAddr, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut s = TcpStream::connect(addr).unwrap();
        let mut ui = ax25::wrap(call("SP5ZZZ"), b"!5213.00N/02100.00E-PHG2360").unwrap();
        let apdw: Vec<u8> = b"APDW18".iter().map(|c| c << 1).collect();
        ui[..6].copy_from_slice(&apdw);
        while !stop.load(Ordering::Relaxed) {
            let _ = s.write_all(&kiss::data_frame(0, &ui));
            thread::sleep(Duration::from_millis(150));
        }
    })
}

fn exchange(drop_every: usize, text: &str) -> (SendOutcome, Vec<Message>, usize) {
    let tnc = fake_tnc(drop_every);
    let alice = KeyFile::generate(call("SA0KAM")).unwrap();
    let bob = KeyFile::generate(call("SO5KM")).unwrap();
    let mut trust = Trust::default();
    trust.insert(alice.call, alice.identity.public());
    let mut alice_trust = Trust::default();
    alice_trust.insert(bob.call, bob.identity.public());

    let stop = Arc::new(AtomicBool::new(false));
    let got: Arc<Mutex<Vec<Message>>> = Arc::default();
    let listener = {
        let (stop, got, addr) = (stop.clone(), got.clone(), tnc.addr.to_string());
        thread::spawn(move || {
            let mut link = KissLink::connect(&addr, call("SO5KM-1"), 0).unwrap();
            let st = Station {
                key: &bob,
                trust: &trust,
                me: call("SO5KM-1"),
                timing: FAST,
            };
            st.listen(&mut link, None, Some(&stop), |m| {
                got.lock().unwrap().push(m);
                Flow::Continue
            })
            .unwrap();
        })
    };
    let noise = aprs_noise(tnc.addr, stop.clone());
    thread::sleep(Duration::from_millis(300)); // everyone connected before sending

    let mut link = KissLink::connect(&tnc.addr.to_string(), call("SA0KAM"), 0).unwrap();
    let bundle = station::build_bundle(
        &alice,
        call("SA0KAM"),
        call("SO5KM-1"),
        text,
        None,
        Precedence::Routine,
        Some(1),
    )
    .unwrap();
    let st = Station {
        key: &alice,
        trust: &alice_trust,
        me: call("SA0KAM"),
        timing: FAST,
    };
    let outcome = st
        .send_object(
            &mut link,
            call("SO5KM-1"),
            bundle.to_vec(),
            0,
            Duration::from_secs(60),
        )
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Relaxed);
    listener.join().unwrap();
    noise.join().unwrap();
    let msgs = got.lock().unwrap().clone();
    (outcome, msgs, tnc.dropped.load(Ordering::SeqCst))
}

#[test]
fn chat_over_kiss_tcp_is_delivered_and_verified() {
    let (outcome, msgs, _) = exchange(0, "73 de SA0KAM, test over KISS");
    assert!(
        matches!(
            outcome,
            SendOutcome::Delivered {
                rounds: 1,
                receipt: Receipt::Verified,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert_eq!(msgs[0].verification, Verification::Verified);
    assert_eq!(msgs[0].text().as_deref(), Some("73 de SA0KAM, test over KISS"));
    assert_eq!(msgs[0].via, call("SA0KAM"));
}

#[test]
fn lossy_channel_still_delivers_exactly_once() {
    let text: String = "The quick brown fox jumps over the lazy dog. ".repeat(40);
    let (outcome, msgs, dropped) = exchange(4, &text);
    assert!(
        matches!(
            outcome,
            SendOutcome::Delivered {
                receipt: Receipt::Verified,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(dropped > 0, "the channel dropped frames");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text().as_deref(), Some(text.as_str()));
}

/// The real binary: keygen, whoami, listen, send.
#[test]
fn hm_binary_end_to_end() {
    let hm = env!("CARGO_BIN_EXE_hm");
    let dir = std::env::temp_dir().join(format!("hm-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Each station in a directory of its own: `hm keygen` writes station.key
    // and a starter station.toml there, and `hm trust add` exchanges keys.
    let (a_cfg, b_cfg) = (dir.join("a/station.toml"), dir.join("b/station.toml"));
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    let run = |cfg: &std::path::Path, args: &[&str]| {
        Command::new(hm)
            .arg("--config")
            .arg(cfg)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(run(&a_cfg, &["keygen", "--call", "SA0KAM"]).status.success());
    assert!(run(&b_cfg, &["keygen", "--call", "SO5KM"]).status.success());
    assert!(
        !run(&b_cfg, &["keygen", "--call", "SO5KM"]).status.success(),
        "no overwrite"
    );
    assert!(dir.join("b/station.key").exists());
    let line = |cfg| {
        String::from_utf8(run(cfg, &["whoami"]).stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let (a_line, b_line) = (line(&a_cfg), line(&b_cfg));
    assert!(run(&b_cfg, &["trust", "add", &a_line, "--note", "Alice"])
        .status
        .success());
    assert!(run(&a_cfg, &["trust", "add", &b_line]).status.success());
    let listed = String::from_utf8(run(&b_cfg, &["trust", "list"]).stdout).unwrap();
    assert!(
        listed.contains("SA0KAM ") && listed.contains("# Alice"),
        "{listed}"
    );
    assert!(!run(&b_cfg, &["trust", "add", "SA0KAM nothex"]).status.success());

    let tnc = fake_tnc(0);
    let addr = tnc.addr.to_string();
    let fast = ["--bitrate", "9600", "--txdelay", "50", "--guard", "300"];
    let mut listener = Command::new(hm)
        .arg("--config")
        .arg(&b_cfg)
        .args(["listen", "--ssid", "1", "--kiss", &addr])
        .args(fast)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    let send = Command::new(hm)
        .arg("--config")
        .arg(&a_cfg)
        .args(["send", "--kiss", &addr, "--to", "SO5KM-1"])
        .args([
            "--text",
            "hello from the hm binary",
            "--subject",
            "test",
            "--precedence",
            "priority",
        ])
        .args(fast)
        .args(["--timeout", "60"])
        .output()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    listener.kill().unwrap();
    let mut printed = String::new();
    listener
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut printed)
        .unwrap();
    let _ = listener.wait();
    let sent = String::from_utf8_lossy(&send.stdout);
    assert!(
        send.status.success(),
        "send failed: {sent} {}",
        String::from_utf8_lossy(&send.stderr)
    );
    assert!(
        sent.contains("Delivered to SO5KM-1") && sent.contains("receipt verified"),
        "{sent}"
    );
    assert!(
        printed.contains("SA0KAM via SA0KAM (verified) Mail [test]: hello from the hm binary"),
        "{printed}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_key_for_one_ssid_is_that_station_only() {
    let hm = env!("CARGO_BIN_EXE_hm");
    let dir = std::env::temp_dir().join(format!("hm-ssid-key-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("server.key");
    let cfg = dir.join("station.toml");
    let run = |args: &[&str]| {
        Command::new(hm)
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(
        run(&["keygen", "--call", "SA0KAM-2", "--out", key.to_str().unwrap()])
            .status
            .success()
    );
    let whoami = String::from_utf8(run(&["whoami", "--key", key.to_str().unwrap()]).stdout).unwrap();
    assert!(whoami.starts_with("SA0KAM-2 "), "{whoami}");
    // Running it as another SSID is refused before anything goes on air.
    let out = run(&[
        "send",
        "--key",
        key.to_str().unwrap(),
        "--ssid",
        "1",
        "--to",
        "SO5KM",
        "--text",
        "x",
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("is for SA0KAM-2 only"),
        "{err}"
    );
    // Keygen wrote a starter settings file next to the key.
    assert!(cfg.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
