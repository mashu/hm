//! FIR filters designed at run time for the sample rate in use.

use alloc::vec::Vec;
use core::f32::consts::PI;
use libm::{cosf, sinf};

/// Direct-form FIR with a circular history.
#[derive(Clone, Debug)]
pub struct Fir {
    taps: Vec<f32>,
    hist: Vec<f32>,
    at: usize,
}

impl Fir {
    pub fn new(taps: Vec<f32>) -> Fir {
        let n = taps.len().max(1);
        Fir {
            taps,
            hist: alloc::vec![0.0; n],
            at: 0,
        }
    }

    pub fn step(&mut self, x: f32) -> f32 {
        let n = self.hist.len();
        self.hist[self.at] = x;
        let mut acc = 0.0;
        // taps[0] applies to the newest sample.
        let (newer, older) = self.hist.split_at(self.at + 1);
        let mut k = 0;
        for &h in newer.iter().rev() {
            acc += h * self.taps[k];
            k += 1;
        }
        for &h in older.iter().rev() {
            acc += h * self.taps[k];
            k += 1;
        }
        self.at = (self.at + 1) % n;
        acc
    }
}

fn hamming(i: usize, n: usize) -> f32 {
    if n <= 1 {
        return 1.0;
    }
    0.54 - 0.46 * cosf(2.0 * PI * i as f32 / (n - 1) as f32)
}

fn sinc(x: f32) -> f32 {
    if x.abs() < 1e-6 {
        1.0
    } else {
        sinf(PI * x) / (PI * x)
    }
}

/// Windowed-sinc band-pass between `lo` and `hi` Hz, unit gain at the centre.
pub fn bandpass(fs: f32, lo: f32, hi: f32, n: usize) -> Vec<f32> {
    let m = (n - 1) as f32 / 2.0;
    let mut taps: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 - m;
            let h = 2.0 * hi / fs * sinc(2.0 * hi / fs * t) - 2.0 * lo / fs * sinc(2.0 * lo / fs * t);
            h * hamming(i, n)
        })
        .collect();
    let fc = (lo + hi) / 2.0;
    let (mut re, mut im) = (0.0, 0.0);
    for (i, h) in taps.iter().enumerate() {
        re += h * cosf(2.0 * PI * fc / fs * i as f32);
        im += h * sinf(2.0 * PI * fc / fs * i as f32);
    }
    let g = libm::sqrtf(re * re + im * im).max(1e-9);
    for h in taps.iter_mut() {
        *h /= g;
    }
    taps
}

/// Smoothing window summing to one (the integrate part of a correlator).
pub fn window(n: usize) -> Vec<f32> {
    let w: Vec<f32> = (0..n).map(|i| hamming(i, n)).collect();
    let s: f32 = w.iter().sum();
    w.into_iter().map(|x| x / s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain_at(taps: &[f32], fs: f32, f: f32) -> f32 {
        let mut fir = Fir::new(taps.to_vec());
        let mut peak: f32 = 0.0;
        for i in 0..(fs as usize / 5) {
            let y = fir.step(sinf(2.0 * PI * f * i as f32 / fs));
            if i > fs as usize / 10 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn bandpass_treats_both_tones_alike_and_rejects_outside() {
        let fs = 48_000.0;
        let taps = bandpass(fs, 900.0, 2500.0, 81);
        let (mark, space) = (gain_at(&taps, fs, 1200.0), gain_at(&taps, fs, 2200.0));
        // Short filters are not flat at the edges; what matters is that both tones
        // are treated alike (the per-tone AGC evens out the rest) and the rest is cut.
        assert!((mark - space).abs() < 0.02, "{mark} vs {space}");
        assert!(mark > 0.7 && gain_at(&taps, fs, 1700.0) > 0.99);
        assert!(gain_at(&taps, fs, 300.0) < 0.1 && gain_at(&taps, fs, 4000.0) < 0.02);
    }
}
