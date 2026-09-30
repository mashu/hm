//! Carrier sense: waiting for a clear channel, the detect delay, hidden
//! terminals.

use super::*;

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
