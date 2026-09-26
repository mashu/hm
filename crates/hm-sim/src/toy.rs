//! A minimal beacon machine for exercising the simulator.
//!
//! Frame: `[station id (1), counter (4, big-endian), zero padding]`.
//! Periodic beacons rotate over the station's ports.

use hm_core::{DetRng, Input, Machine, Millis, Output, Port};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BeaconCmd {
    /// Send a beacon now, on the next port in rotation.
    SendNow,
    /// Transmit these exact bytes on this port.
    SendRaw(Port, Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BeaconEvent {
    Heard { port: Port, from: u8, counter: u32 },
}

pub struct Beacon {
    id: u8,
    frame_len: usize,
    ports: Vec<Port>,
    next_port: usize,
    interval: Option<Millis>,
    jitter_ms: u64,
    rng: DetRng,
    next: Millis,
    counter: u32,
}

impl Beacon {
    /// Beacons every `interval` plus a uniform random `0..=jitter_ms` (local time).
    pub fn periodic(id: u8, frame_len: usize, interval: Millis, jitter_ms: u64, mut rng: DetRng) -> Beacon {
        let first = Millis(interval.0 + rng.below(jitter_ms + 1));
        Beacon {
            id,
            frame_len: frame_len.max(5),
            ports: vec![0],
            next_port: 0,
            interval: Some(interval),
            jitter_ms,
            rng,
            next: first,
            counter: 0,
        }
    }

    /// Transmits only on command.
    pub fn silent(id: u8, frame_len: usize) -> Beacon {
        Beacon {
            id,
            frame_len: frame_len.max(5),
            ports: vec![0],
            next_port: 0,
            interval: None,
            jitter_ms: 0,
            rng: DetRng::from_seed(0),
            next: Millis::ZERO,
            counter: 0,
        }
    }

    /// Rotate beacons over these ports instead of only port 0.
    pub fn on_ports(mut self, ports: &[Port]) -> Beacon {
        assert!(!ports.is_empty());
        self.ports = ports.to_vec();
        self
    }

    pub fn sent(&self) -> u32 {
        self.counter
    }

    fn beacon(&mut self) -> Output<BeaconEvent> {
        let mut data = vec![0u8; self.frame_len];
        data[0] = self.id;
        data[1..5].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        let port = self.ports[self.next_port % self.ports.len()];
        self.next_port += 1;
        Output::Transmit { port, data }
    }
}

impl Machine for Beacon {
    type Input = Input<BeaconCmd>;
    type Output = Output<BeaconEvent>;

    fn handle(&mut self, _now: Millis, input: Self::Input, out: &mut Vec<Self::Output>) {
        match input {
            Input::Command(BeaconCmd::SendNow) => out.push(self.beacon()),
            Input::Command(BeaconCmd::SendRaw(port, data)) => out.push(Output::Transmit { port, data }),
            Input::Frame { port, data } if data.len() >= 5 => {
                let counter = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
                out.push(Output::Event(BeaconEvent::Heard {
                    port,
                    from: data[0],
                    counter,
                }));
            }
            Input::Frame { .. } => {}
        }
    }

    fn on_deadline(&mut self, now: Millis, out: &mut Vec<Self::Output>) {
        if let Some(interval) = self.interval {
            if now >= self.next {
                out.push(self.beacon());
                // Strictly periodic in local time; after a long outage, skip missed slots.
                let jitter = Millis(self.rng.below(self.jitter_ms + 1));
                self.next = self.next + interval + jitter;
                if self.next <= now {
                    self.next = now + interval + jitter;
                }
            }
        }
    }

    fn next_deadline(&self) -> Option<Millis> {
        self.interval.map(|_| self.next)
    }
}
