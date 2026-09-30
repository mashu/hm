//! Whole runs: the same seed gives the same run, a pinned trace, the report.

use super::*;

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
    let t = a.total();
    assert!(t.delivered > 1000, "{t:?}");
    assert!(
        t.lost_collision > 0 && t.lost_channel > 0 && t.lost_down > 0,
        "{t:?}"
    );
    assert_ne!(a.trace, busy_network(2).trace);
}

/// Pins the whole simulator (RNG, airtime, collision and loss rules, event
/// order) across platforms and Rust versions. If a deliberate change to the
/// simulator moves this value, update it in the same commit and say why.
/// Last change: the network runs on `VHF_1200` with AX.25 overhead, bit
/// stuffing and TXTAIL (Phase 1, sim framing).
#[test]
fn busy_network_trace_is_pinned() {
    let r = busy_network(1);
    assert_eq!(
        r.trace,
        BUSY_TRACE,
        "trace {:#018x}, stats {:?}",
        r.trace,
        r.total()
    );
}

const BUSY_TRACE: u64 = 0x2869_a242_a9ea_1370;

#[test]
#[ignore]
fn print_busy_network_report() {
    let r = busy_network(1);
    let t = r.total();
    println!("{t:#?}");
    println!("channel occupancy: {:.3}", t.airtime_ms as f64 / r.now.0 as f64);
}

#[test]
fn airtime_is_split_by_purpose() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 10));
    let h = FrameHeader {
        ftype: FrameType::Data,
        src: Callsign::parse("SA0KAM").unwrap(),
        dst: Dest::Station(Callsign::parse("SO5KM").unwrap()),
        session: 1,
        index: 0,
    };
    let data = h.frame(&[0xAA; 100]).unwrap();
    let ack = FrameHeader {
        ftype: FrameType::Ack,
        ..h
    }
    .frame(&Ack::default().to_vec().unwrap())
    .unwrap();
    sim.command_at(Millis(0), a, BeaconCmd::SendRaw(0, data));
    sim.command_at(Millis(0), a, BeaconCmd::SendRaw(0, ack));
    sim.command_at(Millis(0), a, BeaconCmd::SendRaw(0, vec![0xFF; 12]));
    sim.run_until(Millis::from_secs(10));
    let air = sim.report().total().airtime;
    // At 1200 bit/s one byte takes 6666.67 µs. All three frames go out in one
    // key-up, so TXDELAY is paid once.
    assert_eq!(air.txdelay_us, 300_000);
    assert_eq!(air.overhead_us, 266_666); // 18 + 4 bytes of DATA header and preamble, 18 of ACK header
    assert_eq!(air.payload_us, 640_000); // 96 bytes of symbol
    assert_eq!(air.control_us, 46_666); // 7-byte ACK body
    assert_eq!(air.unknown_us, 80_000); // 12 unrecognised bytes
    assert!(
        air.payload_share() > 0.45 && air.payload_share() < 0.5,
        "{}",
        air.payload_share()
    );
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
    let mut sim: Sim<Stuck, BeaconCmd, BeaconEvent> = Sim::new(0, PLAIN);
    sim.add_node(Stuck);
    sim.run_until(Millis(1));
}

#[test]
#[should_panic(expected = "has no radio")]
fn transmitting_on_a_missing_port_is_a_setup_error() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 10));
    sim.command_at(Millis(0), a, BeaconCmd::SendRaw(7, vec![1, 2, 3]));
    sim.run_until(Millis(1));
}
