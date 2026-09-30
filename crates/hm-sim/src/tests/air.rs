//! Frames on the air: airtime, HDLC framing, queueing on the radio,
//! collisions, half duplex, channels, stations that are down.

use super::*;

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
