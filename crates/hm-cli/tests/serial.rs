//! KISS TNCs on serial ports, played by pseudo-terminals: the link opens the
//! terminal side by its device path, as it would /dev/ttyUSB0, and the test
//! plays the TNC on the other side.
#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use hm_bearer::{ax25, kiss};
use hm_cli::driver::Link;
use hm_cli::kiss_link::{KissLink, KissTarget, TncParams};
use hm_wire::Callsign;
use serialport::{SerialPort, TTYPort};

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

/// A pseudo-terminal: the TNC's end, and the device path the link opens.
fn pty() -> (TTYPort, String) {
    let (mut tnc, station) = TTYPort::pair().expect("pseudo-terminal");
    tnc.set_timeout(Duration::from_millis(50)).unwrap();
    let path = station.name().expect("pty has a name");
    // Close our copy of the station side, so the link is its only user.
    drop(station);
    (tnc, path)
}

/// Reads KISS frames the station sent to its TNC, until `n` or `wait` passes.
fn read_frames(tnc: &mut TTYPort, n: usize, wait: Duration) -> Vec<kiss::KissFrame> {
    let mut dec = kiss::Decoder::new(4096);
    let (mut got, mut buf) = (Vec::new(), [0u8; 1024]);
    let end = Instant::now() + wait;
    while got.len() < n && Instant::now() < end {
        match tnc.read(&mut buf) {
            Ok(k) if k > 0 => dec.push(&buf[..k], &mut got),
            _ => thread::sleep(Duration::from_millis(5)),
        }
    }
    got
}

#[test]
fn serial_tnc_gets_channel_access_and_carries_frames() {
    let (mut tnc, path) = pty();
    let params = TncParams {
        txdelay_ms: 250,
        persist: 31,
        slot_ms: 50,
    };
    let target = KissTarget::parse(&format!("serial:{path}:57600")).unwrap();
    let mut link = KissLink::open(&target, call("SA0KAM-1"), 2, params).unwrap();

    // On opening, the TNC is told TXDELAY, persistence and slot time for our port.
    let got = read_frames(&mut tnc, 3, Duration::from_secs(2));
    let cmds: Vec<(u8, u8, Vec<u8>)> = got.into_iter().map(|f| (f.port, f.command, f.data)).collect();
    assert_eq!(cmds, vec![(2, 1, vec![25]), (2, 2, vec![31]), (2, 3, vec![5])]);

    // An hm frame goes out as an AX.25 UI frame in a KISS data frame on port 2.
    link.send(b"an hm frame").unwrap();
    let got = read_frames(&mut tnc, 1, Duration::from_secs(2));
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].port, got[0].command), (2, 0));
    assert_eq!(ax25::unwrap(&got[0].data), Some(&b"an hm frame"[..]));

    // From the air: other ports and APRS are ignored, our hm frame comes up.
    let ui = |src: &str, info: &[u8]| ax25::wrap(call(src), info).unwrap();
    let mut aprs = ui("SP5ZZZ", b"!5213.00N/02100.00E-");
    aprs[..6].copy_from_slice(&b"APDW18".map(|c| c << 1));
    tnc.write_all(&kiss::data_frame(0, &ui("SO5KM", b"wrong port")))
        .unwrap();
    tnc.write_all(&kiss::data_frame(2, &aprs)).unwrap();
    tnc.write_all(&kiss::data_frame(2, &ui("SO5KM", b"for us")))
        .unwrap();
    let mut heard = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && heard.is_empty() {
        if let Some(f) = link.recv_timeout(Duration::from_millis(100)).unwrap() {
            heard.push(f);
        }
    }
    assert_eq!(heard, vec![b"for us".to_vec()]);
    assert_eq!(link.recv_timeout(Duration::from_millis(200)).unwrap(), None);
}

#[test]
fn a_dropped_link_lets_go_of_the_port() {
    let (_tnc, path) = pty();
    let target = KissTarget::parse(&format!("serial:{path}")).unwrap();
    let link = KissLink::open(&target, call("SA0KAM"), 0, TncParams::default()).unwrap();
    // The port is held exclusively while the link is open.
    assert!(KissLink::open(&target, call("SA0KAM"), 0, TncParams::default()).is_err());
    drop(link);
    // The reader thread notices within its poll interval and closes its copy.
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        match KissLink::open(&target, call("SA0KAM"), 0, TncParams::default()) {
            Ok(_) => break,
            Err(e) => assert!(Instant::now() < end, "port still held after drop: {e}"),
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Two serial TNCs on one channel: data frames from one reach the other.
/// Parameter commands stay with the TNC they were sent to. Each direction
/// reads and writes its own handles, so a read waiting on one terminal never
/// holds up a write to the other. The threads end with the test process.
fn channel(a: TTYPort, b: TTYPort) {
    for (mut from, mut to) in [
        (a.try_clone_native().unwrap(), b.try_clone_native().unwrap()),
        (b, a),
    ] {
        thread::spawn(move || {
            let mut dec = kiss::Decoder::new(4096);
            let (mut buf, mut frames) = ([0u8; 4096], Vec::new());
            loop {
                let n = from.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                dec.push(&buf[..n], &mut frames);
                for f in frames.drain(..).filter(|f| f.is_data()) {
                    let _ = to.write_all(&kiss::data_frame(f.port, &f.data));
                }
            }
        });
    }
}

#[test]
fn hm_binary_over_serial_tncs() {
    let hm = env!("CARGO_BIN_EXE_hm");
    let dir = std::env::temp_dir().join(format!("hm-serial-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
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
    let line = |cfg| {
        String::from_utf8(run(cfg, &["whoami"]).stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let (a_line, b_line) = (line(&a_cfg), line(&b_cfg));
    assert!(run(&b_cfg, &["trust", "add", &a_line]).status.success());
    assert!(run(&a_cfg, &["trust", "add", &b_line]).status.success());

    let ((tnc_a, dev_a), (tnc_b, dev_b)) = (pty(), pty());
    channel(tnc_a, tnc_b);
    let fast = ["--bitrate", "9600", "--txdelay", "50", "--guard", "300"];

    let mut listener = Command::new(hm)
        .arg("--config")
        .arg(&b_cfg)
        .args(["listen", "--ssid", "1"])
        .args(["--kiss", &format!("serial:{dev_b}:115200")])
        .args(fast)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    let send = Command::new(hm)
        .arg("--config")
        .arg(&a_cfg)
        .args(["send", "--kiss", &dev_a])
        .args(["--to", "SO5KM-1", "--text", "over a serial TNC"])
        .args(["--timeout", "60"])
        .args(fast)
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
    assert!(printed.contains("over a serial TNC"), "{printed}");
    std::fs::remove_dir_all(&dir).unwrap();
}
