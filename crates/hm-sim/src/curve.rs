//! Frame loss of a real modem, from a table measured by SNR and frame length.

/// Frame loss measured for one modem.
///
/// Between grid points the survival probability is interpolated in the log
/// domain: linearly in SNR, and in frame length as if bit errors were
/// independent, so outside the measured lengths loss grows as it would for
/// longer or shorter frames. Outside the SNR range the nearest row applies.
#[derive(Debug, PartialEq)]
pub struct LossCurve {
    pub name: &'static str,
    /// Ascending SNR in dB, noise measured in a 3 kHz bandwidth.
    pub snr_db: &'static [f64],
    /// Ascending frame lengths: bytes on air, the link's per-frame overhead included.
    pub len: &'static [u32],
    /// `loss[i][j]`: share of frames lost at `snr_db[i]` and `len[j]`.
    pub loss: &'static [&'static [f64]],
    /// Frames sent per grid point when the table was measured.
    pub frames_per_point: u32,
}

/// Survival below this counts as this, so the log stays finite.
const SURVIVAL_FLOOR: f64 = 1e-6;

impl LossCurve {
    /// Probability of losing a frame of `on_air` bytes at `snr_db`.
    ///
    /// Uses `libm`, not the platform's maths library, so runs stay identical
    /// on every platform.
    pub fn loss(&self, snr_db: f64, on_air: usize) -> f64 {
        let (i, fi) = bracket(self.snr_db, snr_db);
        let row = |r: usize| self.log_survival(r, on_air as f64);
        let ls = if fi == 0.0 {
            row(i)
        } else {
            row(i) + fi * (row(i + 1) - row(i))
        };
        (1.0 - libm::exp(ls)).clamp(0.0, 1.0)
    }

    fn log_survival(&self, row: usize, len: f64) -> f64 {
        let ls = |j: usize| libm::log((1.0 - self.loss[row][j]).max(SURVIVAL_FLOOR));
        let first = self.len[0] as f64;
        let last = *self.len.last().expect("non-empty") as f64;
        if len <= first {
            ls(0) * len / first
        } else if len >= last {
            ls(self.len.len() - 1) * len / last
        } else {
            let lens: &[u32] = self.len;
            let j = lens.iter().rposition(|&l| l as f64 <= len).expect("above first");
            let (a, b) = (lens[j] as f64, lens[j + 1] as f64);
            ls(j) + (len - a) / (b - a) * (ls(j + 1) - ls(j))
        }
    }
}

/// Index `i` and fraction `f` with `x` at `xs[i] + f * (xs[i + 1] - xs[i])`,
/// clamped to the ends (then `f` is 0).
fn bracket(xs: &[f64], x: f64) -> (usize, f64) {
    if x <= xs[0] {
        return (0, 0.0);
    }
    let last = xs.len() - 1;
    if x >= xs[last] {
        return (last, 0.0);
    }
    let i = xs.iter().rposition(|&v| v <= x).expect("above first");
    (i, (x - xs[i]) / (xs[i + 1] - xs[i]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::afsk_1200::CURVE;

    const T: LossCurve = LossCurve {
        name: "test",
        snr_db: &[0.0, 10.0],
        len: &[100, 200],
        loss: &[&[0.5, 0.75], &[0.0, 0.0]],
        frames_per_point: 1,
    };

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn grid_points_come_back_exactly() {
        assert!(close(T.loss(0.0, 100), 0.5));
        assert!(close(T.loss(0.0, 200), 0.75));
        assert!(close(T.loss(10.0, 150), 0.0));
        for (i, &snr) in CURVE.snr_db.iter().enumerate() {
            for (j, &len) in CURVE.len.iter().enumerate() {
                let want = CURVE.loss[i][j];
                if want < 1.0 {
                    assert!(
                        close(CURVE.loss(snr, len as usize), want),
                        "{snr} dB, {len} bytes"
                    );
                }
            }
        }
    }

    #[test]
    fn between_and_beyond_the_grid() {
        // Halfway in SNR: survival is the geometric mean, sqrt(0.5 * 1).
        assert!(close(T.loss(5.0, 100), 1.0 - 0.5f64.sqrt()));
        // Outside the SNR range the nearest row applies.
        assert!(close(T.loss(-20.0, 100), 0.5));
        assert!(close(T.loss(30.0, 100), 0.0));
        // Frames shorter or longer than measured: independent bit errors.
        assert!(close(T.loss(0.0, 50), 1.0 - 0.5f64.sqrt()));
        assert!(close(T.loss(0.0, 400), 1.0 - 0.25 * 0.25));
        // Between lengths, in the log domain: survival sqrt(0.5 * 0.25).
        assert!(close(T.loss(0.0, 150), 1.0 - (0.5f64 * 0.25).sqrt()));
    }

    #[test]
    fn afsk_table_is_well_formed() {
        let c = &CURVE;
        assert!(c.snr_db.windows(2).all(|w| w[0] < w[1]));
        assert!(c.len.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(c.loss.len(), c.snr_db.len());
        let n = c.frames_per_point as f64;
        // Slack for measurement noise at each point.
        let slack = |p: f64| 4.0 * (p * (1.0 - p) / n).sqrt() + 3.0 / n;
        for (i, row) in c.loss.iter().enumerate() {
            assert_eq!(row.len(), c.len.len());
            for (j, &p) in row.iter().enumerate() {
                assert!((0.0..=1.0).contains(&p));
                if j > 0 {
                    assert!(p + slack(p) >= row[j - 1], "longer frames lose less at {i},{j}");
                }
                if i > 0 {
                    assert!(p <= c.loss[i - 1][j] + slack(p), "more SNR loses more at {i},{j}");
                }
            }
        }
        // The ends of the range: almost nothing gets through, almost nothing is lost.
        assert!(c.loss[0].iter().all(|&p| p > 0.9));
        assert!(c.loss.last().unwrap().iter().all(|&p| p < 0.01));
    }
}
