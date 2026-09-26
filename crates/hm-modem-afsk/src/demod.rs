use alloc::vec::Vec;
use core::f32::consts::TAU;

use crate::filter::{bandpass, window, Fir};
use crate::hdlc::Deframer;
use crate::il2p;
use crate::{BAUD, MARK_HZ, SPACE_HZ};

/// Tuning of the demodulator; the defaults suit FM voice radios.
#[derive(Clone, Debug)]
pub struct DemodulatorConfig {
    pub sample_rate: u32,
    /// Space-to-mark weights, one slicer each. Spread in 2 dB steps across
    /// +-6 dB they cover the tilt of pre-emphasised or de-emphasised audio.
    pub slicer_gains: Vec<f32>,
    /// PLL pull towards each transition while searching and once locked.
    pub inertia_searching: f32,
    pub inertia_locked: f32,
    /// Prefilter pass band in Hz and length in bit periods.
    pub prefilter_lo: f32,
    pub prefilter_hi: f32,
    pub prefilter_bits: f32,
    /// Correlator integration window, in bit periods.
    pub window_bits: f32,
}

impl DemodulatorConfig {
    pub fn new(sample_rate: u32) -> DemodulatorConfig {
        DemodulatorConfig {
            sample_rate,
            slicer_gains: alloc::vec![0.5, 0.63, 0.79, 1.0, 1.26, 1.58, 2.0],
            inertia_searching: 0.50,
            inertia_locked: 0.74,
            // Chosen on Direwolf's gen_packets benchmark at 11.025-48 kHz; results are
            // flat for windows of 1.1-1.6 bits, and fall apart below one bit.
            prefilter_lo: 900.0,
            prefilter_hi: 2500.0,
            prefilter_bits: 2.0,
            window_bits: 1.2,
        }
    }
}

/// Follows the envelope of one tone: fast attack, slow release.
#[derive(Clone, Debug)]
struct Agc {
    peak: f32,
    attack: f32,
    release: f32,
}

impl Agc {
    fn new(fs: f32) -> Agc {
        // Attack within a bit or two; release over roughly a quarter second.
        Agc {
            peak: 1e-6,
            attack: 1.0 - libm::expf(-BAUD / (1.5 * fs)),
            release: 1.0 - libm::expf(-4.0 / fs),
        }
    }

    fn norm(&mut self, x: f32) -> f32 {
        let k = if x > self.peak { self.attack } else { self.release };
        self.peak += k * (x - self.peak);
        x / self.peak.max(1e-6)
    }
}

/// Carrier detect scores the last 32 transitions (one bit each in a `u32`).
const DCD_ON: u32 = 26;
const DCD_OFF: u32 = 18;

#[derive(Clone, Debug)]
struct Slicer {
    gain: f32,
    pll: i32,
    prev_level: bool,
    prev_sign: bool,
    deframer: Deframer,
    il2p: il2p::Receiver,
    /// Last 32 transitions: bit set when it fell near the expected moment.
    quality: u32,
    locked: bool,
}

impl Slicer {
    fn good_transitions(&self) -> u32 {
        self.quality.count_ones()
    }
}

/// Audio to verified frames.
pub struct Demodulator {
    cfg: DemodulatorConfig,
    prefilter: Fir,
    lp: [Fir; 4],
    mark_phase: f32,
    space_phase: f32,
    mark_step: f32,
    space_step: f32,
    agc_mark: Agc,
    agc_space: Agc,
    pll_step: i32,
    slicers: Vec<Slicer>,
    samples: u64,
    /// (checksum, sample index) of frames recently emitted, to drop copies from other slicers.
    recent: Vec<(u32, u64)>,
    dedupe_window: u64,
    frames: u64,
    il2p_frames: u64,
}

fn checksum(f: &[u8]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &b in f {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    h
}

impl Demodulator {
    pub fn new(cfg: DemodulatorConfig) -> Demodulator {
        let fs = cfg.sample_rate as f32;
        let per_bit = fs / BAUD;
        let bp_len = ((per_bit * cfg.prefilter_bits) as usize) | 1;
        let lp_len = ((per_bit * cfg.window_bits) as usize).max(3);
        let w = window(lp_len);
        let slicers = cfg
            .slicer_gains
            .iter()
            .map(|&gain| Slicer {
                gain,
                pll: 0,
                prev_level: false,
                prev_sign: false,
                deframer: Deframer::new(),
                il2p: il2p::Receiver::new(),
                quality: 0,
                locked: false,
            })
            .collect();
        Demodulator {
            prefilter: Fir::new(bandpass(fs, cfg.prefilter_lo, cfg.prefilter_hi, bp_len)),
            lp: [
                Fir::new(w.clone()),
                Fir::new(w.clone()),
                Fir::new(w.clone()),
                Fir::new(w),
            ],
            mark_phase: 0.0,
            space_phase: 0.0,
            mark_step: TAU * MARK_HZ / fs,
            space_step: TAU * SPACE_HZ / fs,
            agc_mark: Agc::new(fs),
            agc_space: Agc::new(fs),
            pll_step: ((1u64 << 32) as f64 * BAUD as f64 / fs as f64) as u32 as i32,
            slicers,
            samples: 0,
            recent: Vec::new(),
            dedupe_window: (fs * 0.5) as u64,
            frames: 0,
            il2p_frames: 0,
            cfg,
        }
    }

    pub fn config(&self) -> &DemodulatorConfig {
        &self.cfg
    }

    /// Frames decoded so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// How many of those came in IL2P (the rest in HDLC).
    pub fn il2p_frames(&self) -> u64 {
        self.il2p_frames
    }

    /// Carrier detect: some slicer sees a regular 1200-baud bit stream.
    pub fn dcd(&self) -> bool {
        self.slicers.iter().any(|s| s.locked)
    }

    /// Feed audio samples; verified frames (without FCS) are appended to `out`.
    pub fn process(&mut self, samples: &[f32], out: &mut Vec<Vec<u8>>) {
        for &x in samples {
            self.sample(x, out);
        }
    }

    fn sample(&mut self, x: f32, out: &mut Vec<Vec<u8>>) {
        self.samples += 1;
        let y = self.prefilter.step(x);
        let (ms, mc) = libm::sincosf(self.mark_phase);
        let (ss, sc) = libm::sincosf(self.space_phase);
        self.mark_phase = (self.mark_phase + self.mark_step) % TAU;
        self.space_phase = (self.space_phase + self.space_step) % TAU;
        let mi = self.lp[0].step(y * mc);
        let mq = self.lp[1].step(y * ms);
        let si = self.lp[2].step(y * sc);
        let sq = self.lp[3].step(y * ss);
        let mark = self.agc_mark.norm(libm::sqrtf(mi * mi + mq * mq));
        let space = self.agc_space.norm(libm::sqrtf(si * si + sq * sq));

        for i in 0..self.slicers.len() {
            let s = &mut self.slicers[i];
            let level = mark - s.gain * space;
            let sign = level > 0.0;
            let before = s.pll;
            s.pll = s.pll.wrapping_add(self.pll_step);
            if before > 0 && s.pll < 0 {
                // Middle of a bit: decide it. HDLC undoes NRZI; IL2P takes it as it is.
                let bit = (sign == s.prev_level) as u8;
                s.prev_level = sign;
                let hdlc = s.deframer.push(bit).map(|f| (f, false));
                let il2p = s.il2p.push(sign as u8).map(|f| (f, true));
                for (frame, is_il2p) in hdlc.into_iter().chain(il2p) {
                    let sum = checksum(&frame);
                    let now = self.samples;
                    let window = self.dedupe_window;
                    self.recent.retain(|(_, t)| now - *t < window);
                    if !self.recent.iter().any(|(h, _)| *h == sum) {
                        self.recent.push((sum, now));
                        self.frames += 1;
                        self.il2p_frames += is_il2p as u64;
                        out.push(frame);
                    }
                }
            }
            let s = &mut self.slicers[i];
            if sign != s.prev_sign {
                // A transition belongs half-way between decisions, where the PLL reads 0.
                let near = s.pll.unsigned_abs() < (1u32 << 30);
                s.quality = (s.quality << 1) | near as u32;
                let good = s.good_transitions();
                if s.locked && good < DCD_OFF {
                    s.locked = false;
                } else if !s.locked && good >= DCD_ON {
                    s.locked = true;
                }
                let inertia = if s.locked {
                    self.cfg.inertia_locked
                } else {
                    self.cfg.inertia_searching
                };
                s.pll = (s.pll as f32 * inertia) as i32;
            }
            s.prev_sign = sign;
        }
    }
}
