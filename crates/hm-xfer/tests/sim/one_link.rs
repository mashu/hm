//! One sender, one receiver: the exit criteria, loss, corruption, the
//! measured modem, latency.

use super::*;

#[test]
fn exit_criterion_1kb_at_10_percent_loss() {
    let n = trials();
    let (mut ok, mut dup) = (0u64, 0u64);
    let mut lat = Vec::new();
    let mut air = Vec::new();
    for seed in 0..n {
        let o = run(
            seed,
            1000,
            Loss::Bernoulli(0.1),
            Loss::Bernoulli(0.1),
            0.0,
            Millis::from_secs(1800),
        );
        if o.received == 1 && o.delivered {
            ok += 1;
        }
        if o.received > 1 {
            dup += 1;
        }
        if let Some(t) = o.latency {
            lat.push(t.0);
        }
        air.push(o.report.total().airtime_ms);
    }
    let rate = ok as f64 / n as f64;
    let l = Percentiles::of(&lat).unwrap();
    let a = Percentiles::of(&air).unwrap();
    eprintln!(
        "1 kB, 10% loss both ways, {n} trials: success {:.2}%, duplicates {dup}; \
         latency p50 {:.1} s p95 {:.1} s max {:.1} s; channel airtime p50 {:.1} s p95 {:.1} s",
        rate * 100.0,
        l.p50 as f64 / 1e3,
        l.p95 as f64 / 1e3,
        l.max as f64 / 1e3,
        a.p50 as f64 / 1e3,
        a.p95 as f64 / 1e3
    );
    assert_eq!(dup, 0);
    assert!(rate >= 0.99, "success {rate}");
}

/// hm's own machinery (frame headers, OFFER and ACK, key-ups) must stay under
/// 20% of airtime. The link's framing (AX.25 header, frame check, flags, bit
/// stuffing) is reported beside it: it belongs to the bearer, and IL2P or
/// another modem would change it.
#[test]
fn exit_criterion_overhead_5kb_clean() {
    let o = run(1, 5000, Loss::None, Loss::None, 0.0, Millis::from_secs(600));
    assert!(o.delivered && o.received == 1);
    let t = o.report.total();
    let air = &t.airtime;
    let total = air.total_us() as f64;
    let useful_us = 5000.0 * 8.0 * 1e6 / 1200.0;
    let share = |us: u64| us as f64 / total * 100.0;
    let machinery = (air.txdelay_us + air.overhead_us + air.control_us) as f64 / total;
    eprintln!(
        "5 kB clean: {:.1} s on air, {:.1} s to deliver; AX.25 framing and stuffing {:.1}%, \
         hm headers and preambles {:.1}%, OFFER+ACK {:.1}%, TXDELAY+TXTAIL {:.1}%, \
         symbols {:.1}% of which useful {:.1}% of all airtime",
        total / 1e6,
        o.latency.unwrap().0 as f64 / 1e3,
        share(air.link_us),
        share(air.overhead_us),
        share(air.control_us),
        share(air.txdelay_us),
        share(air.payload_us),
        useful_us / total * 100.0,
    );
    assert!(
        machinery <= 0.20,
        "hm headers, ACKs, control and key-ups take {:.1}%",
        machinery * 100.0
    );
}

#[test]
fn lost_acks_never_cause_duplicate_delivery() {
    for seed in 0..40 {
        let o = run(
            seed,
            3000,
            Loss::Bernoulli(0.05),
            Loss::Bernoulli(0.6),
            0.0,
            Millis::from_secs(3600),
        );
        assert!(o.received <= 1, "seed {seed}: delivered {} times", o.received);
        assert!(
            o.received == 1 || o.failed,
            "seed {seed}: neither delivered nor failed"
        );
    }
}

/// Undetected corruption (bit errors the modem's CRC missed) is rare in
/// practice: CRC-16 passes about 1 in 65,536 bad frames. Even at 5% of all
/// frames, nothing corrupt may ever be delivered (`run` asserts the bytes).
#[test]
fn heavy_corruption_never_delivers_bad_data() {
    let mut ok = 0;
    for seed in 0..40 {
        let o = run(
            seed,
            4000,
            Loss::Bernoulli(0.05),
            Loss::Bernoulli(0.05),
            0.05,
            Millis::from_secs(3600),
        );
        ok += (o.received == 1) as usize;
    }
    eprintln!("4 kB with 5% undetected corruption: {ok}/40 delivered, none corrupt");
}

#[test]
fn moderate_corruption_is_recovered() {
    let mut ok = 0;
    for seed in 0..60 {
        let o = run(
            seed,
            4000,
            Loss::Bernoulli(0.05),
            Loss::Bernoulli(0.05),
            0.01,
            Millis::from_secs(3600),
        );
        ok += (o.received == 1) as usize;
    }
    assert!(
        ok >= 59,
        "only {ok}/60 delivered under 5% loss and 1% undetected corruption"
    );
}

#[test]
fn bursty_loss() {
    let ge = Loss::GilbertElliott {
        p_good_to_bad: 0.05,
        p_bad_to_good: 0.3,
        loss_good: 0.02,
        loss_bad: 0.7,
    };
    let mut ok = 0;
    for seed in 0..100 {
        let o = run(seed, 2000, ge, ge, 0.0, Millis::from_secs(3600));
        ok += (o.received == 1 && o.delivered) as usize;
    }
    eprintln!("2 kB over Gilbert-Elliott (~12% mean loss, bursty): {ok}/100 delivered");
    assert!(ok >= 97);
}

/// 2 kB over links at the SNRs where the built-in modem goes from marginal to
/// clean, with its measured loss: short ACKs survive where long DATA frames do not.
#[test]
fn transfers_over_the_measured_modem() {
    let n = trials().min(100);
    let mut report = Vec::new();
    for snr in [7.0, 8.0, 9.0] {
        let (mut ok, mut lat) = (0u64, Vec::new());
        for seed in 0..n {
            let o = run(
                seed,
                2000,
                Loss::afsk_1200(snr),
                Loss::afsk_1200(snr),
                0.0,
                Millis::from_secs(3600),
            );
            assert!(o.received <= 1, "seed {seed}: duplicate delivery");
            if o.received == 1 && o.delivered {
                ok += 1;
                lat.push(o.latency.unwrap().0);
            }
        }
        let p = Percentiles::of(&lat).unwrap();
        report.push(format!(
            "{snr} dB: {ok}/{n} delivered, latency p50 {:.1} s p95 {:.1} s",
            p.p50 as f64 / 1e3,
            p.p95 as f64 / 1e3
        ));
        assert_eq!(ok, n, "{snr} dB");
    }
    eprintln!(
        "2 kB over the AFSK 1200 modem's measured loss:\n  {}",
        report.join("\n  ")
    );
}

/// An object that needs more overs than `max_rounds` still gets through:
/// only overs that bring no progress count against a transfer. 50 kB is
/// 250 symbols, 16 overs at the most symbols one over carries.
#[test]
fn an_object_needing_many_overs_is_not_abandoned() {
    let o = run(
        7,
        50_000,
        Loss::Bernoulli(0.02),
        Loss::Bernoulli(0.02),
        0.0,
        Millis::from_secs(3600),
    );
    assert!(
        o.delivered && !o.failed,
        "delivered {} failed {}",
        o.delivered,
        o.failed
    );
    assert_eq!(o.received, 1);
}

/// Time for one short chat message (a 150-byte bundle) to reach the other
/// station and for its signed receipt to come back, over one hop.
#[test]
#[ignore = "measurement: one-hop chat latency"]
fn chat_latency_one_hop() {
    let pct = |v: &mut Vec<u64>, p: usize| {
        v.sort_unstable();
        v[(v.len() - 1) * p / 100] as f64 / 1e3
    };
    for (name, loss) in [("clean", 0.0), ("10% loss", 0.1), ("30% loss", 0.3)] {
        let mut arrive = Vec::new();
        for seed in 0..200 {
            let o = run(
                seed,
                150,
                Loss::Bernoulli(loss),
                Loss::Bernoulli(loss),
                0.0,
                Millis::from_secs(600),
            );
            arrive.push(o.latency.expect("delivered").0);
        }
        eprintln!(
            "VHF 1200 bd, {name}: arrives p50 {:.1} s p95 {:.1} s",
            pct(&mut arrive, 50),
            pct(&mut arrive, 95)
        );
    }
    let hf = |me: &str| Config::hf_300(call(me));
    for snr in [17.0, 20.0, 24.0] {
        let runs: Vec<_> = (0..50).map(|s| hf_run_len(s, 150, &hf, snr, 0.5)).collect();
        let mut lat: Vec<u64> = runs.iter().filter(|r| r.0).map(|r| r.1 .0).collect();
        eprintln!(
            "HF 300 bd, 0.5 Hz fading, {snr} dB: {}/50 confirmed, receipt back p50 {:.1} s p95 {:.1} s",
            lat.len(),
            pct(&mut lat, 50),
            pct(&mut lat, 95)
        );
    }
}
