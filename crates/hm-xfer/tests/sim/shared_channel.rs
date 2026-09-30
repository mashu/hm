//! Several stations on one channel: hidden senders, carrier sense, nobody
//! talking over an over, a silent station.

use super::*;

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
