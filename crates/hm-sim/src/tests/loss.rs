//! How frames are lost: measured modem loss, corruption, band openings,
//! Gilbert-Elliott bursts, Watterson fading.

use super::*;

#[test]
fn measured_modem_loss_grows_with_frame_length() {
    // Beacons of 20 and 300 bytes at 7 dB SNR, each to its own receiver so
    // they never collide: the long one is lost far more often.
    let mut sim = BeaconSim::new(4, RadioParams::VHF_1200);
    let mut lost = Vec::new();
    for (i, len) in [(1u8, 20usize), (2, 300)] {
        let tx = sim.add_node(Beacon::periodic(
            i,
            len,
            Millis::from_secs(10),
            3_000,
            DetRng::from_seed(i as u64),
        ));
        let rx = sim.add_node(Beacon::silent(10 + i, 10));
        sim.link_one_way(tx, rx, Loss::afsk_1200(7.0));
        lost.push((tx, rx, len));
    }
    sim.run_until(Millis::from_secs(20_000));
    let share = |(tx, rx, _): (NodeId, NodeId, usize)| {
        1.0 - heard_by(&sim, rx).len() as f64 / sim.report().nodes[tx].frames_sent as f64
    };
    let (short, long) = (share(lost[0]), share(lost[1]));
    // On air: the frame plus 19 bytes of AX.25 framing.
    let (want_short, want_long) = (afsk_1200::CURVE.loss(7.0, 39), afsk_1200::CURVE.loss(7.0, 319));
    assert!(
        (short - want_short).abs() < 0.03,
        "short: {short} vs {want_short}"
    );
    assert!((long - want_long).abs() < 0.05, "long: {long} vs {want_long}");
    assert!(long > 3.0 * short);
}

#[test]
fn corrupted_frames_are_delivered_with_bit_errors() {
    let mut sim = BeaconSim::new(3, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 40));
    let b = sim.add_node(Beacon::silent(2, 40));
    sim.link(a, b, Loss::None);
    sim.set_corruption(ChannelId(0), a, b, 1.0);
    sim.enable_log();
    for i in 0..50 {
        sim.command_at(Millis::from_secs(i), a, BeaconCmd::SendNow);
    }
    sim.run_until(Millis::from_secs(60));
    let s = sim.report().total();
    assert_eq!((s.corrupted, s.delivered), (50, 0));
    let mut tx_digest = std::collections::BTreeMap::new();
    for e in sim.log() {
        match e {
            LogEntry::Tx { id, digest, .. } => {
                tx_digest.insert(*id, *digest);
            }
            LogEntry::Rx {
                tx, digest, outcome, ..
            } => {
                assert_eq!(*outcome, Outcome::Corrupted);
                assert_ne!(tx_digest[tx], *digest, "corruption must change the bytes");
            }
            _ => {}
        }
    }
}

#[test]
fn hourly_loss_models_band_openings() {
    let mut open_afternoons = [1.0; 24];
    for h in open_afternoons.iter_mut().skip(12) {
        *h = 0.0;
    }
    let mut sim = BeaconSim::new(0, RadioParams::raw(300, Millis(50)));
    let a = sim.add_node(Beacon::periodic(
        1,
        10,
        Millis::from_secs(60),
        0,
        DetRng::from_seed(1),
    ));
    let b = sim.add_node(Beacon::silent(2, 10));
    sim.link_one_way(a, b, Loss::Hourly(open_afternoons));
    sim.run_until(Millis::from_secs(48 * 3600));
    let hours: Vec<u64> = heard_by(&sim, b)
        .iter()
        .map(|h| (h.0 .0 / 3_600_000) % 24)
        .collect();
    assert!(!hours.is_empty() && hours.iter().all(|&h| h >= 12));
    // Every beacon sent in open hours arrives: 2 days x 12 h x 60 per hour.
    assert_eq!(hours.len(), 2 * 12 * 60);
}

#[test]
fn gilbert_elliott_matches_theory_and_is_bursty() {
    let (p_gb, p_bg, lg, lb) = (0.05, 0.25, 0.01, 0.8);
    let mut sim = BeaconSim::new(9, RadioParams::raw(9600, Millis(10)));
    let a = sim.add_node(Beacon::periodic(1, 10, Millis(100), 0, DetRng::from_seed(1)));
    let b = sim.add_node(Beacon::silent(2, 10));
    sim.link_one_way(
        a,
        b,
        Loss::GilbertElliott {
            p_good_to_bad: p_gb,
            p_bad_to_good: p_bg,
            loss_good: lg,
            loss_bad: lb,
        },
    );
    sim.run_until(Millis::from_secs(20_000));
    let sent = sim.report().nodes[a].frames_sent as usize;
    let mut lost = vec![true; sent];
    for (_, _, _, c) in heard_by(&sim, b) {
        lost[c as usize] = false;
    }
    let loss_rate = lost.iter().filter(|&&l| l).count() as f64 / sent as f64;
    let pi_bad = p_gb / (p_gb + p_bg);
    let expected = (1.0 - pi_bad) * lg + pi_bad * lb;
    assert!(
        (loss_rate - expected).abs() < 0.01,
        "loss {loss_rate:.4} vs {expected:.4} over {sent} frames"
    );
    let after_loss: Vec<bool> = lost.windows(2).filter(|w| w[0]).map(|w| w[1]).collect();
    let p_loss_after_loss = after_loss.iter().filter(|&&l| l).count() as f64 / after_loss.len() as f64;
    assert!(
        p_loss_after_loss > 3.0 * loss_rate,
        "not bursty: {p_loss_after_loss:.3} vs {loss_rate:.3}"
    );
}

#[test]
fn the_fading_gain_has_unit_power_and_a_gaussian_autocorrelation() {
    let mut rng = DetRng::from_seed(3);
    let spread = 0.5; // CCIR 520 "moderate"
    let paths: Vec<Fade> = (0..5_000).map(|_| Fade::new(&mut rng, spread)).collect();
    let at = |lag_ms: u64| -> Vec<f64> {
        paths
            .iter()
            .enumerate()
            .map(|(i, f)| f.power(Millis(i as u64 * 7_919 + lag_ms), 0.0))
            .collect()
    };
    // Unit mean power, and Rayleigh's deep fades: P[power < 0.1] = 1 - e^-0.1.
    let p0 = at(0);
    let mean = p0.iter().sum::<f64>() / p0.len() as f64;
    assert!((mean - 1.0).abs() < 0.05, "mean power {mean}");
    let deep = p0.iter().filter(|&&p| p < 0.1).count() as f64 / p0.len() as f64;
    assert!((deep - (1.0 - (-0.1f64).exp())).abs() < 0.02, "deep fades {deep}");
    // Power correlation across a lag: exp(-4 pi^2 sigma^2 lag^2) for Rayleigh
    // fading with a Gaussian Doppler spectrum of standard deviation sigma.
    let corr = |lag_ms: u64| {
        let q = at(lag_ms);
        let mq = q.iter().sum::<f64>() / q.len() as f64;
        let cov: f64 = p0.iter().zip(&q).map(|(a, b)| (a - mean) * (b - mq)).sum();
        let va: f64 = p0.iter().map(|a| (a - mean).powi(2)).sum();
        let vb: f64 = q.iter().map(|b| (b - mq).powi(2)).sum();
        cov / (va * vb).sqrt()
    };
    let theory = |lag_ms: u64| {
        let (sigma, lag) = (spread / 2.0, lag_ms as f64 / 1000.0);
        (-4.0 * std::f64::consts::PI.powi(2) * sigma * sigma * lag * lag).exp()
    };
    for lag in [50, 500, 1_000, 2_000, 5_000] {
        let (got, want) = (corr(lag), theory(lag));
        assert!((got - want).abs() < 0.08, "lag {lag} ms: {got:.3} vs {want:.3}");
    }
    // A strong steady part (large Rician factor) holds the power near 1.
    for (i, f) in paths.iter().enumerate().take(500) {
        assert!((f.power(Millis(i as u64 * 1_000), 1_000.0) - 1.0).abs() < 0.3);
    }
}

fn fading(mean_snr_db: f64, doppler_spread_hz: f64) -> Loss {
    Loss::Fading {
        curve: &afsk_1200::CURVE,
        mean_snr_db,
        doppler_spread_hz,
        rician_k: 0.0,
    }
}

/// Frames lost on a fading path, by counter.
fn lost_frames(sim: &BeaconSim, tx: NodeId, rx: NodeId, from: u8) -> Vec<bool> {
    let sent = sim.report().nodes[tx].frames_sent as usize;
    let mut lost = vec![true; sent];
    for (_, _, f, c) in heard_by(sim, rx) {
        if f == from {
            lost[c as usize] = false;
        }
    }
    lost
}

/// On a Rayleigh-fading path the loss is well above what the mean SNR alone
/// gives, and it comes in bursts as long as a fade.
#[test]
fn fading_loses_frames_in_bursts() {
    let mut sim = BeaconSim::new(11, RadioParams::VHF_1200);
    let a = sim.add_node(Beacon::periodic(1, 60, Millis(1_000), 0, DetRng::from_seed(1)));
    let b = sim.add_node(Beacon::silent(2, 10));
    sim.link_one_way(a, b, fading(14.0, 0.2));
    sim.run_until(Millis::from_secs(20_000));
    let lost = lost_frames(&sim, a, b, 1);
    let rate = lost.iter().filter(|&&l| l).count() as f64 / lost.len() as f64;
    let steady = afsk_1200::CURVE.loss(14.0, 60 + 19);
    assert!(
        rate > steady + 0.05 && rate < 0.5,
        "loss {rate:.3}, steady {steady:.3}"
    );
    let after_loss: Vec<bool> = lost.windows(2).filter(|w| w[0]).map(|w| w[1]).collect();
    let p = after_loss.iter().filter(|&&l| l).count() as f64 / after_loss.len() as f64;
    assert!(p > 2.0 * rate, "not bursty: {p:.3} after a loss vs {rate:.3}");
}

/// Both directions share the fade: a frame lost one way means the answer a
/// second later is likely lost too.
#[test]
fn fading_is_shared_by_both_directions() {
    let mut sim = BeaconSim::new(12, RadioParams::VHF_1200);
    let a = sim.add_node(Beacon::periodic(1, 60, Millis(2_000), 0, DetRng::from_seed(1)));
    let b = sim.add_node(Beacon::silent(2, 60));
    sim.link(a, b, fading(12.0, 0.05));
    let rounds = 5_000u64;
    for i in 0..rounds {
        sim.command_at(Millis(3_000 + 2_000 * i), b, BeaconCmd::SendNow);
    }
    sim.run_until(Millis(2_000 * rounds + 1_000));
    let (ab, ba) = (lost_frames(&sim, a, b, 1), lost_frames(&sim, b, a, 2));
    let n = ab.len().min(ba.len());
    let p_ba = ba[..n].iter().filter(|&&l| l).count() as f64 / n as f64;
    let both = (0..n).filter(|&i| ab[i] && ba[i]).count() as f64;
    let p_ba_given_ab = both / (0..n).filter(|&i| ab[i]).count() as f64;
    assert!(
        p_ba_given_ab > 2.0 * p_ba,
        "{p_ba_given_ab:.3} after a loss the other way vs {p_ba:.3}"
    );
}
