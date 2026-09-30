//! The figures each algorithm is scored on.

use super::{MessageState, RoutingAlgorithm, RoutingReport, RoutingScenario};
use crate::metrics::Percentiles;

#[derive(Default)]
pub(super) struct Counters {
    pub(super) payload_transmissions: u64,
    pub(super) payload_bytes: u64,
    pub(super) payload_airtime_ms: u64,
    pub(super) control_bytes: u64,
    pub(super) control_airtime_ms: u64,
    pub(super) duplicate_copies: u64,
    pub(super) custody_failures: u64,
    pub(super) storage_high_water_bytes: u64,
    pub(super) storage_high_water_objects: usize,
}

pub(super) fn update_storage(messages: &[MessageState], counters: &mut Counters) {
    let objects: usize = messages.iter().map(|message| message.holdings.len()).sum();
    let bytes = messages
        .iter()
        .map(|message| message.bundle.bytes.saturating_mul(message.holdings.len() as u64))
        .sum();
    counters.storage_high_water_objects = counters.storage_high_water_objects.max(objects);
    counters.storage_high_water_bytes = counters.storage_high_water_bytes.max(bytes);
}

pub(super) fn report(
    scenario: &RoutingScenario,
    algorithm: RoutingAlgorithm,
    messages: &[MessageState],
    counters: Counters,
) -> RoutingReport {
    let delivered = messages
        .iter()
        .filter(|message| message.delivered_at.is_some())
        .count();
    let latencies: Vec<u64> = messages
        .iter()
        .filter_map(|message| Some(message.delivered_at?.saturating_sub(message.bundle.created)))
        .collect();
    let source_fairness = source_fairness(scenario.nodes, messages);
    let forecasts: Vec<(f64, f64)> = messages
        .iter()
        .filter_map(|message| Some((message.prediction?, f64::from(message.delivered_at.is_some()))))
        .collect();
    let calibration_brier = (!forecasts.is_empty()).then(|| {
        forecasts
            .iter()
            .map(|(forecast, outcome)| (forecast - outcome).powi(2))
            .sum::<f64>()
            / forecasts.len() as f64
    });
    RoutingReport {
        algorithm,
        generated: messages.len(),
        delivered,
        delivery_ratio: if messages.is_empty() {
            1.0
        } else {
            delivered as f64 / messages.len() as f64
        },
        payload_transmissions: counters.payload_transmissions,
        payload_bytes_on_air: counters.payload_bytes,
        payload_airtime_ms: counters.payload_airtime_ms,
        control_bytes_on_air: counters.control_bytes,
        control_airtime_ms: counters.control_airtime_ms,
        duplicate_copies: counters.duplicate_copies,
        custody_failures: counters.custody_failures,
        storage_high_water_bytes: counters.storage_high_water_bytes,
        storage_high_water_objects: counters.storage_high_water_objects,
        latency_secs: Percentiles::of(&latencies),
        source_fairness,
        calibration_brier: matches!(
            algorithm,
            RoutingAlgorithm::BayesianCgr | RoutingAlgorithm::BayesianCgrLive
        )
        .then_some(calibration_brier)
        .flatten(),
    }
}

fn source_fairness(nodes: usize, messages: &[MessageState]) -> f64 {
    let mut generated = vec![0_u64; nodes];
    let mut delivered = vec![0_u64; nodes];
    for message in messages {
        generated[message.bundle.source] += 1;
        delivered[message.bundle.source] += u64::from(message.delivered_at.is_some());
    }
    let ratios: Vec<f64> = generated
        .iter()
        .zip(delivered)
        .filter(|(generated, _)| **generated > 0)
        .map(|(generated, delivered)| delivered as f64 / *generated as f64)
        .collect();
    if ratios.is_empty() {
        return 1.0;
    }
    let sum: f64 = ratios.iter().sum();
    let squares: f64 = ratios.iter().map(|ratio| ratio * ratio).sum();
    if squares == 0.0 {
        1.0
    } else {
        sum * sum / (ratios.len() as f64 * squares)
    }
}
