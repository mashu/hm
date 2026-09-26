//! Delivery and latency metrics computed from simulation events.

use std::collections::BTreeMap;

use hm_core::Millis;

use crate::NodeId;

/// Nearest-rank percentiles of a sample, in the sample's unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Percentiles {
    pub count: usize,
    pub min: u64,
    pub p50: u64,
    pub p90: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

impl Percentiles {
    /// `None` for an empty sample.
    pub fn of(samples: &[u64]) -> Option<Percentiles> {
        if samples.is_empty() {
            return None;
        }
        let mut s = samples.to_vec();
        s.sort_unstable();
        let rank = |p: u64| s[((p as usize * s.len()).div_ceil(100)).max(1) - 1];
        Some(Percentiles {
            count: s.len(),
            min: s[0],
            p50: rank(50),
            p90: rank(90),
            p95: rank(95),
            p99: rank(99),
            max: s[s.len() - 1],
        })
    }
}

/// Tracks end-to-end delivery of objects (bundles, chat lines, ...) to the
/// stations that should receive them.
#[derive(Clone, Debug)]
pub struct DeliveryTracker<K: Ord + Clone> {
    sent: BTreeMap<K, Millis>,
    expected: BTreeMap<(K, NodeId), Option<Millis>>,
    duplicates: u64,
    unexpected: u64,
}

impl<K: Ord + Clone> Default for DeliveryTracker<K> {
    fn default() -> Self {
        DeliveryTracker {
            sent: BTreeMap::new(),
            expected: BTreeMap::new(),
            duplicates: 0,
            unexpected: 0,
        }
    }
}

impl<K: Ord + Clone> DeliveryTracker<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Object `key` was handed to the network at `at`, for `recipients`.
    pub fn sent(&mut self, key: K, at: Millis, recipients: &[NodeId]) {
        self.sent.insert(key.clone(), at);
        for &r in recipients {
            self.expected.entry((key.clone(), r)).or_insert(None);
        }
    }

    /// `node`'s application received `key` at `at`.
    pub fn delivered(&mut self, key: K, node: NodeId, at: Millis) {
        match self.expected.get_mut(&(key, node)) {
            Some(slot @ None) => *slot = Some(at),
            Some(Some(_)) => self.duplicates += 1,
            None => self.unexpected += 1,
        }
    }

    /// Share of (object, recipient) pairs delivered, in `[0, 1]`; 1 when nothing was expected.
    pub fn ratio(&self) -> f64 {
        if self.expected.is_empty() {
            return 1.0;
        }
        let done = self.expected.values().filter(|v| v.is_some()).count();
        done as f64 / self.expected.len() as f64
    }

    /// Latency from send to delivery, in milliseconds, over delivered pairs.
    pub fn latency(&self) -> Option<Percentiles> {
        let samples: Vec<u64> = self
            .expected
            .iter()
            .filter_map(|((k, _), at)| Some(at.as_ref()?.0 - self.sent.get(k)?.0))
            .collect();
        Percentiles::of(&samples)
    }

    /// Deliveries of something already delivered to the same station.
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    /// Deliveries to stations that were not recipients (or of unknown objects).
    pub fn unexpected(&self) -> u64 {
        self.unexpected
    }

    /// (object, recipient) pairs still missing.
    pub fn missing(&self) -> Vec<(K, NodeId)> {
        self.expected
            .iter()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let s: Vec<u64> = (1..=100).collect();
        let p = Percentiles::of(&s).unwrap();
        assert_eq!(
            (p.min, p.p50, p.p90, p.p95, p.p99, p.max),
            (1, 50, 90, 95, 99, 100)
        );
        let p = Percentiles::of(&[7]).unwrap();
        assert_eq!((p.p50, p.p99), (7, 7));
        assert_eq!(Percentiles::of(&[]), None);
        let p = Percentiles::of(&[30, 10, 20]).unwrap();
        assert_eq!((p.min, p.p50, p.max), (10, 20, 30));
    }

    #[test]
    fn tracker_counts_ratio_latency_and_duplicates() {
        let mut t = DeliveryTracker::new();
        t.sent("a", Millis(100), &[1, 2]);
        t.sent("b", Millis(200), &[1]);
        t.delivered("a", 1, Millis(150));
        t.delivered("a", 1, Millis(170));
        t.delivered("b", 1, Millis(1200));
        t.delivered("b", 3, Millis(1300));
        assert!((t.ratio() - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(t.duplicates(), 1);
        assert_eq!(t.unexpected(), 1);
        assert_eq!(t.missing(), vec![("a", 2)]);
        let l = t.latency().unwrap();
        assert_eq!((l.count, l.min, l.max), (2, 50, 1000));
    }
}
