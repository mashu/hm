use alloc::vec::Vec;
use core::f64::consts::TAU;

use crate::{hdlc, BAUD, MARK_HZ, SPACE_HZ};

/// Frames to AFSK audio.
#[derive(Clone, Debug)]
pub struct Modulator {
    pub sample_rate: u32,
    /// Peak amplitude, 0..1.
    pub amplitude: f32,
}

impl Modulator {
    pub fn new(sample_rate: u32) -> Modulator {
        Modulator {
            sample_rate,
            amplitude: 0.5,
        }
    }

    /// One transmission: flags for `txdelay_ms` (the receiver's settling time),
    /// the frames, then a short tail of flags.
    pub fn modulate(&self, frames: &[&[u8]], txdelay_ms: u32) -> Vec<f32> {
        let preamble = libm::ceilf(txdelay_ms as f32 * BAUD / 8000.0) as usize;
        let bits = hdlc::encode(frames, preamble.max(1), 3);
        self.modulate_bits(&bits)
    }

    /// NRZI and phase-continuous FSK: a 0 bit changes the tone, a 1 keeps it.
    pub fn modulate_bits(&self, bits: &[u8]) -> Vec<f32> {
        let fs = self.sample_rate as f64;
        let per_bit = fs / BAUD as f64;
        let mut out = Vec::with_capacity((bits.len() as f64 * per_bit) as usize + 1);
        let mut mark = true;
        let mut phase = 0.0f64;
        let mut t = 0.0f64;
        let mut n = 0usize;
        for &b in bits {
            if b == 0 {
                mark = !mark;
            }
            let f = if mark { MARK_HZ } else { SPACE_HZ } as f64;
            t += per_bit;
            while (n as f64) < t {
                out.push(self.amplitude * libm::sin(phase) as f32);
                phase = (phase + TAU * f / fs) % TAU;
                n += 1;
            }
        }
        out
    }
}
