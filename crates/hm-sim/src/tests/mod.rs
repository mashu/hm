use super::toy::{Beacon, BeaconCmd, BeaconEvent};
use super::*;
use crate::radio::stuffed_bits;
use hm_wire::{Ack, Callsign, Dest};
use hm_wire::{FrameHeader, FrameType};

mod air;
mod clocks;
mod csma;
mod loss;
mod runs;

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
