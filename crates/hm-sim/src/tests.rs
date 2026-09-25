use super::toy::{Beacon, BeaconCmd, BeaconEvent};
use super::*;

type BeaconSim = Sim<Beacon, BeaconCmd, BeaconEvent>;

fn heard_by(sim: &BeaconSim, node: NodeId) -> Vec<(Millis, u8, u32)> {
    sim.events()
        .iter()
        .filter(|(_, n, _)| *n == node)
        .map(|(t, _, BeaconEvent::Heard { from, counter })| (*t, *from, *counter))
        .collect()
}

fn busy_network(seed: u64) -> Report {
    let mut sim = BeaconSim::new(seed, RadioParams::VHF_1200);
    for i in 0..10u8 {
        let rng = sim.machine_rng(i as u64);
        sim.add_node(Beacon::periodic(i, 40, Millis::from_secs(30), 5_000, rng));
    }
    for i in 0..10 {
        for j in (i + 1)..10 {
            // A chain with some long links: not everyone hears everyone.
            if j - i <= 3 {
                let loss = if j - i == 3 {
                    Loss::GilbertElliott {
                        p_good_to_bad: 0.1,
                        p_bad_to_good: 0.3,
                        loss_good: 0.02,
                        loss_bad: 0.7,
                    }
                } else {
                    Loss::Bernoulli(0.05)
                };
                sim.link(i, j, loss);
            }
        }
    }
    sim.set_up_at(Millis::from_secs(600), 4, false);
    sim.set_up_at(Millis::from_secs(1200), 4, true);
    sim.run_until(Millis::from_secs(3600));
    sim.report()
}

#[test]
fn same_seed_same_run() {
    let a = busy_network(1);
    let b = busy_network(1);
    assert_eq!(a, b);
    assert!(a.stats.delivered > 1000, "{:?}", a.stats);
    assert!(
        a.stats.lost_collision > 0 && a.stats.lost_channel > 0 && a.stats.lost_down > 0,
        "{:?}",
        a.stats
    );
    let c = busy_network(2);
    assert_ne!(a.trace, c.trace);
}

#[test]
fn airtime_and_delivery_time() {
    let mut sim = BeaconSim::new(0, RadioParams::VHF_1200);
    let a = sim.add_node(Beacon::silent(1, 30));
    let b = sim.add_node(Beacon::silent(2, 30));
    sim.link(a, b, Loss::None);
    // 300 ms TXDELAY + 30 bytes * 8 / 1200 bit/s = 300 + 200 ms.
    assert_eq!(RadioParams::VHF_1200.airtime(30), Millis(500));
    sim.command_at(Millis(1000), a, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(5));
    assert_eq!(heard_by(&sim, b), vec![(Millis(1500), 1, 0)]);
    assert_eq!(sim.report().nodes[a].airtime_ms, 500);
}

#[test]
fn back_to_back_frames_queue_on_the_radio() {
    let mut sim = BeaconSim::new(0, RadioParams::VHF_1200);
    let a = sim.add_node(Beacon::silent(1, 30));
    let b = sim.add_node(Beacon::silent(2, 30));
    sim.link(a, b, Loss::None);
    sim.command_at(Millis(0), a, BeaconCmd::SendNow);
    sim.command_at(Millis(0), a, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(5));
    assert_eq!(heard_by(&sim, b), vec![(Millis(500), 1, 0), (Millis(1000), 1, 1)]);
}

#[test]
fn hidden_terminals_collide_at_the_middle_station() {
    let mut sim = BeaconSim::new(0, RadioParams::VHF_1200);
    let a = sim.add_node(Beacon::silent(1, 30));
    let b = sim.add_node(Beacon::silent(2, 30));
    let c = sim.add_node(Beacon::silent(3, 30));
    sim.link(a, b, Loss::None);
    sim.link(b, c, Loss::None);
    sim.command_at(Millis(1000), a, BeaconCmd::SendNow);
    sim.command_at(Millis(1200), c, BeaconCmd::SendNow);
    // Far enough apart: no overlap.
    sim.command_at(Millis(5000), a, BeaconCmd::SendNow);
    sim.command_at(Millis(5500), c, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(10));
    assert_eq!(sim.report().stats.lost_collision, 2);
    assert_eq!(
        heard_by(&sim, b),
        vec![(Millis(5500), 1, 1), (Millis(6000), 3, 1)]
    );
}

#[test]
fn half_duplex_stations_miss_each_other() {
    let mut sim = BeaconSim::new(0, RadioParams::VHF_1200);
    let a = sim.add_node(Beacon::silent(1, 30));
    let b = sim.add_node(Beacon::silent(2, 30));
    sim.link(a, b, Loss::None);
    sim.command_at(Millis(1000), a, BeaconCmd::SendNow);
    sim.command_at(Millis(1100), b, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(5));
    let s = sim.report().stats;
    assert_eq!((s.lost_half_duplex, s.delivered), (2, 0));
}

#[test]
fn gilbert_elliott_matches_theory_and_is_bursty() {
    let (p_gb, p_bg, lg, lb) = (0.05, 0.25, 0.01, 0.8);
    let mut sim = BeaconSim::new(
        9,
        RadioParams {
            bitrate_bps: 9600,
            txdelay: Millis(10),
            phy_overhead_bytes: 0,
        },
    );
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
    let got: Vec<u32> = heard_by(&sim, b).into_iter().map(|(_, _, c)| c).collect();
    let mut lost = vec![true; sent];
    for c in &got {
        lost[*c as usize] = false;
    }
    let loss_rate = lost.iter().filter(|&&l| l).count() as f64 / sent as f64;
    let pi_bad = p_gb / (p_gb + p_bg);
    let expected = (1.0 - pi_bad) * lg + pi_bad * lb;
    assert!(
        (loss_rate - expected).abs() < 0.01,
        "loss {loss_rate:.4} vs expected {expected:.4} over {sent} frames"
    );
    let after_loss: Vec<bool> = lost.windows(2).filter(|w| w[0]).map(|w| w[1]).collect();
    let p_loss_after_loss = after_loss.iter().filter(|&&l| l).count() as f64 / after_loss.len() as f64;
    assert!(
        p_loss_after_loss > 3.0 * loss_rate,
        "not bursty: {p_loss_after_loss:.3} vs {loss_rate:.3}"
    );
}

#[test]
fn stations_that_are_down_hear_nothing() {
    let mut sim = BeaconSim::new(0, RadioParams::VHF_1200);
    let a = sim.add_node(Beacon::periodic(
        1,
        20,
        Millis::from_secs(1),
        0,
        DetRng::from_seed(1),
    ));
    let b = sim.add_node(Beacon::silent(2, 20));
    sim.link(a, b, Loss::None);
    sim.set_up_at(Millis::from_secs(10), b, false);
    sim.set_up_at(Millis::from_secs(20), b, true);
    sim.run_until(Millis::from_secs(30));
    let times: Vec<u64> = heard_by(&sim, b).iter().map(|(t, _, _)| t.0).collect();
    assert!(times.iter().all(|&t| t <= 10_000 || t >= 20_000));
    assert!(times.iter().any(|&t| t > 20_000));
    assert!(sim.report().stats.lost_down >= 9);
}

#[test]
#[should_panic(expected = "without advancing it")]
fn machines_that_never_advance_their_deadline_are_caught() {
    struct Stuck;
    impl Machine for Stuck {
        type Input = Input<BeaconCmd>;
        type Output = Output<BeaconEvent>;
        fn handle(&mut self, _: Millis, _: Self::Input, _: &mut Vec<Self::Output>) {}
        fn on_deadline(&mut self, _: Millis, _: &mut Vec<Self::Output>) {}
        fn next_deadline(&self) -> Option<Millis> {
            Some(Millis::ZERO)
        }
    }
    let mut sim: Sim<Stuck, BeaconCmd, BeaconEvent> = Sim::new(0, RadioParams::VHF_1200);
    sim.add_node(Stuck);
    sim.run_until(Millis(1));
}

/// Pins the whole simulator (RNG, airtime, collision and loss rules, event
/// order) across platforms and Rust versions. If a deliberate change to the
/// simulator moves this value, update it in the same commit and say why.
#[test]
fn busy_network_trace_is_pinned() {
    let r = busy_network(1);
    assert_eq!(
        r.trace, BUSY_TRACE,
        "trace {:#018x}, stats {:?}",
        r.trace, r.stats
    );
}

const BUSY_TRACE: u64 = 0x08f1_eae3_0538_2f27;

#[test]
#[ignore]
fn print_busy_network_report() {
    let r = busy_network(1);
    println!("{:#?}", r.stats);
    let airtime_share = r.stats.airtime_ms as f64 / r.now.0 as f64;
    println!("channel-seconds of airtime per second: {airtime_share:.3}");
}
