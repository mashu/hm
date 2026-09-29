//! hm-xfer in the simulator, measured against the Phase 1 exit criteria.
//!
//! `HM_XFER_TRIALS=5000 cargo test -p hm-xfer --release --test sim -- --nocapture`

use hm_core::{Millis, Output};
use hm_ident::Identity;
use hm_sim::metrics::Percentiles;
use hm_sim::{ChannelId, Csma, Loss, RadioParams, Report, Sim};
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

/// With two stations, a frame lost to half-duplex means one keyed up while the
/// other was still sending: the receiver misjudged the end of an over (or the
/// sender the end of an ACK). Long overs with lost frames are where the
/// prediction is stretched furthest.
#[test]
fn nobody_talks_over_an_over() {
    let (mut talked_over, mut delivered) = (0, 0);
    for seed in 0..40 {
        let o = run(
            seed,
            8000,
            Loss::Bernoulli(0.15),
            Loss::Bernoulli(0.15),
            0.0,
            Millis::from_secs(3600),
        );
        talked_over += o.report.total().lost_half_duplex;
        delivered += (o.received == 1 && o.delivered) as u32;
    }
    eprintln!("8 kB at 15% loss, 40 runs: {delivered} delivered, {talked_over} frames talked over");
    assert_eq!(delivered, 40);
    assert_eq!(
        talked_over, 0,
        "a station keyed up during the other's transmission"
    );
}

struct Busy {
    delivered: usize,
    last: Option<Millis>,
    collisions: u64,
    /// Overs sent per delivered object, probes included.
    rounds: f64,
}

/// `senders` stations and a hub, all hearing each other at `snr_db`. Each sender
/// has a 1.5 kB object for the hub, queued within the first 5 s.
fn busy_channel(seed: u64, senders: usize, snr_db: f64, csma: bool) -> Busy {
    let mut sim = XferSim::new(seed, RadioParams::VHF_1200);
    let names: Vec<String> = (0..senders).map(|i| format!("SA{i}KAM")).collect();
    let hub_rng = sim.machine_rng(99);
    let all: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let hub = sim.add_node(station("SO5KM-1", hub_rng, &all));
    let mut nodes = vec![hub];
    for (i, n) in names.iter().enumerate() {
        let rng = sim.machine_rng(i as u64);
        nodes.push(sim.add_node(station(n, rng, &["SO5KM-1"])));
    }
    for (i, &a) in nodes.iter().enumerate() {
        for &b in &nodes[i + 1..] {
            sim.link(a, b, Loss::afsk_1200(snr_db));
        }
    }
    if csma {
        for &n in &nodes {
            sim.set_csma(n, 0, Some(Csma::DEFAULT));
        }
    }
    let mut g = hm_core::DetRng::from_seed(seed ^ 0xB05);
    for (i, &n) in nodes[1..].iter().enumerate() {
        sim.command_at(
            Millis(g.below(5_000)),
            n,
            Command::Send {
                to: call("SO5KM-1"),
                object: object(1500, seed * 100 + i as u64),
                precedence: 0,
            },
        );
    }
    sim.run_until(Millis::from_secs(3600));
    let got: Vec<(Millis, u8)> = sim
        .events()
        .iter()
        .filter(|(_, n, _)| *n != hub)
        .filter_map(|(t, _, e)| match e {
            Event::Delivered { rounds, .. } => Some((*t, *rounds)),
            _ => None,
        })
        .collect();
    Busy {
        delivered: got.len(),
        last: got.iter().map(|g| g.0).max(),
        collisions: sim.report().total().lost_collision,
        rounds: got.iter().map(|g| g.1 as f64).sum::<f64>() / got.len().max(1) as f64,
    }
}

/// Stations that hear each other share one channel: with carrier sense they
/// mostly take turns. What collides still is two stations keying up within
/// the carrier-detect delay of each other. Overs held back for the busy
/// channel are not given up on: a 1.5 kB object needs one over, and senders
/// that probe again before their over has gone out would need several.
#[test]
fn busy_channel_with_and_without_csma() {
    let (senders, runs) = (4, 20);
    let mut line = Vec::new();
    let mut coll = [0u64; 2];
    let mut p50 = [0u64; 2];
    let mut rounds = [0f64; 2];
    for (k, csma) in [false, true].into_iter().enumerate() {
        let mut last = Vec::new();
        for seed in 0..runs {
            let b = busy_channel(seed, senders, 9.0, csma);
            assert_eq!(b.delivered, senders, "seed {seed}, csma {csma}");
            last.push(b.last.unwrap().0);
            coll[k] += b.collisions;
            rounds[k] += b.rounds / runs as f64;
        }
        let p = Percentiles::of(&last).unwrap();
        p50[k] = p.p50;
        line.push(format!(
            "{}: all delivered in {runs}/{runs}, last p50 {:.0} s max {:.0} s, {:.2} overs per object, \
             {:.1} receptions lost to collisions per run",
            if csma { "CSMA" } else { "no CSMA" },
            p.p50 as f64 / 1e3,
            p.max as f64 / 1e3,
            rounds[k],
            coll[k] as f64 / runs as f64
        ));
    }
    eprintln!(
        "{senders} stations to one hub at 9 dB, 1.5 kB each:\n  {}",
        line.join("\n  ")
    );
    assert!(
        coll[1] * 2 < coll[0],
        "carrier sense should halve collisions: {coll:?}"
    );
    assert!(p50[1] * 3 < p50[0] * 2, "and finish sooner: {p50:?}");
    assert!(rounds[1] < 2.5, "overs per object with CSMA: {:.2}", rounds[1]);
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

/// A station that does not answer holds up only the traffic to itself: while
/// that transfer backs off, the one queued behind it, to a station that does
/// answer, goes ahead instead of waiting for all its rounds to run out.
#[test]
fn a_silent_station_does_not_hold_up_the_others() {
    let mut delivered = 0;
    let mut times = Vec::new();
    for seed in 0..20 {
        let mut sim = XferSim::new(seed, RadioParams::VHF_1200);
        let [r0, r1, r2] = [sim.machine_rng(0), sim.machine_rng(1), sim.machine_rng(2)];
        let a = sim.add_node(station("SA0KAM", r0, &["SO5KM-1", "SP5DDD"]));
        let b = sim.add_node(station("SO5KM-1", r1, &["SA0KAM"]));
        let d = sim.add_node(station("SP5DDD", r2, &["SA0KAM"]));
        sim.link(a, b, Loss::Bernoulli(0.02));
        sim.link(a, d, Loss::Bernoulli(0.02));
        sim.set_up_at(Millis(0), d, false);
        for (to, len) in [("SP5DDD", 3000), ("SO5KM-1", 1000)] {
            sim.command_at(
                Millis(0),
                a,
                Command::Send {
                    to: call(to),
                    object: object(len, seed),
                    precedence: 0,
                },
            );
        }
        sim.run_until(Millis::from_secs(3600));
        let live = sim.events().iter().find_map(|(t, node, e)| match e {
            Event::Delivered { to, .. } if *node == a && *to == call("SO5KM-1") => Some(t.0),
            _ => None,
        });
        let dead_failed = sim.events().iter().find_map(|(t, node, e)| match e {
            Event::Failed { to, .. } if *node == a && *to == call("SP5DDD") => Some(t.0),
            _ => None,
        });
        if let (Some(live), Some(dead)) = (live, dead_failed) {
            assert!(
                live < dead,
                "seed {seed}: delivered at {live} ms, after giving up at {dead} ms"
            );
            delivered += 1;
            times.push(live);
        }
    }
    let p = Percentiles::of(&times).unwrap();
    eprintln!(
        "1 kB to a live station queued behind 3 kB to a silent one: delivered first in {delivered}/20; p50 {:.0} s, max {:.0} s",
        p.p50 as f64 / 1e3,
        p.max as f64 / 1e3
    );
    assert_eq!(delivered, 20);
    assert!(
        p.max < 300_000,
        "within minutes, not after the silent transfer's rounds"
    );
}

/// One 2 kB transfer on a 300 bd HF path fading at `spread` Hz around
/// `snr_db`, with `cfg` at both ends: (delivered, latency, airtime of both).
fn hf_run(seed: u64, cfg: &dyn Fn(&str) -> Config, snr_db: f64, spread: f64) -> (bool, Millis, Millis) {
    hf_run_len(seed, 2000, cfg, snr_db, spread)
}

fn hf_run_len(
    seed: u64,
    len: usize,
    cfg: &dyn Fn(&str) -> Config,
    snr_db: f64,
    spread: f64,
) -> (bool, Millis, Millis) {
    let mut sim = XferSim::new(seed, RadioParams::HF_300);
    let [ra, rb] = [sim.machine_rng(0), sim.machine_rng(1)];
    let make = |me: &str, rng, peer: &str| {
        let mut x = Xfer::new(cfg(me), identity(me), rng).unwrap();
        x.trust(call(peer), identity(peer).public());
        x
    };
    let a = sim.add_node(make("SA0KAM", ra, "SO5KM-1"));
    let b = sim.add_node(make("SO5KM-1", rb, "SA0KAM"));
    sim.link(
        a,
        b,
        Loss::Fading {
            curve: &hm_sim::afsk_1200::CURVE,
            mean_snr_db: snr_db,
            doppler_spread_hz: spread,
            rician_k: 0.0,
        },
    );
    sim.command_at(
        Millis(0),
        a,
        Command::Send {
            to: call("SO5KM-1"),
            object: object(len, seed),
            precedence: 0,
        },
    );
    sim.run_until(Millis::from_secs(3 * 3600));
    let done = sim.events().iter().find_map(|(t, n, e)| match e {
        Event::Delivered { .. } if *n == a => Some(*t),
        _ => None,
    });
    let air = Millis(sim.report().channels[0].airtime_ms);
    (done.is_some(), done.unwrap_or(Millis::ZERO), air)
}

/// On a 300 bd HF path fading with a 0.5 Hz Doppler spread (CCIR 520
/// "moderate") at 20 dB, the link sizing of [`Config::hf_300`] (64-byte
/// symbols, overs up to 60 s) delivers everything, sooner and with less
/// airtime than the VHF 1200 sizes the daemon used at 300 bd before (200-byte
/// symbols).
#[test]
fn hf_link_sizing_on_a_fading_path() {
    let hf = |me: &str| Config::hf_300(call(me));
    let vhf_sized = |me: &str| {
        let mut c = Config::vhf_1200(call(me));
        c.bitrate_bps = 300;
        c.max_over = Millis::from_secs(120);
        c
    };
    let measure = |cfg: &dyn Fn(&str) -> Config| {
        let runs: Vec<_> = (0..20).map(|s| hf_run(s, cfg, 20.0, 0.5)).collect();
        let delivered = runs.iter().filter(|r| r.0).count();
        let mut latency: Vec<u64> = runs.iter().filter(|r| r.0).map(|r| r.1 .0).collect();
        latency.sort_unstable();
        let airtime = runs.iter().map(|r| r.2 .0).sum::<u64>() / runs.len() as u64;
        (delivered, latency[latency.len() / 2], airtime)
    };
    let (hf_ok, hf_p50, hf_air) = measure(&hf);
    let (vhf_ok, vhf_p50, vhf_air) = measure(&vhf_sized);
    eprintln!(
        "2 kB at 300 bd, 0.5 Hz fading, 20 dB: hf_300 {hf_ok}/20, p50 {:.0} s, airtime {:.0} s; \
         VHF-sized {vhf_ok}/20, p50 {:.0} s, airtime {:.0} s",
        hf_p50 as f64 / 1e3,
        hf_air as f64 / 1e3,
        vhf_p50 as f64 / 1e3,
        vhf_air as f64 / 1e3
    );
    assert_eq!(hf_ok, 20);
    assert!(hf_p50 < vhf_p50 && hf_air * 10 < vhf_air * 9);
}

#[test]
#[ignore = "exploration: HF link parameters on fading channels"]
fn explore_hf_parameters_on_fading() {
    for spread in [0.1, 0.5, 1.0] {
        for snr in [17.0, 20.0, 24.0] {
            for symbol in [64u16, 128, 200] {
                for over in [20u64, 60, 120] {
                    let cfg = |me: &str| {
                        let mut c = Config::hf_300(call(me));
                        c.symbol_size = symbol;
                        c.max_over = Millis::from_secs(over);
                        c
                    };
                    let runs: Vec<_> = (0..30).map(|s| hf_run(s, &cfg, snr, spread)).collect();
                    let ok: Vec<_> = runs.iter().filter(|r| r.0).collect();
                    let mut lat: Vec<u64> = ok.iter().map(|r| r.1 .0).collect();
                    lat.sort();
                    let air: u64 = runs.iter().map(|r| r.2 .0).sum::<u64>() / runs.len() as u64;
                    eprintln!(
                        "spread {spread:>3} Hz snr {snr} dB symbol {symbol:>3} over {over:>3} s: {:>2}/30, p50 {:>5.0} s, airtime {:>4.0} s",
                        ok.len(),
                        lat.get(lat.len() / 2).copied().unwrap_or(0) as f64 / 1e3,
                        air as f64 / 1e3
                    );
                }
            }
        }
    }
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

/// A 2 kB bulletin to 10 listeners, each losing 25% of frames on its own:
/// (listeners that got it, repair requests sent, airtime).
fn bulletin_run(seed: u64, repairs: u8) -> (usize, usize, Millis) {
    let mut sim = XferSim::new(seed, RadioParams::VHF_1200);
    let make = |me: &str, rng| {
        let mut cfg = Config::vhf_1200(call(me));
        cfg.broadcast_repairs = repairs;
        Xfer::new(cfg, identity(me), rng).unwrap()
    };
    let rng = sim.machine_rng(0);
    let a = sim.add_node(make("SA0KAM", rng));
    let listeners: Vec<_> = (0..10)
        .map(|i| {
            let rng = sim.machine_rng(1 + i as u64);
            sim.add_node(make(&format!("SP{i}AAA"), rng))
        })
        .collect();
    for n in 0..=listeners.len() {
        sim.set_csma(n, 0, Some(Csma::DEFAULT));
    }
    for &l in &listeners {
        sim.link(a, l, Loss::Bernoulli(0.25));
        for &m in &listeners {
            if l < m {
                sim.link(l, m, Loss::Bernoulli(0.25));
            }
        }
    }
    sim.command_at(
        Millis(0),
        a,
        Command::Broadcast {
            object: object(2000, seed),
            precedence: 0,
        },
    );
    sim.enable_log();
    sim.run_until(Millis::from_secs(600));
    let got = sim
        .events()
        .iter()
        .filter(|(_, n, e)| *n != a && matches!(e, Event::Received { .. }))
        .count();
    let nacks = sim
        .log()
        .iter()
        .filter(|entry| match entry {
            hm_sim::LogEntry::Tx { from, data, .. } => {
                *from != a
                    && hm_wire::FrameHeader::decode(data)
                        .is_ok_and(|(h, _)| h.ftype == hm_wire::FrameType::Ack)
            }
            _ => false,
        })
        .count();
    (got, nacks, Millis(sim.report().channels[0].airtime_ms))
}

/// Listeners that miss symbols of a bulletin ask for more, a few requests
/// standing for all, and repair overs complete them: nearly every listener
/// gets it, where one publish left more than one in four without it.
#[test]
fn bulletin_repair_reaches_listeners_that_missed_symbols() {
    let runs = 30;
    let measure = |repairs: u8| {
        let r: Vec<_> = (0..runs).map(|s| bulletin_run(s, repairs)).collect();
        let got: usize = r.iter().map(|x| x.0).sum();
        let nacks: usize = r.iter().map(|x| x.1).sum();
        let air: u64 = r.iter().map(|x| x.2 .0).sum();
        (
            got as f64 / (runs as f64 * 10.0),
            nacks as f64 / runs as f64,
            air as f64 / runs as f64 / 1e3,
        )
    };
    let (once, _, once_air) = measure(0);
    let (repaired, nacks, repaired_air) = measure(3);
    eprintln!(
        "2 kB bulletin to 10 listeners at 25% loss: one publish {:.0}% ({once_air:.0} s on air); \
         with repair {:.0}% ({repaired_air:.0} s on air, {nacks:.1} requests per bulletin)",
        100.0 * once,
        100.0 * repaired
    );
    assert!(repaired > 0.97 && repaired > once, "{repaired} vs {once}");
    assert!(nacks < 8.0, "requests suppress each other: {nacks} per bulletin");
}
