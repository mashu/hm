//! A virtual radio channel in real time, for tests without hardware.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hm_core::DetRng;

use crate::AudioPort;

struct Tx {
    station: usize,
    start: u64,
    samples: Vec<f32>,
}

struct State {
    txs: Vec<Tx>,
    stations: usize,
    rng: DetRng,
}

/// The shared channel. Every [`Ether::port`] is one station's radio.
#[derive(Clone)]
pub struct Ether {
    state: Arc<Mutex<State>>,
    fs: u32,
    noise_rms: f32,
    start: Instant,
}

impl Ether {
    pub fn new(sample_rate: u32, noise_rms: f32, seed: u64) -> Ether {
        Ether {
            state: Arc::new(Mutex::new(State {
                txs: Vec::new(),
                stations: 0,
                rng: DetRng::from_seed(seed),
            })),
            fs: sample_rate,
            noise_rms,
            start: Instant::now(),
        }
    }

    /// A new station on the channel.
    pub fn port(&self) -> EtherPort {
        let mut st = self.state.lock().expect("lock");
        st.stations += 1;
        let now = self.now();
        EtherPort {
            ether: self.clone(),
            id: st.stations - 1,
            read_pos: now,
        }
    }

    fn now(&self) -> u64 {
        (self.start.elapsed().as_secs_f64() * self.fs as f64) as u64
    }
}

pub struct EtherPort {
    ether: Ether,
    id: usize,
    read_pos: u64,
}

fn gaussian(rng: &mut DetRng) -> f32 {
    let u1 = rng.next_f64().max(f64::MIN_POSITIVE);
    let u2 = rng.next_f64();
    ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
}

impl AudioPort for EtherPort {
    fn sample_rate(&self) -> u32 {
        self.ether.fs
    }

    fn capture(&mut self, out: &mut Vec<f32>, wait: Duration) -> io::Result<()> {
        let deadline = Instant::now() + wait;
        let mut now = self.ether.now();
        while now <= self.read_pos && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
            now = self.ether.now();
        }
        if now <= self.read_pos {
            return Ok(());
        }
        let (from, to) = (self.read_pos, now);
        let mut buf = vec![0f32; (to - from) as usize];
        let mut own = vec![false; buf.len()];
        let mut st = self.ether.state.lock().expect("lock");
        for tx in &st.txs {
            let end = tx.start + tx.samples.len() as u64;
            if end <= from || tx.start >= to {
                continue;
            }
            for t in tx.start.max(from)..end.min(to) {
                let i = (t - from) as usize;
                if tx.station == self.id {
                    own[i] = true;
                } else {
                    buf[i] += tx.samples[(t - tx.start) as usize];
                }
            }
        }
        let sigma = self.ether.noise_rms;
        for (v, mine) in buf.iter_mut().zip(&own) {
            // A transmitting radio hears nothing.
            *v = if *mine {
                0.0
            } else {
                *v + sigma * gaussian(&mut st.rng)
            };
        }
        let keep_from = to.saturating_sub(self.ether.fs as u64 * 30);
        st.txs.retain(|t| t.start + t.samples.len() as u64 > keep_from);
        drop(st);
        self.read_pos = to;
        out.extend_from_slice(&buf);
        Ok(())
    }

    fn play(&mut self, samples: &[f32]) -> io::Result<()> {
        let start = self.ether.now();
        {
            let mut st = self.ether.state.lock().expect("lock");
            st.txs.push(Tx {
                station: self.id,
                start,
                samples: samples.to_vec(),
            });
        }
        let secs = samples.len() as f64 / self.ether.fs as f64;
        std::thread::sleep(Duration::from_secs_f64(secs));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stations_hear_each_other_but_not_themselves() {
        let ether = Ether::new(8_000, 0.0, 1);
        let (mut a, mut b) = (ether.port(), ether.port());
        let tone: Vec<f32> = (0..800).map(|i| (i as f32 * 0.3).sin()).collect();
        a.play(&tone).unwrap();
        let (mut heard_a, mut heard_b) = (Vec::new(), Vec::new());
        a.capture(&mut heard_a, Duration::from_millis(50)).unwrap();
        b.capture(&mut heard_b, Duration::from_millis(50)).unwrap();
        let energy = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>();
        assert!(energy(&heard_b) > 100.0, "B hears A: {}", energy(&heard_b));
        assert_eq!(energy(&heard_a), 0.0, "A hears nothing while transmitting");
    }
}
