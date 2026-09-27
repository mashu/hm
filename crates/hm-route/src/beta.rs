const LANCZOS_G: f64 = 7.0;
const LANCZOS: [f64; 9] = [
    0.999_999_999_999_809_9,
    676.520_368_121_885_1,
    -1_259.139_216_722_402_8,
    771.323_428_777_653_1,
    -176.615_029_162_140_6,
    12.507_343_278_686_905,
    -0.138_571_095_265_720_12,
    9.984_369_578_019_572e-6,
    1.505_632_735_149_311_6e-7,
];

pub(crate) fn quantile(alpha: f64, beta: f64, probability: f64) -> f64 {
    debug_assert!(alpha > 0.0 && beta > 0.0);
    if probability <= 0.0 {
        return 0.0;
    }
    if probability >= 1.0 {
        return 1.0;
    }
    let mut low = 0.0;
    let mut high = 1.0;
    for _ in 0..64 {
        let middle = (low + high) * 0.5;
        if regularized_incomplete(alpha, beta, middle) < probability {
            low = middle;
        } else {
            high = middle;
        }
    }
    (low + high) * 0.5
}

fn regularized_incomplete(alpha: f64, beta: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let scale =
        (ln_gamma(alpha + beta) - ln_gamma(alpha) - ln_gamma(beta) + alpha * x.ln() + beta * (-x).ln_1p())
            .exp();
    if x < (alpha + 1.0) / (alpha + beta + 2.0) {
        scale * continued_fraction(alpha, beta, x) / alpha
    } else {
        1.0 - scale * continued_fraction(beta, alpha, 1.0 - x) / beta
    }
}

fn continued_fraction(alpha: f64, beta: f64, x: f64) -> f64 {
    const EPSILON: f64 = 3.0e-14;
    const MIN: f64 = 1.0e-300;
    let sum = alpha + beta;
    let alpha_plus = alpha + 1.0;
    let alpha_minus = alpha - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - sum * x / alpha_plus;
    if d.abs() < MIN {
        d = MIN;
    }
    d = 1.0 / d;
    let mut result = d;
    for iteration in 1..=200 {
        let m = f64::from(iteration);
        let twice = 2.0 * m;
        let even = m * (beta - m) * x / ((alpha_minus + twice) * (alpha + twice));
        d = 1.0 + even * d;
        if d.abs() < MIN {
            d = MIN;
        }
        c = 1.0 + even / c;
        if c.abs() < MIN {
            c = MIN;
        }
        d = 1.0 / d;
        result *= d * c;

        let odd = -(alpha + m) * (sum + m) * x / ((alpha + twice) * (alpha_plus + twice));
        d = 1.0 + odd * d;
        if d.abs() < MIN {
            d = MIN;
        }
        c = 1.0 + odd / c;
        if c.abs() < MIN {
            c = MIN;
        }
        d = 1.0 / d;
        let delta = d * c;
        result *= delta;
        if (delta - 1.0).abs() <= EPSILON {
            break;
        }
    }
    result
}

fn ln_gamma(z: f64) -> f64 {
    if z < 0.5 {
        return std::f64::consts::PI.ln() - (std::f64::consts::PI * z).sin().ln() - ln_gamma(1.0 - z);
    }
    let shifted = z - 1.0;
    let mut series = LANCZOS[0];
    for (index, coefficient) in LANCZOS.iter().enumerate().skip(1) {
        series += coefficient / (shifted + index as f64);
    }
    let t = shifted + LANCZOS_G + 0.5;
    0.5 * std::f64::consts::TAU.ln() + (shifted + 0.5) * t.ln() - t + series.ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_simple_distributions() {
        assert!((quantile(1.0, 1.0, 0.1) - 0.1).abs() < 1e-12);
        assert!((quantile(2.0, 1.0, 0.1) - 0.1_f64.sqrt()).abs() < 1e-12);
        assert!((quantile(1.0, 2.0, 0.1) - (1.0 - 0.9_f64.sqrt())).abs() < 1e-12);
    }

    #[test]
    fn quantiles_are_monotonic() {
        let mut previous = 0.0;
        for percentile in 1..100 {
            let value = quantile(4.5, 2.25, f64::from(percentile) / 100.0);
            assert!(value > previous);
            previous = value;
        }
    }
}
