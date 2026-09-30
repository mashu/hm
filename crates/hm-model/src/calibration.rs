//! Whether the handoff chances the beliefs give come true: Bayesian binning
//! of the station's own record.
//!
//! The link model is an approximation. Its filter factorises reach, the
//! daily pattern and the state now; its population prior is one bump where
//! links are mostly either usable or not. Approximations like these err the
//! same way for long stretches: in the simulated HF networks, links never
//! heard were still given a few percent after dozens of failed handoffs, and
//! none of them ever completed one. The station can see this for itself.
//! For every handoff it starts it knows the chance it gave, and later
//! whether the link carried it.
//!
//! The chances given are grouped in bands of log-odds. Each band holds its
//! record, faded with a week's half-life: how many handoffs, how many the
//! link carried, and the chances given for them. The whole record, pooled,
//! gives an overall correction: the log-odds from the chances given to the
//! rate that came true, that rate a Beta whose prior says the chances were
//! right, worth [`PRIOR_STRENGTH`] handoffs. Each band's rate is a Beta of
//! the same strength whose prior is the chances given moved by the overall
//! correction (hierarchical: a band with a short record follows the rest);
//! the band's correction is the log-odds from its chances given to its rate. Bands out of order are pooled
//! (isotonic regression, weighted by their records), so a higher chance given
//! is never a lower chance out. Between bands with a record the corrected
//! log-odds are interpolated; beyond the outermost, the nearest correction
//! holds. This is Bayesian binning rather than one curve through the whole
//! record (Platt scaling), which stretched the chances at both ends when
//! the model erred one way on some links and the other way on others. A band
//! with no record, and a station whose chances come true, keep the chances
//! as given.
//!
//! Chances about a path the station has seen open and about one it has only
//! inferred (never heard, known from the population and from failures) err
//! differently, so each kind is checked against its own record: in the
//! simulated networks the first kind came true about as often as given, and
//! the second nine times less often.

use minicbor::{Decode, Encode};

use crate::math::{fade, logit, sigmoid};
use crate::Bearer;

/// Inner edges of the bands, in log-odds of the chance given: from about
/// 1 % to 95 %, a band a unit of log-odds wide in the middle.
const EDGES: [f64; 8] = [-4.5, -3.0, -2.0, -1.0, 0.0, 1.0, 2.0, 3.0];
const BANDS: usize = EDGES.len() + 1;
/// Handoffs' worth of belief that the chances given were right overall, and
/// in every band that it errs as the whole record does.
const PRIOR_STRENGTH: f64 = 10.0;
/// Outcomes count half after a week: the record follows changes in how the
/// model errs (the season, the network).
const HALF_LIFE: u64 = 7 * 86_400;
/// Chances are taken in log-odds this far from 0 and 1: beyond, a forecast
/// is as sure as it can be.
const EDGE: f64 = 1.0e-4;

/// The chance the beliefs gave a handoff when it started, before
/// calibration: kept to be checked against how the handoff ends
/// ([`Beliefs::observe_forecast`](crate::Beliefs::observe_forecast)).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Forecast {
    pub(crate) bearer: Bearer,
    /// Whether the path had been seen open.
    pub(crate) seen: bool,
    pub(crate) chance: f64,
}

/// One band's record, faded to the calibration's `at`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Encode, Decode)]
struct Band {
    /// Handoffs.
    #[n(0)]
    outcomes: f64,
    /// Of those, carried by the link.
    #[n(1)]
    carried: f64,
    /// Sum of the chances given.
    #[n(2)]
    given: f64,
    /// Sum of their log-odds.
    #[n(3)]
    log_odds: f64,
}

/// What a station has seen of how the chances it gave came true, for one
/// kind of forecast: what is saved.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub(crate) struct Record {
    #[n(0)]
    bands: [Band; BANDS],
    /// Time the record was last faded to: when its latest outcome came.
    #[n(1)]
    at: u64,
}

/// A station's calibration for one kind of forecast: its record, and the
/// correction worked out from it whenever an outcome comes (the chances are
/// asked for far more often than outcomes come).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Calibration {
    record: Record,
    /// The bands with a record: where each sits (the mean log-odds given in
    /// it) and the log-odds that come true there; the first `points` only.
    corrected: [(f64, f64); BANDS],
    points: usize,
}

fn log_odds(p: f64) -> f64 {
    logit(p.clamp(EDGE, 1.0 - EDGE))
}

fn band_of(l: f64) -> usize {
    EDGES.partition_point(|&edge| edge <= l)
}

impl Calibration {
    /// The calibration a saved record gives.
    pub(crate) fn from_record(record: Record) -> Calibration {
        let mut c = Calibration {
            record,
            ..Calibration::default()
        };
        c.correct();
        c
    }

    pub(crate) fn record(&self) -> &Record {
        &self.record
    }

    /// The chance that comes true when `p` is given.
    pub fn apply(&self, p: f64) -> f64 {
        if p.is_nan() || p <= 0.0 {
            return 0.0;
        }
        if p >= 1.0 {
            return 1.0;
        }
        let points = &self.corrected[..self.points];
        let l = log_odds(p);
        let out = match (points.first(), points.last()) {
            (Some(&(x, y)), _) if l <= x => l + y - x,
            (_, Some(&(x, y))) if l >= x => l + y - x,
            (Some(_), Some(_)) => {
                let k = points.partition_point(|&(x, _)| x <= l);
                let ((x0, y0), (x1, y1)) = (points[k - 1], points[k]);
                if x1 > x0 {
                    y0 + (y1 - y0) * (l - x0) / (x1 - x0)
                } else {
                    y1
                }
            }
            _ => l,
        };
        sigmoid(out)
    }

    /// A handoff given chance `p` ended at `at`: `carried` if the link
    /// carried it (whatever the custodian then said).
    pub fn observe(&mut self, p: f64, carried: bool, at: u64) {
        if !(0.0..=1.0).contains(&p) {
            return;
        }
        let record = &mut self.record;
        if at > record.at {
            let keep = fade(at - record.at, HALF_LIFE);
            for band in &mut record.bands {
                band.outcomes *= keep;
                band.carried *= keep;
                band.given *= keep;
                band.log_odds *= keep;
            }
            record.at = at;
        }
        let l = log_odds(p);
        let band = &mut record.bands[band_of(l)];
        band.outcomes += 1.0;
        band.carried += f64::from(u8::from(carried));
        band.given += p;
        band.log_odds += l;
        self.correct();
    }

    /// Work out the correction from the record. Each band's rate is shrunk
    /// toward the chances given moved by the kind's overall correction,
    /// itself the whole record pooled and shrunk toward none: every outcome
    /// teaches every band, and a band departs from the rest only as far as
    /// its own record says. Adjacent bands out of order are pooled, weighted
    /// by their records, so a higher chance given is never a lower chance
    /// out.
    fn correct(&mut self) {
        let bands = &self.record.bands;
        let (outcomes, carried, given) = bands.iter().fold((0.0, 0.0, 0.0), |(n, c, g), band| {
            (n + band.outcomes, c + band.carried, g + band.given)
        });
        let overall = if outcomes > 1.0e-9 {
            let given = (given / outcomes).clamp(EDGE, 1.0 - EDGE);
            let comes = (carried + PRIOR_STRENGTH * given) / (outcomes + PRIOR_STRENGTH);
            log_odds(comes) - logit(given)
        } else {
            0.0
        };
        let mut points = [(0.0, 0.0, 0.0); BANDS];
        let mut n = 0;
        for band in bands.iter().filter(|band| band.outcomes > 1.0e-9) {
            let at = band.log_odds / band.outcomes;
            let given = (band.given / band.outcomes).clamp(EDGE, 1.0 - EDGE);
            let expected = sigmoid(logit(given) + overall);
            let comes = (band.carried + PRIOR_STRENGTH * expected) / (band.outcomes + PRIOR_STRENGTH);
            points[n] = (at, at + log_odds(comes) - logit(given), band.outcomes);
            n += 1;
        }
        // Pool adjacent violators: blocks of (first point, mean, weight),
        // each running to the next block's first point.
        let mut blocks = [(0usize, 0.0, 0.0); BANDS];
        let mut m = 0;
        for (i, &(_, y, w)) in points[..n].iter().enumerate() {
            let mut block = (i, y, w);
            while m > 0 && blocks[m - 1].1 >= block.1 {
                let (first, mean, weight) = blocks[m - 1];
                let total = weight + block.2;
                block = (first, (mean * weight + block.1 * block.2) / total, total);
                m -= 1;
            }
            blocks[m] = block;
            m += 1;
        }
        for (b, &(first, mean, _)) in blocks[..m].iter().enumerate() {
            let end = if b + 1 < m { blocks[b + 1].0 } else { n };
            for (out, point) in self.corrected[first..end].iter_mut().zip(&points[first..end]) {
                *out = (point.0, mean);
            }
        }
        self.points = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_core::DetRng;

    const DAY: u64 = 86_400;

    #[test]
    fn it_keeps_the_chances_given_before_any_outcome() {
        let c = Calibration::default();
        for p in [0.001, 0.03, 0.2, 0.5, 0.8, 0.99] {
            assert!((c.apply(p) - p).abs() < 1.0e-9, "{p}");
        }
        assert_eq!((c.apply(0.0), c.apply(1.0)), (0.0, 1.0));
    }

    /// Chances of 30 % that come true one time in ten are read as 10 %; far
    /// from them, chances are corrected as their nearest band was, and the
    /// order of chances is kept.
    #[test]
    fn overconfident_chances_are_brought_down() {
        let mut c = Calibration::default();
        for n in 0..600u64 {
            c.observe(0.3, n % 10 == 0, n * 60);
        }

        assert!((c.apply(0.3) - 0.1).abs() < 0.01, "{}", c.apply(0.3));
        let grid: Vec<f64> = (1..100).map(|k| c.apply(f64::from(k) / 100.0)).collect();
        assert!(grid.windows(2).all(|w| w[0] <= w[1]), "{grid:?}");
    }

    /// Chances that come true as often as they say are left as they are.
    #[test]
    fn calibrated_chances_are_left_alone() {
        let mut c = Calibration::default();
        let mut rng = DetRng::from_seed(3);
        for n in 0..4_000u64 {
            let p = 0.02 + 0.96 * (rng.below(1_000) as f64 / 1_000.0);
            c.observe(p, rng.chance(p), n * 30);
        }
        for p in [0.05, 0.2, 0.5, 0.8, 0.95] {
            let out = c.apply(p);
            assert!((out - p).abs() < 0.05, "{p} -> {out}");
        }
    }

    /// Links that are either good or dead, given chances in between: the low
    /// chances (the dead links) go down and the high ones up, each by its own
    /// record, and a chance near neither is not pushed to the ends.
    #[test]
    fn each_band_is_corrected_by_its_own_record() {
        let mut c = Calibration::default();
        for n in 0..2_000u64 {
            let dead = n % 2 == 0;
            let p = if dead { 0.04 } else { 0.6 };
            c.observe(p, !dead && n % 10 != 1, n * 60);
        }

        assert!(c.apply(0.04) < 0.002, "{}", c.apply(0.04));
        assert!((c.apply(0.6) - 0.8).abs() < 0.02, "{}", c.apply(0.6));
        assert!(c.apply(0.97) < 0.99, "{}", c.apply(0.97));
    }

    /// A band with a short record follows how the rest of the record errs:
    /// chances of 5 % that never come true teach the station to give 3 %
    /// less, before any handoff given 3 % has ended.
    #[test]
    fn a_band_with_a_short_record_follows_the_rest() {
        let mut c = Calibration::default();
        for n in 0..200u64 {
            c.observe(0.05, false, n * 60);
        }
        c.observe(0.012, false, 200 * 60);

        assert!(c.apply(0.012) < 0.002, "{}", c.apply(0.012));
        assert!(c.apply(0.05) < 0.005, "{}", c.apply(0.05));
    }

    /// An old record counts for little against the outcomes that come
    /// after it.
    #[test]
    fn an_old_record_fades() {
        let mut c = Calibration::default();
        for n in 0..100u64 {
            c.observe(0.5, false, n);
        }
        assert!(c.apply(0.5) < 0.1, "{}", c.apply(0.5));
        c.observe(0.5, true, 100 + 60 * DAY);
        assert!(c.apply(0.5) > 0.4, "{}", c.apply(0.5));
    }

    #[test]
    fn the_record_round_trips_through_cbor() {
        let mut c = Calibration::default();
        c.observe(0.4, false, 100);
        let bytes = minicbor::to_vec(c.record()).unwrap();
        let back = Calibration::from_record(minicbor::decode(&bytes).unwrap());
        assert_eq!(back, c);
    }
}
