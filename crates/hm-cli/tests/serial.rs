//! KISS TNCs on serial ports, played by pseudo-terminals: the link opens the
//! terminal side by its device path, as it would /dev/ttyUSB0, and the test
//! plays the TNC on the other side.
#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
/// Parameter commands stay with the TNC they were sent to.
fn channel(a: TTYPort, b: TTYPort, stop: Arc<AtomicBool>) -> Vec<thread::JoinHandle<()>> {
    let a = Arc::new(Mutex::new(a));
    let b = Arc::new(Mutex::new(b));
    [(a.clone(), b.clone()), (b, a)]
        .into_iter()
        .map(|(from, to)| {
            let stop = stop.clone();
            thread::spawn(move || {
                let mut dec = kiss::Decoder::new(4096);
                let (mut buf, mut frames) = ([0u8; 4096], Vec::new());
                while !stop.load(Ordering::Relaxed) {
                    let n = from.lock().unwrap().read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    dec.push(&buf[..n], &mut frames);
                    for f in frames.drain(..).filter(|f| f.is_data()) {
                        let _ = to.lock().unwrap().write_all(&kiss::data_frame(f.port, &f.data));
                    }
                }
            })
        })
        .collect()
}

#[test]
fn hm_binary_over_serial_tncs() {
    let hm = env!("CARGO_BIN_EXE_hm");
    let dir = std::env::temp_dir().join(format!("hm-serial-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str]| Command::new(hm).args(args).output().unwrap();
    let (a_key, b_key) = (dir.join("a.key"), dir.join("b.key"));
    assert!(
        run(&["keygen", "--call", "SA0KAM", "--out", a_key.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        run(&["keygen", "--call", "SO5KM", "--out", b_key.to_str().unwrap()])
            .status
            .success()
    );
    let a_trust = dir.join("a-trusted.txt");
    std::fs::write(
        &a_trust,
        run(&["whoami", "--key", b_key.to_str().unwrap()]).stdout,
    )
    .unwrap();
    let b_trust = dir.join("b-trusted.txt");
    std::fs::write(
        &b_trust,
        run(&["whoami", "--key", a_key.to_str().unwrap()]).stdout,
    )
    .unwrap();

    let ((tnc_a, dev_a), (tnc_b, dev_b)) = (pty(), pty());
    let stop = Arc::new(AtomicBool::new(false));
    let relay = channel(tnc_a, tnc_b, stop.clone());
    let fast = ["--bitrate", "9600", "--txdelay", "50", "--guard", "300"];

    let mut listener = Command::new(hm)
        .args(["listen", "--key", b_key.to_str().unwrap(), "--ssid", "1"])
        .args(["--kiss", &format!("serial:{dev_b}:115200")])
        .args(["--trust", b_trust.to_str().unwrap()])
        .args(fast)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    let send = Command::new(hm)
        .args(["send", "--key", a_key.to_str().unwrap(), "--kiss", &dev_a])
        .args(["--to", "SO5KM-1", "--text", "over a serial TNC"])
        .args(["--timeout", "60", "--trust", a_trust.to_str().unwrap()])
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
    stop.store(true, Ordering::Relaxed);
    for r in relay {
        r.join().unwrap();
    }
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
