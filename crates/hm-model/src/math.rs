//! Special functions and samplers, without `std` (through `libm`).

use hm_core::DetRng;
use libm::{exp, lgamma, log, log1p, sqrt};

/// Largest magnitude of a log-odds value we work with: probabilities stay
/// within about 1e-13 of 0 and 1, so logs and ratios stay finite.
const LOGIT_LIMIT: f64 = 30.0;

/// The logistic function, `1 / (1 + e^-z)`, computed without overflow.
pub fn sigmoid(z: f64) -> f64 {
    if z >= 0.0 {
        1.0 / (1.0 + exp(-z))
    } else {
        let e = exp(z);
        e / (1.0 + e)
    }
}

/// Log-odds of `p`, clamped to `±LOGIT_LIMIT`.
pub fn logit(p: f64) -> f64 {
    let p = p.clamp(1.0e-13, 1.0 - 1.0e-13);
    (log(p) - log1p(-p)).clamp(-LOGIT_LIMIT, LOGIT_LIMIT)
}

/// `ln B(a, b)`.
pub fn ln_beta(a: f64, b: f64) -> f64 {
    lgamma(a) + lgamma(b) - lgamma(a + b)
}

/// The regularized incomplete beta function `I_x(a, b)`.
pub fn incomplete_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let scale = exp(-ln_beta(a, b) + a * log(x) + b * log1p(-x));
    if x < (a + 1.0) / (a + b + 2.0) {
        scale * continued_fraction(a, b, x) / a
    } else {
        1.0 - scale * continued_fraction(b, a, 1.0 - x) / b
    }
}

/// Lentz's continued fraction for the incomplete beta function.
fn continued_fraction(a: f64, b: f64, x: f64) -> f64 {
    const EPSILON: f64 = 3.0e-14;
    const TINY: f64 = 1.0e-300;
    let guard = |v: f64| if v.abs() < TINY { TINY } else { v };
    let mut c = 1.0;
    let mut d = 1.0 / guard(1.0 - (a + b) * x / (a + 1.0));
    let mut result = d;
    for m in 1..=300 {
        let m = f64::from(m);
        let even = m * (b - m) * x / ((a - 1.0 + 2.0 * m) * (a + 2.0 * m));
        d = 1.0 / guard(1.0 + even * d);
        c = guard(1.0 + even / c);
        result *= d * c;
        let odd = -(a + m) * (a + b + m) * x / ((a + 2.0 * m) * (a + 1.0 + 2.0 * m));
        d = 1.0 / guard(1.0 + odd * d);
        c = guard(1.0 + odd / c);
        let delta = d * c;
        result *= delta;
        if (delta - 1.0).abs() <= EPSILON {
            break;
        }
    }
    result
}

/// The `p` quantile of Beta(a, b), by bisection.
pub fn beta_quantile(a: f64, b: f64, p: f64) -> f64 {
    if p <= 0.0 {
        return 0.0;
    }
    if p >= 1.0 {
        return 1.0;
    }
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..64 {
        let middle = 0.5 * (low + high);
        if incomplete_beta(a, b, middle) < p {
            low = middle;
        } else {
            high = middle;
        }
    }
    0.5 * (low + high)
}

/// `P[T <= t]` for Student's t with `nu` degrees of freedom.
pub fn student_t_cdf(t: f64, nu: f64) -> f64 {
    let tail = 0.5 * incomplete_beta(0.5 * nu, 0.5, nu / (nu + t * t));
    if t >= 0.0 {
        1.0 - tail
    } else {
        tail
    }
}

/// A standard normal draw (Box–Muller).
pub fn normal(rng: &mut DetRng) -> f64 {
    let u1 = rng.next_f64().max(f64::MIN_POSITIVE);
    let u2 = rng.next_f64();
    sqrt(-2.0 * log(u1)) * libm::cos(core::f64::consts::TAU * u2)
}

/// A Gamma(shape, 1) draw (Marsaglia and Tsang; shape below 1 by boosting).
pub fn gamma(rng: &mut DetRng, shape: f64) -> f64 {
    if shape < 1.0 {
        let u = rng.next_f64().max(f64::MIN_POSITIVE);
        return gamma(rng, shape + 1.0) * libm::pow(u, 1.0 / shape);
    }
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / sqrt(9.0 * d);
    loop {
        let x = normal(rng);
        let v = 1.0 + c * x;
        if v <= 0.0 {
            continue;
        }
        let v = v * v * v;
        let u = rng.next_f64().max(f64::MIN_POSITIVE);
        if log(u) < 0.5 * x * x + d - d * v + d * log(v) {
            return d * v;
        }
    }
}

/// A Beta(a, b) draw.
pub fn beta(rng: &mut DetRng, a: f64, b: f64) -> f64 {
    let x = gamma(rng, a);
    let y = gamma(rng, b);
    if x + y <= 0.0 {
        a / (a + b)
    } else {
        x / (x + y)
    }
}

/// `0.5^(elapsed / half_life)`: how much of the evidence gathered `elapsed`
/// seconds ago still counts.
pub fn fade(elapsed: u64, half_life: u64) -> f64 {
    libm::exp2(-(elapsed as f64) / half_life.max(1) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logistic_round_trips() {
        for p in [1e-6, 0.1, 0.5, 0.9, 1.0 - 1e-6] {
            assert!((sigmoid(logit(p)) - p).abs() < 1e-12, "{p}");
        }
        assert_eq!(sigmoid(1e6), 1.0);
        assert!(logit(0.0).is_finite() && logit(1.0).is_finite());
    }

    #[test]
    fn beta_quantiles_of_known_shapes() {
        assert!((beta_quantile(1.0, 1.0, 0.1) - 0.1).abs() < 1e-12);
        assert!((beta_quantile(2.0, 1.0, 0.1) - sqrt(0.1)).abs() < 1e-12);
        assert!((incomplete_beta(3.0, 3.0, 0.5) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn student_t_is_symmetric_and_near_normal_for_many_degrees() {
        assert!((student_t_cdf(0.0, 3.0) - 0.5).abs() < 1e-12);
        assert!((student_t_cdf(1.0, 5.0) + student_t_cdf(-1.0, 5.0) - 1.0).abs() < 1e-12);
        // Standard normal: P[Z <= 1.96] = 0.975.
        assert!((student_t_cdf(1.96, 1e6) - 0.975).abs() < 1e-3);
        // t with 1 degree of freedom is Cauchy: P[T <= 1] = 0.75.
        assert!((student_t_cdf(1.0, 1.0) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn samplers_have_the_right_means() {
        let mut rng = DetRng::from_seed(7);
        let n = 20_000;
        let mean = |f: &mut dyn FnMut() -> f64| (0..n).map(|_| f()).sum::<f64>() / n as f64;
        assert!((mean(&mut || gamma(&mut rng, 0.5)) - 0.5).abs() < 0.03);
        let mut rng = DetRng::from_seed(8);
        assert!((mean(&mut || gamma(&mut rng, 4.0)) - 4.0).abs() < 0.1);
        let mut rng = DetRng::from_seed(9);
        assert!((mean(&mut || beta(&mut rng, 2.0, 6.0)) - 0.25).abs() < 0.01);
    }
}
