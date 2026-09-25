//! A minimal beacon machine for exercising the simulator.
//!
//! Frame: `[station id (1), counter (4, big-endian), zero padding]`.

use hm_core::{DetRng, Input, Machine, Millis, Output};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BeaconCmd {
    SendNow,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BeaconEvent {
    Heard { from: u8, counter: u32 },
}

pub struct Beacon {
    id: u8,
    frame_len: usize,
    interval: Option<Millis>,
    jitter_ms: u64,
    rng: DetRng,
    next: Millis,
    counter: u32,
}

impl Beacon {
    /// Beacons every `interval` plus a uniform random `0..=jitter_ms`.
    pub fn periodic(id: u8, frame_len: usize, interval: Millis, jitter_ms: u64, mut rng: DetRng) -> Beacon {
        let first = Millis(interval.0 + rng.below(jitter_ms + 1));
        Beacon {
            id,
            frame_len: frame_len.max(5),
            interval: Some(interval),
            jitter_ms,
            rng,
            next: first,
            counter: 0,
        }
    }

    /// Transmits only on [`BeaconCmd::SendNow`].
    pub fn silent(id: u8, frame_len: usize) -> Beacon {
        Beacon {
            id,
            frame_len: frame_len.max(5),
            interval: None,
            jitter_ms: 0,
            rng: DetRng::from_seed(0),
            next: Millis::ZERO,
            counter: 0,
        }
    }

    fn frame(&mut self) -> Vec<u8> {
        let mut f = vec![0u8; self.frame_len];
        f[0] = self.id;
        f[1..5].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        f
    }
}

impl Machine for Beacon {
    type Input = Input<BeaconCmd>;
    type Output = Output<BeaconEvent>;

    fn handle(&mut self, _now: Millis, input: Self::Input, out: &mut Vec<Self::Output>) {
        match input {
            Input::Command(BeaconCmd::SendNow) => {
                let f = self.frame();
                out.push(Output::Transmit(f));
            }
            Input::Frame(f) if f.len() >= 5 => {
                let counter = u32::from_be_bytes([f[1], f[2], f[3], f[4]]);
                out.push(Output::Event(BeaconEvent::Heard { from: f[0], counter }));
            }
            Input::Frame(_) => {}
        }
    }

    fn on_deadline(&mut self, now: Millis, out: &mut Vec<Self::Output>) {
        if let Some(interval) = self.interval {
            if now >= self.next {
                let f = self.frame();
                out.push(Output::Transmit(f));
                self.next = now + interval + Millis(self.rng.below(self.jitter_ms + 1));
            }
        }
    }

    fn next_deadline(&self) -> Option<Millis> {
        self.interval.map(|_| self.next)
    }
}
