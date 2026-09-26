//! hm-xfer in the simulator, measured against the Phase 1 exit criteria.
//!
//! `HM_XFER_TRIALS=5000 cargo test -p hm-xfer --release --test sim -- --nocapture`

use hm_core::{Millis, Output};
use hm_ident::Identity;
use hm_sim::metrics::Percentiles;
use hm_sim::{ChannelId, Loss, RadioParams, Report, Sim};
use hm_wire::Callsign;
use hm_xfer::{object_id, Command, Config, Event, Receipt, Xfer};

/// A fixed key per callsign; every station trusts every other one.
fn identity(me: &str) -> Identity {
    let mut secret = [0x5Au8; 32];
    secret[..8].copy_from_slice(&Callsign::parse(me).unwrap().packed().to_be_bytes());
    Identity::from_secret(secret)
}

fn station(me: &str, rng: hm_core::DetRng, peers: &[&str]) -> Xfer {
    let mut x = Xfer::new(Config::vhf_1200(call(me)), identity(me), rng).unwrap();
    for p in peers {
        x.trust(call(p), identity(p).public());
    }
    x
}

type XferSim = Sim<Xfer, Command, Event>;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn trials() -> u64 {
    std::env::var("HM_XFER_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300)
}

struct Outcome {
    received: usize,
    delivered: bool,
    failed: bool,
    latency: Option<Millis>,
    report: Report,
}

fn object(len: usize, seed: u64) -> Vec<u8> {
    let mut g = hm_core::DetRng::from_seed(seed);
    (0..len).map(|_| g.next_u64() as u8).collect()
}

/// One transfer SA0KAM -> SO5KM-1 on a single 1200 bd channel.
fn run(seed: u64, len: usize, forward: Loss, back: Loss, corrupt: f64, limit: Millis) -> Outcome {
    let mut sim = XferSim::new(seed, RadioParams::VHF_1200);
    let a_rng = sim.machine_rng(0);
    let b_rng = sim.machine_rng(1);
    let a = sim.add_node(station("SA0KAM", a_rng, &["SO5KM-1"]));
    let b = sim.add_node(station("SO5KM-1", b_rng, &["SA0KAM"]));
    sim.link_one_way(a, b, forward);
    sim.link_one_way(b, a, back);
    if corrupt > 0.0 {
        sim.set_corruption(ChannelId(0), a, b, corrupt);
    }
    let obj = object(len, seed);
    let id = object_id(&obj);
    sim.command_at(
        Millis(0),
        a,
        Command::Send {
            to: call("SO5KM-1"),
            object: obj.clone(),
            precedence: 0,
        },
    );
    sim.run_until(limit);
    let mut o = Outcome {
        received: 0,
        delivered: false,
        failed: false,
        latency: None,
        report: sim.report(),
    };
    for (t, n, e) in sim.events() {
        match e {
            Event::Received { object, id: got, .. } if *n == b => {
                assert_eq!((object, got), (&obj, &id), "seed {seed}: wrong bytes delivered");
                o.received += 1;
                o.latency = Some(*t);
            }
            Event::Delivered { receipt, .. } if *n == a => {
                assert_eq!(*receipt, Receipt::Verified, "seed {seed}: receipt not verified");
                o.delivered = true
            }
            Event::Failed { .. } if *n == a => o.failed = true,
            _ => {}
        }
    }
    let _ = Output::<Event>::Event;
    o
}

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

#[test]
fn exit_criterion_overhead_5kb_clean() {
    let o = run(1, 5000, Loss::None, Loss::None, 0.0, Millis::from_secs(600));
    assert!(o.delivered && o.received == 1);
    let t = o.report.total();
    let air = &t.airtime;
    let total = air.total_us() as f64;
    let useful_us = 5000.0 * 8.0 * 1e6 / 1200.0;
    let machinery = (air.txdelay_us + air.overhead_us + air.control_us) as f64 / total;
    eprintln!(
        "5 kB clean: {:.1} s on air, {:.1} s to deliver; headers+preambles {:.1}%, OFFER+ACK {:.1}%, \
         TXDELAY {:.1}%, symbols {:.1}% of which useful {:.1}% of all airtime",
        total / 1e6,
        o.latency.unwrap().0 as f64 / 1e3,
        air.overhead_us as f64 / total * 100.0,
        air.control_us as f64 / total * 100.0,
        air.txdelay_us as f64 / total * 100.0,
        air.payload_us as f64 / total * 100.0,
        useful_us / total * 100.0,
    );
    assert!(
        machinery <= 0.20,
        "headers, ACKs, control and TXDELAY take {:.1}%",
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

#[test]
fn two_senders_to_one_node_without_csma() {
    // No carrier sense yet: the senders may collide. Randomised ACK timeouts
    // must still get both objects through.
    let mut both = 0;
    let mut times = Vec::new();
    for seed in 0..30 {
        let mut sim = XferSim::new(seed, RadioParams::VHF_1200);
        let r = [sim.machine_rng(0), sim.machine_rng(1), sim.machine_rng(2)];
        let [r0, r1, r2] = r;
        let a = sim.add_node(station("SA0KAM", r0, &["SO5KM-1"]));
        let c = sim.add_node(station("SA0KAM-2", r1, &["SO5KM-1"]));
        let n = sim.add_node(station("SO5KM-1", r2, &[]));
        sim.link(a, n, Loss::Bernoulli(0.02));
        sim.link(c, n, Loss::Bernoulli(0.02));
        // a and c are hidden from each other.
        sim.command_at(
            Millis(0),
            a,
            Command::Send {
                to: call("SO5KM-1"),
                object: object(1500, seed),
                precedence: 0,
            },
        );
        sim.command_at(
            Millis(0),
            c,
            Command::Send {
                to: call("SO5KM-1"),
                object: object(1500, seed + 1000),
                precedence: 0,
            },
        );
        sim.run_until(Millis::from_secs(3600));
        let got: Vec<u64> = sim
            .events()
            .iter()
            .filter(|(_, node, e)| *node == n && matches!(e, Event::Received { .. }))
            .map(|(t, _, _)| t.0)
            .collect();
        if got.len() == 2 {
            both += 1;
            times.push(*got.iter().max().unwrap());
        }
    }
    let p = Percentiles::of(&times).unwrap();
    eprintln!(
        "two hidden senders, 1.5 kB each: both delivered in {both}/30; last delivery p50 {:.0} s, max {:.0} s",
        p.p50 as f64 / 1e3,
        p.max as f64 / 1e3
    );
    assert_eq!(both, 30);
}
