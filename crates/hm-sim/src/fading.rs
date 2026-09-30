//! Loss on a link: the Watterson-style fading of an HF path, and the
//! simpler per-frame models.

use hm_core::{DetRng, Millis};

use crate::{Link, Loss};

/// Sinusoids summed to make one path's fading gain in [`Loss::Fading`].
const FADE_PATHS: usize = 16;

/// The scattered part of one path's complex gain in [`Loss::Fading`]:
/// `sum(exp(j(2 pi f_n t + phase_n))) / sqrt(N)`, with Doppler shifts `f_n`
/// drawn from a Gaussian of standard deviation `spread / 2` and uniform
/// phases. Unit power on average, with the Gaussian autocorrelation of the
/// Watterson model, `exp(-2 pi^2 sigma^2 dt^2)`. Being a function of time, it
/// can be evaluated in any order.
pub(crate) struct Fade {
    doppler_hz: [f64; FADE_PATHS],
    phase: [f64; FADE_PATHS],
}

impl Fade {
    pub(crate) fn new(rng: &mut DetRng, doppler_spread_hz: f64) -> Fade {
        let sigma = doppler_spread_hz / 2.0;
        let mut doppler_hz = [0.0; FADE_PATHS];
        let mut phase = [0.0; FADE_PATHS];
        for n in 0..FADE_PATHS {
            // Unit-power complex Gaussian: each part has variance 1/2.
            let (z, _) = unit_complex_gaussian(rng);
            doppler_hz[n] = sigma * z * core::f64::consts::SQRT_2;
            phase[n] = core::f64::consts::TAU * rng.next_f64();
        }
        Fade { doppler_hz, phase }
    }

    /// Power gain at `t`, mean 1, with Rician factor `k`.
    pub(crate) fn power(&self, t: Millis, k: f64) -> f64 {
        let secs = t.0 as f64 / 1000.0;
        let (mut re, mut im) = (0.0, 0.0);
        for n in 0..FADE_PATHS {
            let angle = core::f64::consts::TAU * self.doppler_hz[n] * secs + self.phase[n];
            re += libm::cos(angle);
            im += libm::sin(angle);
        }
        let scale = libm::sqrt(1.0 / (FADE_PATHS as f64 * (k + 1.0)));
        let steady = libm::sqrt(k / (k + 1.0));
        let (re, im) = (steady + scale * re, scale * im);
        re * re + im * im
    }

    /// The weakest SNR between `start` and `end`, sampled every eighth of
    /// `1 / doppler_spread_hz` (the fading is smooth at that scale), at most
    /// 64 times per frame.
    pub(crate) fn weakest_snr_db(
        &self,
        (start, end): (Millis, Millis),
        doppler_spread_hz: f64,
        k: f64,
        mean_snr_db: f64,
    ) -> f64 {
        let fine = (125.0 / doppler_spread_hz.max(1e-3)) as u64;
        let step = fine.max(1).max(end.0.saturating_sub(start.0).div_ceil(64));
        let mut weakest = self.power(start, k);
        let mut t = start.0;
        while t < end.0 {
            t = (t + step).min(end.0);
            weakest = weakest.min(self.power(Millis(t), k));
        }
        mean_snr_db + 10.0 * libm::log10(weakest.max(1e-12))
    }
}

/// A complex Gaussian of unit power: two independent normals of variance
/// 1/2, by Box–Muller (with `libm`, so runs match on every platform).
fn unit_complex_gaussian(rng: &mut DetRng) -> (f64, f64) {
    let u = rng.next_f64().max(f64::MIN_POSITIVE);
    let angle = core::f64::consts::TAU * rng.next_f64();
    let r = libm::sqrt(-libm::log(u));
    (r * libm::cos(angle), r * libm::sin(angle))
}

pub(crate) fn lose(rng: &mut DetRng, link: &mut Link, now: Millis, on_air: usize) -> bool {
    match link.loss {
        Loss::None => false,
        Loss::Bernoulli(p) => rng.chance(p),
        Loss::GilbertElliott {
            p_good_to_bad,
            p_bad_to_good,
            loss_good,
            loss_bad,
        } => {
            let flip = if link.bad { p_bad_to_good } else { p_good_to_bad };
            if rng.chance(flip) {
                link.bad = !link.bad;
            }
            rng.chance(if link.bad { loss_bad } else { loss_good })
        }
        Loss::Hourly(table) => rng.chance(table[((now.0 / 3_600_000) % 24) as usize]),
        Loss::Measured { curve, snr_db } => rng.chance(curve.loss(snr_db, on_air)),
        // Judged with the path's fading state, in `Sim::finish_tx`.
        Loss::Fading {
            curve, mean_snr_db, ..
        } => rng.chance(curve.loss(mean_snr_db, on_air)),
    }
}
