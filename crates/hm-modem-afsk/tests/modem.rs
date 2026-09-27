//! The modem against synthetic channels, and against Direwolf 1.7 when its
//! `gen_packets` and `atest` tools are installed.

use std::process::Command;

use hm_core::DetRng;
use hm_modem_afsk::{Demodulator, DemodulatorConfig, Modulator};

fn gaussian(rng: &mut DetRng) -> f32 {
    let u1 = rng.next_f64().max(f64::MIN_POSITIVE);
    let u2 = rng.next_f64();
    ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
}

/// White noise at `snr_db` relative to the signal power, over the whole audio band.
fn add_noise(x: &mut [f32], snr_db: f32, rng: &mut DetRng) {
    let p: f32 = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
    let sigma = (p / 10f32.powf(snr_db / 10.0)).sqrt();
    for v in x.iter_mut() {
        *v += sigma * gaussian(rng);
    }
}

/// First-order tilt: `db_per_octave` > 0 boosts highs (pre-emphasis), < 0 cuts them.
fn tilt(x: &mut [f32], fs: f32, db_per_octave: f32) {
    // Blend of the signal and its derivative gives a slope around the tones.
    let k = (10f32.powf(db_per_octave / 20.0) - 1.0) * fs / (std::f32::consts::TAU * 1600.0);
    let mut prev = 0.0;
    for v in x.iter_mut() {
        let d = *v - prev;
        prev = *v;
        *v = if db_per_octave >= 0.0 { *v + k * d } else { *v };
    }
    if db_per_octave < 0.0 {
        // One-pole low-pass for de-emphasis.
        let a = (-std::f32::consts::TAU * 1200.0 / fs * (-db_per_octave / 6.0)).exp();
        let mut y = 0.0;
        for v in x.iter_mut() {
            y = a * y + (1.0 - a) * *v;
            *v = y;
        }
    }
}

fn frames(n: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut rng = DetRng::from_seed(seed);
    (0..n)
        .map(|_| {
            (0..20 + rng.below(200) as usize)
                .map(|_| rng.next_u64() as u8)
                .collect()
        })
        .collect()
}

/// Each frame in its own transmission with silence between, as on a shared channel.
fn transmissions(m: &Modulator, fs: u32, frames: &[Vec<u8>]) -> Vec<f32> {
    let mut audio = Vec::new();
    for f in frames {
        audio.extend(m.modulate(&[f.as_slice()], 150));
        audio.extend(std::iter::repeat_n(0.0, fs as usize / 5));
    }
    audio
}

fn decode(fs: u32, audio: &[f32]) -> Vec<Vec<u8>> {
    let mut d = Demodulator::new(DemodulatorConfig::new(fs));
    let mut out = Vec::new();
    d.process(audio, &mut out);
    out
}

fn success(sent: &[Vec<u8>], got: &[Vec<u8>]) -> usize {
    sent.iter().filter(|f| got.contains(f)).count()
}

#[test]
fn clean_roundtrip_at_common_sample_rates() {
    for fs in [8_000, 11_025, 22_050, 44_100, 48_000] {
        let sent = frames(20, fs as u64);
        let audio = transmissions(&Modulator::new(fs), fs, &sent);
        let got = decode(fs, &audio);
        assert_eq!(success(&sent, &got), 20, "at {fs} Hz");
        assert_eq!(got.len(), 20, "one copy of each frame at {fs} Hz");
    }
}

#[test]
fn back_to_back_frames_in_one_transmission() {
    let fs = 48_000;
    let sent = frames(8, 7);
    let refs: Vec<&[u8]> = sent.iter().map(|f| f.as_slice()).collect();
    let audio = Modulator::new(fs).modulate(&refs, 300);
    assert_eq!(decode(fs, &audio), sent);
}

#[test]
fn noise_sweep() {
    let fs = 48_000;
    let mut rng = DetRng::from_seed(11);
    let mut report = Vec::new();
    for snr in [20.0, 12.0, 9.0, 6.0, 3.0] {
        let sent = frames(40, snr as u64);
        let mut audio = transmissions(&Modulator::new(fs), fs, &sent);
        add_noise(&mut audio, snr, &mut rng);
        let ok = success(&sent, &decode(fs, &audio));
        report.push((snr, ok));
    }
    eprintln!("frames decoded of 40 by SNR (dB, full band): {report:?}");
    let at = |s: f32| report.iter().find(|(x, _)| *x == s).unwrap().1;
    assert_eq!(at(20.0), 40);
    assert!(at(12.0) >= 39, "{report:?}");
    assert!(at(9.0) >= 36, "{report:?}");
}

#[test]
fn tilted_audio_from_emphasis() {
    let fs = 44_100;
    for db in [-6.0, 6.0] {
        let sent = frames(20, 3);
        let mut audio = transmissions(&Modulator::new(fs), fs, &sent);
        tilt(&mut audio, fs as f32, db);
        let ok = success(&sent, &decode(fs, &audio));
        assert!(ok >= 19, "{ok}/20 with {db} dB/octave tilt");
    }
}

#[test]
fn transmitter_clock_off_by_a_percent() {
    let sent = frames(20, 5);
    // Modulate at 48.48 kHz, play back as 48 kHz: 1% fast baud and tones.
    let audio = transmissions(&Modulator::new(48_480), 48_480, &sent);
    assert!(success(&sent, &decode(48_000, &audio)) >= 19);
}

#[test]
fn carrier_detect_follows_the_signal() {
    let fs = 48_000;
    let mut d = Demodulator::new(DemodulatorConfig::new(fs));
    let mut out = Vec::new();
    let mut rng = DetRng::from_seed(9);
    let mut noise: Vec<f32> = (0..fs as usize).map(|_| 0.2 * gaussian(&mut rng)).collect();
    d.process(&noise, &mut out);
    assert!(!d.dcd(), "noise alone is not a carrier");
    let sent = frames(1, 1);
    let audio = Modulator::new(fs).modulate(&[sent[0].as_slice()], 300);
    d.process(&audio[..audio.len() / 2], &mut out);
    assert!(d.dcd(), "carrier detected mid-transmission");
    noise.truncate(fs as usize / 2);
    d.process(&noise, &mut out);
    assert!(!d.dcd(), "released after the carrier ends");
}

// ---- Direwolf interop -------------------------------------------------------

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-x").output().is_ok()
}

fn write_wav(path: &std::path::Path, fs: u32, x: &[f32]) {
    let mut b = Vec::with_capacity(44 + 2 * x.len());
    let data = (2 * x.len()) as u32;
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&fs.to_le_bytes());
    b.extend_from_slice(&(fs * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for v in x {
        b.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// 16-bit PCM WAV, first channel.
fn read_wav(path: &std::path::Path) -> (u32, Vec<f32>) {
    let b = std::fs::read(path).unwrap();
    let mut i = 12;
    let (mut fs, mut channels, mut data) = (0u32, 1usize, &b[0..0]);
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " {
            channels = u16::from_le_bytes(body[2..4].try_into().unwrap()) as usize;
            fs = u32::from_le_bytes(body[4..8].try_into().unwrap());
        } else if id == b"data" {
            data = body;
        }
        i += 8 + len + (len & 1);
    }
    let x = data
        .chunks_exact(2 * channels)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    (fs, x)
}

fn atest_count(path: &std::path::Path) -> usize {
    atest_count_with(path, &[])
}

/// Direwolf's best result: its default and its multi-slicer "A+" profile.
fn atest_best(path: &std::path::Path) -> usize {
    atest_count(path).max(atest_count_with(path, &["-P", "A+"]))
}

fn atest_count_with(path: &std::path::Path, args: &[&str]) -> usize {
    let out = Command::new("atest").args(args).arg(path).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| {
            l.split_once(" packets decoded")
                .and_then(|(n, _)| n.trim().parse().ok())
        })
        .unwrap_or(0)
}

/// A valid AX.25 UI frame so Direwolf's decoder accepts and counts it.
fn ax25_ui(n: usize) -> Vec<u8> {
    let addr = |call: &str, ssid: u8, last: bool| {
        let mut a: Vec<u8> = format!("{call:<6}").bytes().map(|c| c << 1).collect();
        // A command, as hm sends: C bit set on the destination.
        a.push(0x60 | (ssid << 1) | last as u8 | if last { 0 } else { 0x80 });
        a
    };
    let mut f = addr("HMNET", 0, false);
    f.extend(addr("SA0KAM", 1, true));
    f.extend_from_slice(&[0x03, 0xF0]);
    f.extend(format!(">hm-net modem interop test frame {n:03}").bytes());
    f
}

#[test]
fn direwolf_decodes_our_audio() {
    if !have("atest") {
        eprintln!("skipped: Direwolf's atest is not installed");
        return;
    }
    let dir = std::env::temp_dir();
    let mut rng = DetRng::from_seed(21);
    for (fs, snr) in [(44_100u32, None), (48_000, Some(12.0f32))] {
        let sent: Vec<Vec<u8>> = (0..50).map(ax25_ui).collect();
        let mut audio = transmissions(&Modulator::new(fs), fs, &sent);
        if let Some(s) = snr {
            add_noise(&mut audio, s, &mut rng);
        }
        let path = dir.join(format!("hm-to-dw-{fs}-{}.wav", std::process::id()));
        write_wav(&path, fs, &audio);
        let n = atest_count(&path);
        let ours = success(&sent, &decode(fs, &audio));
        eprintln!("our AFSK at {fs} Hz, SNR {snr:?}: Direwolf decoded {n}/50, we decoded {ours}/50");
        assert!(n >= 49, "Direwolf decoded only {n}/50 of our frames");
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn we_decode_direwolf_audio_as_well_as_direwolf() {
    if !have("gen_packets") || !have("atest") {
        eprintln!("skipped: Direwolf's gen_packets and atest are not installed");
        return;
    }
    let dir = std::env::temp_dir();
    let mut total = (0, 0);
    for (fs, n) in [(44_100u32, 100usize), (48_000, 100), (22_050, 100), (11_025, 100)] {
        // gen_packets -n: n frames with steadily increasing noise, the usual Direwolf benchmark.
        let path = dir.join(format!("dw-{fs}-{}.wav", std::process::id()));
        let st = Command::new("gen_packets")
            .args(["-n", &n.to_string(), "-r", &fs.to_string(), "-o"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(st.status.success());
        let theirs = atest_best(&path);
        let (rate, audio) = read_wav(&path);
        assert_eq!(rate, fs);
        let ours = decode(fs, &audio).len();
        eprintln!("gen_packets -n {n} at {fs} Hz: Direwolf decoded {theirs}, we decoded {ours}");
        total.0 += theirs;
        total.1 += ours;
        std::fs::remove_file(path).unwrap();
    }
    eprintln!(
        "total: Direwolf {}, ours {} ({:.0}%)",
        total.0,
        total.1,
        100.0 * total.1 as f64 / total.0 as f64
    );
    // Phase 1 exit criterion: at least 95% of Direwolf's decodes (its better profile).
    assert!(
        total.1 * 100 >= total.0 * 95,
        "ours {} vs Direwolf {}",
        total.1,
        total.0
    );
}

/// Held out from tuning: our audio with emphasis tilt and noise, decoded by both.
#[test]
fn held_out_tilt_and_noise_against_direwolf() {
    if !have("atest") {
        eprintln!("skipped: Direwolf's atest is not installed");
        return;
    }
    let fs = 44_100;
    let dir = std::env::temp_dir();
    let mut rng = DetRng::from_seed(77);
    let (mut theirs, mut ours) = (0, 0);
    for (db, snr) in [
        (-6.0, 3.0),
        (6.0, 3.0),
        (0.0, -1.0),
        (-6.0, -2.0),
        (6.0, -2.0),
        (0.0, -3.0),
        (0.0, -4.0),
    ] {
        let sent: Vec<Vec<u8>> = (0..50).map(ax25_ui).collect();
        let mut audio = transmissions(&Modulator::new(fs), fs, &sent);
        tilt(&mut audio, fs as f32, db);
        add_noise(&mut audio, snr, &mut rng);
        let peak = audio.iter().fold(0f32, |m, v| m.max(v.abs()));
        audio.iter_mut().for_each(|v| *v *= 0.9 / peak);
        let path = dir.join(format!("held-out-{}-{}.wav", db as i32, std::process::id()));
        write_wav(&path, fs, &audio);
        let t = atest_best(&path);
        let o = success(&sent, &decode(fs, &audio));
        eprintln!("tilt {db} dB/octave, SNR {snr} dB: Direwolf {t}/50, ours {o}/50");
        theirs += t;
        ours += o;
        std::fs::remove_file(path).unwrap();
    }
    eprintln!("held out total: Direwolf {theirs}, ours {ours}");
    assert!(ours * 100 >= theirs * 95, "ours {ours} vs Direwolf {theirs}");
}

/// IL2P frames from Direwolf's `gen_packets -I` decode here, in noise too.
#[test]
fn we_decode_direwolf_il2p() {
    if !have("gen_packets") || !have("atest") {
        eprintln!("skipped: Direwolf's gen_packets and atest are not installed");
        return;
    }
    let dir = std::env::temp_dir();
    for (fec, n) in [("1", 100usize), ("0", 100)] {
        let fs = 48_000u32;
        let path = dir.join(format!("dw-il2p-{fec}-{}.wav", std::process::id()));
        let st = Command::new("gen_packets")
            .args(["-I", fec, "-n", &n.to_string(), "-r", &fs.to_string(), "-o"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(st.status.success());
        let theirs = atest_best(&path);
        let (_, audio) = read_wav(&path);
        let got = decode(fs, &audio);
        // Every frame is a Direwolf test UI frame from WB2OSZ-15.
        assert!(got
            .iter()
            .all(|f| f.len() > 16 && f[7..13] == b"WB2OSZ".map(|c| c << 1)));
        eprintln!(
            "gen_packets -I {fec} -n {n}: Direwolf decoded {theirs}, we decoded {}",
            got.len()
        );
        assert!(
            got.len() * 100 >= theirs * 95,
            "ours {} vs Direwolf {theirs}",
            got.len()
        );
        std::fs::remove_file(path).unwrap();
    }
}

/// Our IL2P frames decode in Direwolf, with both FEC levels.
#[test]
fn direwolf_decodes_our_il2p() {
    if !have("atest") {
        eprintln!("skipped: Direwolf's atest is not installed");
        return;
    }
    let dir = std::env::temp_dir();
    let fs = 48_000u32;
    for max_fec in [true, false] {
        let sent: Vec<Vec<u8>> = (0..50).map(ax25_ui).collect();
        let m = Modulator::new(fs);
        let mut audio = Vec::new();
        for f in &sent {
            audio.extend(m.modulate_il2p(&[f.as_slice()], 150, max_fec));
            audio.extend(std::iter::repeat_n(0.0, fs as usize / 5));
        }
        let path = dir.join(format!("hm-il2p-{max_fec}-{}.wav", std::process::id()));
        write_wav(&path, fs, &audio);
        let n = atest_count(&path);
        let ours = success(&sent, &decode(fs, &audio));
        eprintln!("our IL2P (max FEC {max_fec}): Direwolf decoded {n}/50, we decoded {ours}/50");
        assert_eq!((n, ours), (50, 50));
        std::fs::remove_file(path).unwrap();
    }
}

/// What IL2P buys in noise: the same frames, HDLC against IL2P, at falling SNR.
#[test]
fn il2p_outlasts_hdlc_in_noise() {
    let fs = 48_000;
    let mut rng = DetRng::from_seed(31);
    let sent: Vec<Vec<u8>> = (0..40).map(ax25_ui).collect();
    let m = Modulator::new(fs);
    let mut report = Vec::new();
    let (mut hdlc_total, mut il2p_total) = (0, 0);
    for snr in [-2.0f32, -3.0, -4.0, -5.0, -6.0] {
        let mut ok = [0usize; 2];
        for (k, il2p) in [false, true].into_iter().enumerate() {
            for f in &sent {
                let mut audio = if il2p {
                    m.modulate_il2p(&[f.as_slice()], 150, true)
                } else {
                    m.modulate(&[f.as_slice()], 150)
                };
                audio.extend(std::iter::repeat_n(0.0, fs as usize / 10));
                add_noise(&mut audio, snr, &mut rng);
                ok[k] += decode(fs, &audio).contains(f) as usize;
            }
        }
        hdlc_total += ok[0];
        il2p_total += ok[1];
        report.push(format!("{snr} dB: HDLC {}/40, IL2P {}/40", ok[0], ok[1]));
    }
    eprintln!("{}", report.join("; "));
    assert!(il2p_total > hdlc_total, "IL2P {il2p_total} vs HDLC {hdlc_total}");
    assert!(
        il2p_total >= 184,
        "deep-noise exit criterion: IL2P decoded {il2p_total}/200; expected at least 184"
    );
}
