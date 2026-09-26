use super::toy::{Beacon, BeaconCmd, BeaconEvent};
use super::*;
use hm_wire::{Ack, Callsign, Dest};

type BeaconSim = Sim<Beacon, BeaconCmd, BeaconEvent>;

/// 1200 bit/s with 300 ms TXDELAY and nothing else, for exact timings.
const PLAIN: RadioParams = RadioParams::raw(1200, Millis(300));

fn heard_by(sim: &BeaconSim, node: NodeId) -> Vec<(Millis, Port, u8, u32)> {
    sim.events()
        .iter()
        .filter(|(_, n, _)| *n == node)
        .map(|(t, _, BeaconEvent::Heard { port, from, counter })| (*t, *port, *from, *counter))
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
fn airtime_and_delivery_time() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 30));
    let b = sim.add_node(Beacon::silent(2, 30));
    sim.link(a, b, Loss::None);
    // 300 ms TXDELAY + 30 bytes * 8 / 1200 bit/s = 300 + 200 ms.
    assert_eq!(PLAIN.airtime(30), Millis(500));
    sim.command_at(Millis(1000), a, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(5));
    assert_eq!(heard_by(&sim, b), vec![(Millis(1500), 0, 1, 0)]);
    assert_eq!(sim.report().nodes[a].airtime_ms, 500);
}

#[test]
fn hdlc_framing_adds_overhead_stuffing_and_tail() {
    let p = RadioParams::VHF_1200;
    // 0x00 never stuffs; 0xFF stuffs after every fifth 1 bit.
    assert_eq!(stuffed_bits(&[0x00; 50]), 0);
    assert_eq!(stuffed_bits(&[0xFF; 5]), 8);
    assert_eq!(stuffed_bits(&[0x1F, 0xF8]), 2); // a run of five at each end
    assert_eq!(stuffed_bits(&[0xF0, 0x01]), 1); // four 1s, then the fifth in the next byte
                                                // 30 bytes + 19 overhead = 392 bits = 326.7 ms, plus 300 ms TXDELAY and 14 ms TXTAIL.
    assert_eq!(p.airtime_of(&[0; 30], true), Millis(300 + 14 + 327));
    assert_eq!(p.airtime_of(&[0; 30], false), Millis(327));
    // 30 bytes of 0xFF: 48 stuffed bits = 40 ms more.
    assert_eq!(p.airtime_of(&[0xFF; 30], false), Millis(367));
    assert_eq!(p.airtime(30), p.airtime_of(&[0; 30], true));

    let mut sim = BeaconSim::new(0, p);
    let a = sim.add_node(Beacon::silent(1, 10));
    sim.command_at(Millis(0), a, BeaconCmd::SendRaw(0, vec![0xFF; 30]));
    sim.command_at(Millis(0), a, BeaconCmd::SendRaw(0, vec![0x00; 30]));
    sim.run_until(Millis::from_secs(5));
    let air = sim.report().total().airtime;
    assert_eq!(air.txdelay_us, 314_000, "TXDELAY and TXTAIL once per key-up");
    // 19 bytes of link overhead each and 48 stuffed bits: 200 + 152 bits at
    // 833.33 µs, rounded down per frame. No hm headers in these frames.
    assert_eq!(air.link_us, 166_666 + 126_666);
    assert_eq!(air.overhead_us, 0);
    assert_eq!(air.unknown_us, 400_000);
}

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

/// A sends a 300-byte frame (about 2.6 s); B, with CSMA, asks to send while
/// A is on air. C hears both.
fn csma_pair(b_asks_at: Millis, b_hears_a: bool) -> (BeaconSim, NodeId, NodeId, NodeId) {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 300));
    let b = sim.add_node(Beacon::silent(2, 30));
    let c = sim.add_node(Beacon::silent(3, 30));
    if b_hears_a {
        sim.link(a, b, Loss::None);
    }
    sim.link(a, c, Loss::None);
    sim.link(b, c, Loss::None);
    // Always key up on a clear slot, so the timing is exact.
    let csma = Csma {
        persist: 255,
        ..Csma::DEFAULT
    };
    sim.set_csma(b, 0, Some(csma));
    sim.command_at(Millis(1000), a, BeaconCmd::SendNow);
    sim.command_at(b_asks_at, b, BeaconCmd::SendNow);
    sim.command_at(b_asks_at, b, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(20));
    (sim, a, b, c)
}

#[test]
fn csma_waits_for_the_channel_and_sends_in_one_key_up() {
    // A is on air from 1.0 s to 3.3 s (300 ms + 2000 ms). B asks at 1.5 s, hears
    // the carrier, and tries each 100 ms slot: 1.5, 1.6, ... 3.3 is the first clear one.
    let (sim, _, _, c) = csma_pair(Millis(1500), true);
    let a_end = Millis(1000) + PLAIN.airtime(300);
    assert_eq!(a_end, Millis(3300));
    let t = PLAIN.airtime(30);
    assert_eq!(
        heard_by(&sim, c),
        vec![
            (a_end, 0, 1, 0),
            // Both of B's frames in one key-up: only one TXDELAY.
            (a_end + t, 0, 2, 0),
            (a_end + t + PLAIN.airtime_keyed(30), 0, 2, 1),
        ]
    );
    assert_eq!(sim.report().total().lost_collision, 0);
}

#[test]
fn csma_does_not_hear_a_carrier_younger_than_the_detect_delay() {
    // B asks 50 ms after A keyed up: no carrier detected yet, so B keys up and
    // both frames are lost at C.
    let (sim, _, _, c) = csma_pair(Millis(1050), true);
    assert!(heard_by(&sim, c).is_empty());
    assert_eq!(sim.report().total().lost_collision, 3);
}

#[test]
fn csma_cannot_help_hidden_terminals() {
    let (sim, _, _, c) = csma_pair(Millis(1500), false);
    assert!(heard_by(&sim, c).is_empty());
    assert_eq!(sim.report().total().lost_collision, 3);
}

#[test]
fn csma_frames_waiting_are_dropped_when_the_station_goes_down() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 300));
    let b = sim.add_node(Beacon::silent(2, 30));
    sim.link(a, b, Loss::None);
    sim.set_csma(b, 0, Some(Csma::DEFAULT));
    sim.command_at(Millis(0), a, BeaconCmd::SendNow);
    sim.command_at(Millis(500), b, BeaconCmd::SendNow);
    sim.set_up_at(Millis(1000), b, false);
    sim.set_up_at(Millis(1100), b, true);
    sim.run_until(Millis::from_secs(20));
    assert_eq!(sim.report().nodes[b].frames_sent, 0);
}

#[test]
fn csma_switched_off_while_waiting_sends_at_once() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 300));
    let b = sim.add_node(Beacon::silent(2, 30));
    sim.link(a, b, Loss::None);
    sim.set_csma(b, 0, Some(Csma::DEFAULT));
    sim.command_at(Millis(0), a, BeaconCmd::SendNow);
    sim.command_at(Millis(500), b, BeaconCmd::SendNow);
    sim.run_until(Millis(1000));
    assert_eq!(sim.report().nodes[b].frames_sent, 0, "waiting for A to finish");
    sim.set_csma(b, 0, None);
    sim.run_until(Millis(1200));
    assert_eq!(sim.report().nodes[b].frames_sent, 1);
}

#[test]
fn back_to_back_frames_queue_on_the_radio() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 30));
    let b = sim.add_node(Beacon::silent(2, 30));
    sim.link(a, b, Loss::None);
    sim.command_at(Millis(0), a, BeaconCmd::SendNow);
    sim.command_at(Millis(0), a, BeaconCmd::SendNow);
    // Queued 2 s later, after the radio went idle: keys up again.
    sim.command_at(Millis(2000), a, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(5));
    // The second frame follows the first with PTT still keyed: no second TXDELAY.
    assert_eq!(
        heard_by(&sim, b),
        vec![
            (Millis(500), 0, 1, 0),
            (Millis(700), 0, 1, 1),
            (Millis(2500), 0, 1, 2)
        ]
    );
}

#[test]
fn hidden_terminals_collide_at_the_middle_station() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 30));
    let b = sim.add_node(Beacon::silent(2, 30));
    let c = sim.add_node(Beacon::silent(3, 30));
    sim.link(a, b, Loss::None);
    sim.link(b, c, Loss::None);
    sim.command_at(Millis(1000), a, BeaconCmd::SendNow);
    sim.command_at(Millis(1200), c, BeaconCmd::SendNow);
    sim.command_at(Millis(5000), a, BeaconCmd::SendNow);
    sim.command_at(Millis(5500), c, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(10));
    assert_eq!(sim.report().total().lost_collision, 2);
    assert_eq!(
        heard_by(&sim, b),
        vec![(Millis(5500), 0, 1, 1), (Millis(6000), 0, 3, 1)]
    );
}

#[test]
fn half_duplex_stations_miss_each_other() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::silent(1, 30));
    let b = sim.add_node(Beacon::silent(2, 30));
    sim.link(a, b, Loss::None);
    sim.command_at(Millis(1000), a, BeaconCmd::SendNow);
    sim.command_at(Millis(1100), b, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(5));
    let s = sim.report().total();
    assert_eq!((s.lost_half_duplex, s.delivered), (2, 0));
}

#[test]
fn channels_are_isolated_and_radios_independent() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let hf = sim.add_channel(RadioParams::raw(600, Millis(100)));
    let node = sim.add_node_on(
        Beacon::silent(1, 30).on_ports(&[0, 1]),
        &[(0, ChannelId(0)), (1, hf)],
    );
    let vhf_peer = sim.add_node(Beacon::silent(2, 30));
    let hf_peer = sim.add_node_on(Beacon::silent(3, 30), &[(0, hf)]);
    sim.link(node, vhf_peer, Loss::None);
    sim.link_on(hf, node, hf_peer, Loss::None);
    // Node's first beacon goes out on VHF (port 0), busy 0..500 ms.
    sim.command_at(Millis(0), node, BeaconCmd::SendNow);
    // HF peer transmits meanwhile (HF airtime 100 + 400 ms): the node's idle HF radio hears it.
    sim.command_at(Millis(100), hf_peer, BeaconCmd::SendNow);
    // Node's second beacon goes out on HF (port 1).
    sim.command_at(Millis(3000), node, BeaconCmd::SendNow);
    // Simultaneous traffic on the two channels never collides.
    sim.command_at(Millis(6000), vhf_peer, BeaconCmd::SendNow);
    sim.command_at(Millis(6000), hf_peer, BeaconCmd::SendNow);
    sim.run_until(Millis::from_secs(10));
    assert_eq!(heard_by(&sim, vhf_peer), vec![(Millis(500), 0, 1, 0)]);
    assert_eq!(heard_by(&sim, hf_peer), vec![(Millis(3500), 0, 1, 1)]);
    assert_eq!(
        heard_by(&sim, node),
        vec![
            (Millis(600), 1, 3, 0),
            (Millis(6500), 0, 2, 0),
            (Millis(6500), 1, 3, 1)
        ]
    );
    let r = sim.report();
    for ch in &r.channels {
        assert_eq!((ch.lost_half_duplex, ch.lost_collision), (0, 0));
    }
}

#[test]
fn clock_inverse_is_exact() {
    let mut rng = DetRng::from_seed(5);
    for _ in 0..20_000 {
        let c = Clock {
            offset: Millis(rng.below(100_000)),
            ppm: rng.below(20_001) as i32 - 10_000,
        };
        let local = Millis(rng.below(10_000_000_000));
        let g = c.global_for(local);
        assert!(c.local(g) >= local, "{c:?} {local:?} -> {g:?}");
        if g.0 > 0 {
            assert!(
                c.local(Millis(g.0 - 1)) < local,
                "{c:?} {local:?} -> {g:?} not minimal"
            );
        }
    }
}

#[test]
fn drifting_clocks_shift_timers() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let fast = sim.add_node(Beacon::periodic(1, 10, Millis(1000), 0, DetRng::from_seed(1)));
    let slow = sim.add_node(Beacon::periodic(2, 10, Millis(1000), 0, DetRng::from_seed(1)));
    sim.set_clock(
        fast,
        Clock {
            offset: Millis::ZERO,
            ppm: 1000,
        },
    );
    sim.set_clock(
        slow,
        Clock {
            offset: Millis::ZERO,
            ppm: -1000,
        },
    );
    sim.run_until(Millis::from_secs(1000));
    // 1000 global seconds are 1001 s on the fast clock and 999 s on the slow one.
    assert_eq!(sim.node(fast).sent(), 1001);
    assert_eq!(sim.node(slow).sent(), 999);
}

#[test]
fn clock_offset_delays_first_deadline() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::periodic(1, 10, Millis(5000), 0, DetRng::from_seed(1)));
    let b = sim.add_node(Beacon::silent(2, 10));
    sim.link(a, b, Loss::None);
    // Clock already reads 3 s at global 0, so the first beacon (local 5 s) goes at global 2 s.
    sim.set_clock(
        a,
        Clock {
            offset: Millis(3000),
            ppm: 0,
        },
    );
    sim.run_until(Millis(4000));
    let t = PLAIN.airtime(10);
    assert_eq!(heard_by(&sim, b), vec![(Millis(2000) + t, 0, 1, 0)]);
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
fn partitions_cut_links_until_healed() {
    let mut sim = BeaconSim::new(0, PLAIN);
    let a = sim.add_node(Beacon::periodic(1, 10, Millis(1000), 0, DetRng::from_seed(1)));
    let b = sim.add_node(Beacon::silent(2, 10));
    let c = sim.add_node(Beacon::silent(3, 10));
    sim.link(a, b, Loss::None);
    sim.link(a, c, Loss::None);
    sim.partition_at(Millis::from_secs(10), ChannelId(0), &[a], &[b]);
    sim.heal_at(Millis::from_secs(20), ChannelId(0), &[a], &[b]);
    sim.run_until(Millis::from_secs(30));
    let b_times: Vec<u64> = heard_by(&sim, b).iter().map(|h| h.0 .0).collect();
    assert!(
        b_times.iter().all(|&t| !(10_000..20_000).contains(&t)),
        "{b_times:?}"
    );
    assert!(b_times.iter().any(|&t| t > 20_000));
    assert!(heard_by(&sim, c).len() > heard_by(&sim, b).len());
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
fn stations_that_are_down_hear_nothing() {
    let mut sim = BeaconSim::new(0, PLAIN);
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
    let times: Vec<u64> = heard_by(&sim, b).iter().map(|h| h.0 .0).collect();
    assert!(times.iter().all(|&t| t <= 10_000 || t >= 20_000));
    assert!(times.iter().any(|&t| t > 20_000));
    assert!(sim.report().total().lost_down >= 9);
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
