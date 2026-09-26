use alloc::vec::Vec;
use core::f64::consts::TAU;

use crate::{hdlc, il2p, BAUD, MARK_HZ, SPACE_HZ};

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

    /// One IL2P transmission: preamble bytes for `txdelay_ms`, then the
    /// frames, each with its sync word and Reed–Solomon parity (16 bytes per
    /// block with `max_fec`).
    pub fn modulate_il2p(&self, frames: &[&[u8]], txdelay_ms: u32, max_fec: bool) -> Vec<f32> {
        let preamble = libm::ceilf(txdelay_ms as f32 * BAUD / 8000.0) as usize;
        let bits = il2p::encode_bits(frames, preamble.max(1), max_fec);
        self.tones(bits.iter().map(|&b| b == 1))
    }

    /// NRZI and phase-continuous FSK: a 0 bit changes the tone, a 1 keeps it.
    pub fn modulate_bits(&self, bits: &[u8]) -> Vec<f32> {
        let mut mark = true;
        self.tones(bits.iter().map(move |&b| {
            if b == 0 {
                mark = !mark;
            }
            mark
        }))
    }

    /// Phase-continuous FSK, one tone per bit: mark where `true`.
    fn tones(&self, marks: impl Iterator<Item = bool>) -> Vec<f32> {
        let fs = self.sample_rate as f64;
        let per_bit = fs / BAUD as f64;
        let mut out = Vec::new();
        let mut phase = 0.0f64;
        let mut t = 0.0f64;
        let mut n = 0usize;
        for mark in marks {
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
