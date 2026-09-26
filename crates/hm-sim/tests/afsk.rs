//! The simulator's AFSK 1200 channel against the real modem: airtime of hm
//! frames sent as AX.25 UI frames by the built-in modulator.

use hm_bearer::ax25;
use hm_core::{DetRng, Millis};
use hm_modem_afsk::{Modulator, BAUD};
use hm_sim::RadioParams;
use hm_wire::Callsign;

/// 10 samples per bit, so sample counts convert to bits exactly.
const FS: u32 = 12_000;

/// Frames like the transfer engine's: an 18-byte header and random symbol bytes.
fn frames(rng: &mut DetRng, n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|_| {
            let len = 18 + rng.below(240) as usize;
            let mut f: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            if rng.chance(0.1) {
                // Now and then a zero-padded tail, as at the end of an object.
                let pad = len / 3;
                f[len - pad..].fill(0);
            }
            f
        })
        .collect()
}

/// Airtime of one transmission as the modulator produces it, in bits.
fn modulated_bits(frames: &[Vec<u8>], txdelay_ms: u32) -> u64 {
    let me = Callsign::parse("SA0KAM-1").unwrap();
    let ui: Vec<Vec<u8>> = frames.iter().map(|f| ax25::wrap(me, f).unwrap()).collect();
    let refs: Vec<&[u8]> = ui.iter().map(|f| f.as_slice()).collect();
    let samples = Modulator::new(FS).modulate(&refs, txdelay_ms).len() as u64;
    let per_bit = (FS as f32 / BAUD) as u64;
    assert_eq!(samples % per_bit, 0);
    samples / per_bit
}

#[test]
fn sim_airtime_matches_the_modulator() {
    let p = RadioParams::VHF_1200;
    let mut rng = DetRng::from_seed(1);
    let (mut worst, mut sum_err, mut sum_real, mut n) = (0i64, 0i64, 0u64, 0u64);
    for burst in 1..=8 {
        for _ in 0..40 {
            let fs = frames(&mut rng, burst);
            let real_bits = modulated_bits(&fs, p.txdelay.0 as u32);
            let real_ms = (real_bits * 1000).div_ceil(p.bitrate_bps as u64) as i64;
            let sim_ms: u64 = fs
                .iter()
                .enumerate()
                .map(|(i, f)| p.airtime_of(f, i == 0).0)
                .sum();
            let err = sim_ms as i64 - real_ms;
            worst = worst.max(err.abs());
            sum_err += err;
            sum_real += real_ms as u64;
            n += 1;
            // Rounding up to 1 ms per frame, and the stuffing of the AX.25
            // header and frame check that the sim leaves out: a few ms per frame.
            assert!(
                err.abs() <= 3 * burst as i64 + 2,
                "{burst} frames: sim {sim_ms} ms, modulator {real_ms} ms"
            );
        }
    }
    let bias = sum_err as f64 / sum_real as f64;
    eprintln!(
        "{n} transmissions: worst error {worst} ms, mean bias {:+.3}% of airtime",
        bias * 100.0
    );
    assert!(bias.abs() < 0.005, "sim airtime biased by {:.2}%", bias * 100.0);
}

#[test]
fn per_frame_overhead_is_what_the_link_adds() {
    // The modulator's cost of one more frame in a key-up, with no stuffing in
    // the payload, is the AX.25 header, the frame check and a flag.
    let a = modulated_bits(&[vec![0u8; 100]], 300);
    let b = modulated_bits(&[vec![0u8; 100], vec![0u8; 100]], 300);
    let per_frame_bytes = (b - a) / 8 - 100;
    assert!(
        (b - a) % 8 <= 2,
        "header and frame-check stuffing is small: {} bits",
        (b - a) % 8
    );
    assert_eq!(per_frame_bytes, RadioParams::VHF_1200.phy_overhead_bytes as u64);
    assert_eq!(per_frame_bytes, (ax25::UI_OVERHEAD + 2 + 1) as u64);
    // Key-up: TXDELAY of flags, then the frame, then the tail.
    let tail_bits = a - 300 * 1200 / 1000 - (100 + 19) * 8 - (b - a - 119 * 8);
    let tail = Millis((tail_bits * 1000).div_ceil(1200));
    assert_eq!(tail, RadioParams::VHF_1200.txtail);
}

// ---- frame loss against SNR -------------------------------------------------

fn gaussian(rng: &mut DetRng) -> f32 {
    let u1 = rng.next_f64().max(f64::MIN_POSITIVE);
    let u2 = rng.next_f64();
    ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
}

/// Frames lost of `n` AX.25 frames of `on_air` bytes each (frame check and
/// flag included), each in its own key-up with 300 ms of TXDELAY flags, in
/// white noise at `snr_db` measured in a 3 kHz bandwidth. The receiver hears
/// noise between transmissions too.
fn frames_lost(fs: u32, snr_db: f32, on_air: usize, n: usize, seed: u64) -> usize {
    let mut rng = DetRng::from_seed(seed);
    let m = Modulator::new(fs);
    let signal_power = m.amplitude * m.amplitude / 2.0;
    let sigma = (signal_power * (fs as f32 / 2.0) / (3000.0 * 10f32.powf(snr_db / 10.0))).sqrt();
    let mut demod = hm_modem_afsk::Demodulator::new(hm_modem_afsk::DemodulatorConfig::new(fs));
    let (mut decoded, mut sent) = (Vec::new(), Vec::new());
    for i in 0..n {
        let mut f: Vec<u8> = (0..on_air - 3).map(|_| rng.next_u64() as u8).collect();
        f[..4].copy_from_slice(&(i as u32).to_be_bytes());
        let mut audio = m.modulate(&[f.as_slice()], 300);
        audio.extend(std::iter::repeat_n(0.0, fs as usize / 5));
        for v in audio.iter_mut() {
            *v += sigma * gaussian(&mut rng);
        }
        demod.process(&audio, &mut decoded);
        sent.push(f);
    }
    sent.iter().filter(|f| !decoded.contains(f)).count()
}

/// Sample rate of the table: what sound cards run at. The modem does about
/// 1 dB worse at 12 kHz, so a table measured there would be too pessimistic.
const CURVE_FS: u32 = 48_000;
const CURVE_SNR_DB: [f64; 15] = [
    4.0, 4.5, 5.0, 5.5, 6.0, 6.5, 7.0, 7.5, 8.0, 8.5, 9.0, 9.5, 10.0, 10.5, 11.0,
];
/// Bytes on air: an 18-byte hm header plus AX.25 (19 bytes) is 37, an ACK with
/// receipt 116, a DATA frame with a 200-byte symbol 241.
const CURVE_LEN: [u32; 5] = [40, 80, 160, 240, 360];
const CURVE_FRAMES: usize = 300;

/// Measures every grid point on all cores: `lost[i][j]` of `CURVE_FRAMES`.
fn measure_curve() -> Vec<Vec<usize>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    let cells: Vec<(usize, usize)> = (0..CURVE_SNR_DB.len())
        .flat_map(|i| (0..CURVE_LEN.len()).map(move |j| (i, j)))
        .collect();
    let next = AtomicUsize::new(0);
    let lost = Mutex::new(vec![vec![0usize; CURVE_LEN.len()]; CURVE_SNR_DB.len()]);
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(i, j)) = cells.get(k) else { break };
                let seed = 0xC0DE_0000 + (i as u64) * 100 + j as u64;
                let n = frames_lost(
                    CURVE_FS,
                    CURVE_SNR_DB[i] as f32,
                    CURVE_LEN[j] as usize,
                    CURVE_FRAMES,
                    seed,
                );
                lost.lock().unwrap()[i][j] = n;
            });
        }
    });
    lost.into_inner().unwrap()
}

fn curve_source(lost: &[Vec<usize>]) -> String {
    let list = |v: Vec<String>| v.join(", ");
    let mut rows = String::new();
    for (i, row) in lost.iter().enumerate() {
        let cells = list(
            row.iter()
                .map(|&n| format!("{:.4}", n as f64 / CURVE_FRAMES as f64))
                .collect(),
        );
        rows += &format!("        &[{cells}], // {:.1} dB\n", CURVE_SNR_DB[i]);
    }
    format!(
        r#"//! Frame loss of the built-in AFSK 1200 modem (`hm-modem-afsk`) in white noise.
//!
//! Generated; do not edit by hand. To measure again and rewrite this file:
//! `HM_WRITE_CURVE=1 cargo test -p hm-sim --release --test afsk afsk_1200_curve -- --ignored`
//! (without `HM_WRITE_CURVE` the same test checks this table against the modem).
//!
//! Each point: {frames} AX.25 frames of random bytes, each in its own key-up with
//! 300 ms of TXDELAY flags and 200 ms of silence after, sampled at {fs} Hz, in
//! white Gaussian noise at the given SNR (noise power in a 3 kHz bandwidth),
//! heard continuously by one demodulator. Flat audio: no emphasis tilt, no
//! fading, no FM threshold or squelch effects.

use crate::curve::LossCurve;

pub const CURVE: LossCurve = LossCurve {{
    name: "hm-modem-afsk 1200 bd, white noise, {fs} Hz",
    snr_db: &[
        {snr},
    ],
    len: &[{len}],
    loss: &[
{rows}    ],
    frames_per_point: {frames},
}};
"#,
        frames = CURVE_FRAMES,
        fs = CURVE_FS,
        snr = list(CURVE_SNR_DB.iter().map(|v| format!("{v:.1}")).collect()),
        len = list(CURVE_LEN.iter().map(|v| v.to_string()).collect()),
    )
}

/// Measures the modem on the table's grid. With `HM_WRITE_CURVE=1` writes
/// `src/afsk_1200.rs`; otherwise checks the committed table against the
/// measurement, point by point, within binomial noise.
#[test]
#[ignore]
fn afsk_1200_curve() {
    let t = std::time::Instant::now();
    let lost = measure_curve();
    eprintln!(
        "measured {} points in {:.0?}",
        CURVE_SNR_DB.len() * CURVE_LEN.len(),
        t.elapsed()
    );
    for (i, row) in lost.iter().enumerate() {
        eprintln!(
            "{:5.1} dB: lost of {CURVE_FRAMES} by length {CURVE_LEN:?}: {row:?}",
            CURVE_SNR_DB[i]
        );
    }
    if std::env::var_os("HM_WRITE_CURVE").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/afsk_1200.rs");
        std::fs::write(&path, curve_source(&lost)).unwrap();
        eprintln!("wrote {}", path.display());
        return;
    }
    let c = &hm_sim::afsk_1200::CURVE;
    assert_eq!(
        c.snr_db, CURVE_SNR_DB,
        "grid changed: regenerate with HM_WRITE_CURVE=1"
    );
    assert_eq!(c.len, CURVE_LEN, "grid changed: regenerate with HM_WRITE_CURVE=1");
    let n = CURVE_FRAMES as f64;
    let mut bad = Vec::new();
    for (i, row) in lost.iter().enumerate() {
        for (j, &got) in row.iter().enumerate() {
            let p = c.loss[i][j];
            // Two independent measurements of the same p: their difference has
            // twice the variance of one.
            let tol = 4.0 * (2.0 * n * p * (1.0 - p)).sqrt() + 3.0;
            if (got as f64 - p * n).abs() > tol {
                bad.push(format!(
                    "{} dB, {} bytes: table {:.0}, now {got}",
                    c.snr_db[i],
                    c.len[j],
                    p * n
                ));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "the modem no longer matches its table:\n{}",
        bad.join("\n")
    );
}

/// Time from the start of a transmission to the demodulator's carrier detect,
/// in ms, at `snr_db`; `None` if it never rose.
fn dcd_latency_ms(fs: u32, snr_db: f32, seed: u64) -> Option<f64> {
    let mut rng = DetRng::from_seed(seed);
    let m = Modulator::new(fs);
    let sigma =
        (m.amplitude * m.amplitude / 2.0 * (fs as f32 / 2.0) / (3000.0 * 10f32.powf(snr_db / 10.0))).sqrt();
    let mut demod = hm_modem_afsk::Demodulator::new(hm_modem_afsk::DemodulatorConfig::new(fs));
    let mut out = Vec::new();
    let mut noise = |x: &mut Vec<f32>| x.iter_mut().for_each(|v| *v += sigma * gaussian(&mut rng));
    let mut quiet = vec![0.0f32; fs as usize / 2];
    noise(&mut quiet);
    demod.process(&quiet, &mut out);
    let mut audio = m.modulate(&[&[0x55; 60]], 300);
    noise(&mut audio);
    let chunk = fs as usize / 1000;
    for (k, c) in audio.chunks(chunk).enumerate() {
        demod.process(c, &mut out);
        if demod.dcd() {
            return Some((k + 1) as f64 * chunk as f64 * 1000.0 / fs as f64);
        }
    }
    None
}

/// The simulator's default carrier-detect delay covers the real demodulator.
#[test]
fn carrier_detect_rises_within_the_simulated_delay() {
    // The link reads audio in 20 ms chunks, so it can learn of the carrier that much later.
    let limit = hm_sim::Csma::DEFAULT.dcd_delay.0 as f64 - 20.0;
    let mut all = Vec::new();
    for snr in [20.0f32, 12.0, 9.0, 7.0] {
        for seed in 0..6 {
            let t = dcd_latency_ms(48_000, snr, seed).expect("carrier detected");
            all.push(t);
            assert!(t <= limit, "{snr} dB seed {seed}: carrier detected after {t} ms");
        }
    }
    eprintln!("carrier detect after {all:?} ms");
}
