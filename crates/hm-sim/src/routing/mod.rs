//! Deterministic contact-trace comparison for delay-tolerant routing.
//!
//! Every algorithm sees the same contacts, capacities, bundles, and
//! algorithm-independent contact outcome draws. This complements the
//! frame-level simulator: it compares forwarding policy without giving one
//! policy a luckier loss trace.

mod algorithms;
mod report;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use hm_route::{Bearer, ContactGraph, ContactKey, ScheduledContact};
use hm_wire::Callsign;

use crate::metrics::Percentiles;
use crate::NodeId;
use algorithms::Model;
use report::{report, update_storage, Counters};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RoutingAlgorithm {
    /// Contact graph routing given the whole contact plan in advance.
    BayesianCgr,
    /// The same router knowing, as a station does, only the contacts open
    /// now (heard, linked, or advertised while they last), not when closed
    /// links will open again. Not in [`RoutingAlgorithm::comparison_set`]: it
    /// measures what the plan is worth on links that open and close.
    BayesianCgrLive,
    Epidemic,
    SprayAndWait {
        copies: u8,
    },
    Prophet,
    Meed,
}

impl RoutingAlgorithm {
    pub const fn comparison_set() -> [Self; 5] {
        [
            Self::BayesianCgr,
            Self::Epidemic,
            Self::SprayAndWait { copies: 2 },
            Self::Prophet,
            Self::Meed,
        ]
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ContactOpportunity {
    pub start: u64,
    pub end: u64,
    pub from: NodeId,
    pub to: NodeId,
    pub bearer: Bearer,
    pub rate_bps: u32,
    pub capacity_bytes: u64,
    pub success_permyriad: u16,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SimBundle {
    pub id: u64,
    pub source: NodeId,
    pub destination: NodeId,
    pub created: u64,
    pub ttl_secs: u64,
    pub bytes: u64,
    pub urgent: bool,
}

impl SimBundle {
    pub fn expires_at(self) -> u64 {
        self.created.saturating_add(self.ttl_secs)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingScenario {
    pub nodes: usize,
    pub contacts: Vec<ContactOpportunity>,
    pub bundles: Vec<SimBundle>,
    pub seed: u64,
    pub transfer_overhead_bytes: u64,
    pub txdelay_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutingReport {
    pub algorithm: RoutingAlgorithm,
    pub generated: usize,
    pub delivered: usize,
    pub delivery_ratio: f64,
    pub payload_transmissions: u64,
    pub payload_bytes_on_air: u64,
    pub payload_airtime_ms: u64,
    pub control_bytes_on_air: u64,
    pub control_airtime_ms: u64,
    pub duplicate_copies: u64,
    pub custody_failures: u64,
    pub storage_high_water_bytes: u64,
    pub storage_high_water_objects: usize,
    pub latency_secs: Option<Percentiles>,
    pub source_fairness: f64,
    /// Brier score of Bayesian CGR's first route forecast against delivery
    /// before TTL. `None` for non-probabilistic baselines or no forecasts.
    pub calibration_brier: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutingComparison {
    pub reports: Vec<RoutingReport>,
}

impl RoutingComparison {
    pub fn report(&self, algorithm: RoutingAlgorithm) -> Option<&RoutingReport> {
        self.reports.iter().find(|report| report.algorithm == algorithm)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutingSimError {
    Invalid(&'static str),
    Route(String),
}

impl std::fmt::Display for RoutingSimError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Route(message) => write!(formatter, "contact graph: {message}"),
        }
    }
}

impl std::error::Error for RoutingSimError {}

pub fn compare_routing(scenario: &RoutingScenario) -> Result<RoutingComparison, RoutingSimError> {
    validate(scenario)?;
    let mut reports = Vec::new();
    for algorithm in RoutingAlgorithm::comparison_set() {
        reports.push(run_routing(scenario, algorithm)?);
    }
    Ok(RoutingComparison { reports })
}

pub fn run_routing(
    scenario: &RoutingScenario,
    algorithm: RoutingAlgorithm,
) -> Result<RoutingReport, RoutingSimError> {
    validate(scenario)?;
    if matches!(algorithm, RoutingAlgorithm::SprayAndWait { copies: 0 }) {
        return Err(RoutingSimError::Invalid("Spray-and-Wait needs at least one copy"));
    }
    let mut contacts: Vec<(usize, ContactOpportunity)> =
        scenario.contacts.iter().copied().enumerate().collect();
    contacts.sort_by_key(|(index, contact)| (contact.start, *index));
    let mut model = Model::new(scenario, algorithm)?;
    let mut messages: Vec<MessageState> = scenario
        .bundles
        .iter()
        .copied()
        .map(|bundle| MessageState::new(bundle, algorithm))
        .collect();
    let mut counters = Counters::default();
    update_storage(&messages, &mut counters);

    for (contact_index, contact) in contacts {
        model.observe_contact(contact, contact_index, scenario.nodes);
        let control_bytes = model.control_bytes(contact, &messages, scenario.nodes);
        counters.control_bytes = counters.control_bytes.saturating_add(control_bytes);
        counters.control_airtime_ms = counters.control_airtime_ms.saturating_add(airtime_ms(
            control_bytes,
            contact.rate_bps,
            scenario.txdelay_ms,
        ));
        let mut capacity = contact.capacity_bytes.saturating_sub(control_bytes);
        let mut order: Vec<usize> = (0..messages.len()).collect();
        order.sort_by_key(|index| {
            let bundle = messages[*index].bundle;
            (!bundle.urgent, bundle.expires_at(), bundle.id)
        });
        for message_index in order {
            let message = &messages[message_index];
            if message.delivered_at.is_some()
                || contact.start < message.bundle.created
                || contact.start >= message.bundle.expires_at()
                || !message.holdings.contains_key(&contact.from)
                || message.holdings.contains_key(&contact.to)
            {
                continue;
            }
            let Some(decision) = model.decision(message, contact, contact_index, scenario.nodes) else {
                continue;
            };
            let bytes_on_air = message
                .bundle
                .bytes
                .saturating_add(scenario.transfer_overhead_bytes);
            if bytes_on_air > capacity {
                continue;
            }
            capacity -= bytes_on_air;
            counters.payload_transmissions += 1;
            counters.payload_bytes = counters.payload_bytes.saturating_add(bytes_on_air);
            counters.payload_airtime_ms = counters.payload_airtime_ms.saturating_add(airtime_ms(
                bytes_on_air,
                contact.rate_bps,
                scenario.txdelay_ms,
            ));
            let success = succeeds(
                scenario.seed,
                contact_index,
                message.bundle.id,
                contact.from,
                contact.to,
                contact.success_permyriad,
            );
            model.outcome(contact, success);
            if !success {
                counters.custody_failures += 1;
                continue;
            }
            let message = &mut messages[message_index];
            if message.prediction.is_none() {
                message.prediction = decision.prediction;
            }
            if contact.to == message.bundle.destination {
                message.delivered_at = Some(contact.start);
                message.holdings.clear();
                continue;
            }
            let sender_tokens = message.holdings.get(&contact.from).copied().unwrap_or(1);
            if decision.retain_sender {
                message
                    .holdings
                    .insert(contact.to, decision.receiver_tokens.max(1));
                if decision.sender_tokens > 0 {
                    message.holdings.insert(contact.from, decision.sender_tokens);
                } else {
                    message.holdings.insert(contact.from, sender_tokens);
                }
                counters.duplicate_copies += 1;
            } else {
                message.holdings.remove(&contact.from);
                message
                    .holdings
                    .insert(contact.to, decision.receiver_tokens.max(1));
            }
        }
        update_storage(&messages, &mut counters);
    }
    Ok(report(scenario, algorithm, &messages, counters))
}

fn validate(scenario: &RoutingScenario) -> Result<(), RoutingSimError> {
    if scenario.nodes < 2 {
        return Err(RoutingSimError::Invalid("scenario needs at least two nodes"));
    }
    let mut ids = BTreeSet::new();
    for bundle in &scenario.bundles {
        if bundle.source >= scenario.nodes
            || bundle.destination >= scenario.nodes
            || bundle.source == bundle.destination
            || bundle.ttl_secs == 0
            || bundle.bytes == 0
            || !ids.insert(bundle.id)
        {
            return Err(RoutingSimError::Invalid("invalid or duplicate bundle"));
        }
    }
    for contact in &scenario.contacts {
        if contact.from >= scenario.nodes
            || contact.to >= scenario.nodes
            || contact.from == contact.to
            || contact.end <= contact.start
            || contact.rate_bps == 0
            || contact.capacity_bytes == 0
            || contact.success_permyriad > 10_000
        {
            return Err(RoutingSimError::Invalid("invalid contact"));
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct MessageState {
    bundle: SimBundle,
    holdings: BTreeMap<NodeId, u8>,
    delivered_at: Option<u64>,
    prediction: Option<f64>,
}

impl MessageState {
    fn new(bundle: SimBundle, algorithm: RoutingAlgorithm) -> Self {
        let tokens = match algorithm {
            RoutingAlgorithm::SprayAndWait { copies } => copies,
            _ => 1,
        };
        Self {
            bundle,
            holdings: BTreeMap::from([(bundle.source, tokens)]),
            delivered_at: None,
            prediction: None,
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct Decision {
    retain_sender: bool,
    sender_tokens: u8,
    receiver_tokens: u8,
    prediction: Option<f64>,
}

impl Decision {
    const fn custody(prediction: Option<f64>) -> Self {
        Self {
            retain_sender: false,
            sender_tokens: 0,
            receiver_tokens: 1,
            prediction,
        }
    }

    const fn replicate(prediction: Option<f64>) -> Self {
        Self {
            retain_sender: true,
            sender_tokens: 0,
            receiver_tokens: 1,
            prediction,
        }
    }
}

fn add_contact(graph: &mut ContactGraph, contact: ContactOpportunity) -> Result<ContactKey, RoutingSimError> {
    graph
        .add_schedule(ScheduledContact {
            from: station(contact.from)?,
            to: station(contact.to)?,
            bearer: contact.bearer,
            start: contact.start,
            end: contact.end,
            rate_bps: contact.rate_bps,
            capacity_bytes: contact.capacity_bytes,
            success_permyriad: Some(contact.success_permyriad),
            flags: 0,
        })
        .map_err(|error| RoutingSimError::Route(error.to_string()))
}

fn station(node: NodeId) -> Result<Callsign, RoutingSimError> {
    if node > 9_999 {
        return Err(RoutingSimError::Invalid(
            "routing comparison supports at most 10,000 nodes",
        ));
    }
    Callsign::parse(&format!("N{node:04}"))
        .map_err(|_| RoutingSimError::Invalid("could not map node to callsign"))
}

fn airtime_ms(bytes: u64, bitrate_bps: u32, txdelay_ms: u64) -> u64 {
    if bytes == 0 {
        0
    } else {
        txdelay_ms.saturating_add(
            bytes
                .saturating_mul(8)
                .saturating_mul(1_000)
                .div_ceil(u64::from(bitrate_bps)),
        )
    }
}

fn succeeds(seed: u64, contact_index: usize, bundle: u64, from: NodeId, to: NodeId, permyriad: u16) -> bool {
    let mut value = seed
        ^ (contact_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ bundle.rotate_left(17)
        ^ (from as u64).rotate_left(31)
        ^ (to as u64).rotate_left(47);
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    value % 10_000 < u64::from(permyriad)
}
