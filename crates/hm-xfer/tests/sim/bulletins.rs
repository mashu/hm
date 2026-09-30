//! Bulletins to many listeners, with repair.

use super::*;

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
