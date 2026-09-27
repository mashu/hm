//! Deterministic contact-trace comparison for delay-tolerant routing.
//!
//! Every algorithm sees the same contacts, capacities, bundles, and
//! algorithm-independent contact outcome draws. This complements the
//! frame-level simulator: it compares forwarding policy without giving one
//! policy a luckier loss trace.

use std::collections::{BTreeMap, BTreeSet};

use hm_route::{
    plan_routes, Bearer, ContactGraph, ContactKey, GraphConfig, RouteRequest, RoutingPolicy, ScheduledContact,
};
use hm_wire::Callsign;

use crate::metrics::Percentiles;
use crate::NodeId;

const PROPHET_P_INIT: f64 = 0.75;
const PROPHET_BETA: f64 = 0.25;
const PROPHET_GAMMA_PER_HOUR: f64 = 0.98;
const PROPHET_FORWARD_MARGIN: f64 = 0.05;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RoutingAlgorithm {
    BayesianCgr,
    Epidemic,
    SprayAndWait { copies: u8 },
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

impl RoutingReport {
    pub fn airtime_per_delivered_byte(&self, delivered_payload_bytes: u64) -> Option<f64> {
        (delivered_payload_bytes > 0).then(|| {
            (self.payload_airtime_ms + self.control_airtime_ms) as f64 / delivered_payload_bytes as f64
        })
    }
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
        model.observe_contact(contact, scenario.nodes);
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
            model.outcome(message.bundle, contact, success);
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

enum Model {
    BayesianCgr {
        graph: ContactGraph,
        contacts: Vec<ContactKey>,
        advertised: BTreeSet<(NodeId, NodeId, Bearer)>,
    },
    Epidemic,
    SprayAndWait,
    Prophet(ProphetState),
    Meed(MeedState),
}

impl Model {
    fn new(scenario: &RoutingScenario, algorithm: RoutingAlgorithm) -> Result<Self, RoutingSimError> {
        Ok(match algorithm {
            RoutingAlgorithm::BayesianCgr => {
                let mut graph = ContactGraph::new(GraphConfig::default())
                    .map_err(|error| RoutingSimError::Route(error.to_string()))?;
                let mut contacts = Vec::with_capacity(scenario.contacts.len());
                for contact in &scenario.contacts {
                    contacts.push(
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
                            .map_err(|error| RoutingSimError::Route(error.to_string()))?,
                    );
                }
                Self::BayesianCgr {
                    graph,
                    contacts,
                    advertised: BTreeSet::new(),
                }
            }
            RoutingAlgorithm::Epidemic => Self::Epidemic,
            RoutingAlgorithm::SprayAndWait { .. } => Self::SprayAndWait,
            RoutingAlgorithm::Prophet => Self::Prophet(ProphetState::new(scenario.nodes)),
            RoutingAlgorithm::Meed => Self::Meed(MeedState::default()),
        })
    }

    fn observe_contact(&mut self, contact: ContactOpportunity, nodes: usize) {
        match self {
            Self::Prophet(state) => state.observe(contact, nodes),
            Self::Meed(state) => state.observe(contact),
            _ => {}
        }
    }

    fn control_bytes(&mut self, contact: ContactOpportunity, messages: &[MessageState], nodes: usize) -> u64 {
        let held = messages
            .iter()
            .filter(|message| message.holdings.contains_key(&contact.from))
            .count() as u64;
        match self {
            Self::BayesianCgr { advertised, .. } => {
                let first = advertised.insert((contact.from, contact.to, contact.bearer));
                16 + u64::from(first) * 101
            }
            Self::Epidemic => 16 + held.saturating_mul(8),
            Self::SprayAndWait => 8,
            Self::Prophet(_) => 16 + nodes as u64 * 2,
            Self::Meed(state) => 16 + state.edges.len() as u64 * 12,
        }
    }

    fn decision(
        &mut self,
        message: &MessageState,
        contact: ContactOpportunity,
        contact_index: usize,
        nodes: usize,
    ) -> Option<Decision> {
        if contact.to == message.bundle.destination {
            return Some(Decision::custody(None));
        }
        match self {
            Self::Epidemic => Some(Decision::replicate(None)),
            Self::SprayAndWait => {
                let tokens = message.holdings.get(&contact.from).copied().unwrap_or(1);
                if tokens <= 1 {
                    return None;
                }
                let receiver = tokens / 2;
                Some(Decision {
                    retain_sender: true,
                    sender_tokens: tokens - receiver,
                    receiver_tokens: receiver,
                    prediction: None,
                })
            }
            Self::Prophet(state) => {
                let destination = message.bundle.destination;
                (state.probability(contact.to, destination)
                    > state.probability(contact.from, destination) + PROPHET_FORWARD_MARGIN)
                    .then(|| Decision::replicate(None))
            }
            Self::Meed(state) => {
                let destination = message.bundle.destination;
                let sender = state.distance(contact.from, destination, nodes);
                let receiver = state.distance(contact.to, destination, nodes);
                (receiver < sender).then(|| Decision::custody(None))
            }
            Self::BayesianCgr { graph, contacts, .. } => {
                graph.prune(contact.start);
                let request = RouteRequest {
                    source: station(contact.from).ok()?,
                    destination: station(message.bundle.destination).ok()?,
                    now: contact.start,
                    expires_at: message.bundle.expires_at(),
                    object_bytes: message.bundle.bytes,
                    max_hops: (nodes.saturating_sub(1).min(16)) as u8,
                    airtime_budget_millis: u64::MAX / 4,
                    visited: &[],
                    excluded_contacts: &[],
                    urgent: message.bundle.urgent,
                };
                let plan = plan_routes(graph, &request, RoutingPolicy::default()).ok()?;
                let current = *contacts.get(contact_index)?;
                if !plan
                    .active
                    .iter()
                    .any(|route| route.hops.first().is_some_and(|hop| hop.contact == current))
                {
                    return None;
                }
                let prediction = Some(plan.combined_success_probability);
                if message.bundle.urgent && message.holdings.len() < 2 {
                    Some(Decision::replicate(prediction))
                } else {
                    Some(Decision::custody(prediction))
                }
            }
        }
    }

    fn outcome(&mut self, bundle: SimBundle, contact: ContactOpportunity, success: bool) {
        if let Self::BayesianCgr { graph, .. } = self {
            graph.record_delivery(
                station(contact.from).expect("validated node id"),
                station(contact.to).expect("validated node id"),
                contact.bearer,
                success,
                contact.start,
            );
            if success {
                let _ = bundle;
            }
        }
    }
}

struct ProphetState {
    probabilities: Vec<Vec<f64>>,
    last_age: u64,
}

impl ProphetState {
    fn new(nodes: usize) -> Self {
        let mut probabilities = vec![vec![0.0; nodes]; nodes];
        for (node, row) in probabilities.iter_mut().enumerate() {
            row[node] = 1.0;
        }
        Self {
            probabilities,
            last_age: 0,
        }
    }

    fn observe(&mut self, contact: ContactOpportunity, nodes: usize) {
        let elapsed = contact.start.saturating_sub(self.last_age);
        let age = PROPHET_GAMMA_PER_HOUR.powf(elapsed as f64 / 3_600.0);
        for row in &mut self.probabilities {
            for probability in row {
                *probability *= age;
            }
        }
        self.last_age = contact.start;
        let from = contact.from;
        let to = contact.to;
        self.probabilities[from][to] += (1.0 - self.probabilities[from][to]) * PROPHET_P_INIT;
        let via = self.probabilities[from][to];
        for destination in 0..nodes {
            if destination == from || destination == to {
                continue;
            }
            let transitive = via * self.probabilities[to][destination] * PROPHET_BETA;
            self.probabilities[from][destination] +=
                (1.0 - self.probabilities[from][destination]) * transitive;
        }
    }

    fn probability(&self, from: NodeId, destination: NodeId) -> f64 {
        self.probabilities[from][destination]
    }
}

#[derive(Copy, Clone, Debug)]
struct Encounter {
    last: u64,
    mean_secs: f64,
    samples: u64,
}

#[derive(Default)]
struct MeedState {
    edges: BTreeMap<(NodeId, NodeId), Encounter>,
}

impl MeedState {
    fn observe(&mut self, contact: ContactOpportunity) {
        self.edges
            .entry((contact.from, contact.to))
            .and_modify(|edge| {
                let interval = contact.start.saturating_sub(edge.last) as f64;
                edge.samples += 1;
                edge.mean_secs += (interval - edge.mean_secs) / edge.samples as f64;
                edge.last = contact.start;
            })
            .or_insert(Encounter {
                last: contact.start,
                mean_secs: 3_600.0,
                samples: 1,
            });
    }

    fn distance(&self, source: NodeId, destination: NodeId, nodes: usize) -> f64 {
        if source == destination {
            return 0.0;
        }
        let mut distance = vec![f64::INFINITY; nodes];
        let mut visited = vec![false; nodes];
        distance[source] = 0.0;
        for _ in 0..nodes {
            let Some(node) = (0..nodes)
                .filter(|node| !visited[*node])
                .min_by(|left, right| distance[*left].total_cmp(&distance[*right]))
            else {
                break;
            };
            if !distance[node].is_finite() {
                break;
            }
            visited[node] = true;
            for (&(from, to), encounter) in &self.edges {
                if from == node {
                    distance[to] = distance[to].min(distance[node] + encounter.mean_secs);
                }
            }
        }
        distance[destination]
    }
}

#[derive(Default)]
struct Counters {
    payload_transmissions: u64,
    payload_bytes: u64,
    payload_airtime_ms: u64,
    control_bytes: u64,
    control_airtime_ms: u64,
    duplicate_copies: u64,
    custody_failures: u64,
    storage_high_water_bytes: u64,
    storage_high_water_objects: usize,
}

fn update_storage(messages: &[MessageState], counters: &mut Counters) {
    let objects: usize = messages.iter().map(|message| message.holdings.len()).sum();
    let bytes = messages
        .iter()
        .map(|message| message.bundle.bytes.saturating_mul(message.holdings.len() as u64))
        .sum();
    counters.storage_high_water_objects = counters.storage_high_water_objects.max(objects);
    counters.storage_high_water_bytes = counters.storage_high_water_bytes.max(bytes);
}

fn report(
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
        calibration_brier: matches!(algorithm, RoutingAlgorithm::BayesianCgr)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(start: u64, from: usize, to: usize) -> ContactOpportunity {
        ContactOpportunity {
            start,
            end: start + 10,
            from,
            to,
            bearer: Bearer::Radio,
            rate_bps: 9_600,
            capacity_bytes: 64 * 1024,
            success_permyriad: 10_000,
        }
    }

    fn line() -> RoutingScenario {
        let mut contacts = Vec::new();
        for cycle in 0..4 {
            let base = cycle * 100;
            contacts.extend([
                contact(base + 10, 0, 1),
                contact(base + 20, 1, 2),
                contact(base + 30, 2, 3),
                contact(base + 40, 1, 0),
                contact(base + 50, 2, 1),
                contact(base + 60, 3, 2),
            ]);
        }
        RoutingScenario {
            nodes: 4,
            contacts,
            bundles: (0..8)
                .map(|id| SimBundle {
                    id,
                    source: 0,
                    destination: 3,
                    created: 0,
                    ttl_secs: 500,
                    bytes: 256,
                    urgent: false,
                })
                .collect(),
            seed: 7,
            transfer_overhead_bytes: 80,
            txdelay_ms: 100,
        }
    }

    #[test]
    fn all_algorithms_run_on_the_same_trace_deterministically() {
        let scenario = line();
        let first = compare_routing(&scenario).unwrap();
        let second = compare_routing(&scenario).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.reports.len(), 5);
        assert!(first
            .reports
            .iter()
            .all(|report| report.delivery_ratio >= 0.0 && report.delivery_ratio <= 1.0));
    }

    #[test]
    fn single_copy_cgr_uses_less_storage_than_the_flood_upper_bound() {
        let comparison = compare_routing(&line()).unwrap();
        let cgr = comparison.report(RoutingAlgorithm::BayesianCgr).unwrap();
        let flood = comparison.report(RoutingAlgorithm::Epidemic).unwrap();
        assert_eq!(cgr.delivered, cgr.generated);
        assert_eq!(flood.delivered, flood.generated);
        assert!(cgr.storage_high_water_objects < flood.storage_high_water_objects);
        assert!(cgr.duplicate_copies < flood.duplicate_copies);
        assert!(cgr.calibration_brier.is_some());
        let active_copy_bound =
            line().bundles.len() + line().bundles.iter().filter(|bundle| bundle.urgent).count();
        assert!(cgr.storage_high_water_objects <= active_copy_bound);
    }

    #[test]
    fn spray_copy_count_and_invalid_inputs_are_bounded() {
        let report = run_routing(&line(), RoutingAlgorithm::SprayAndWait { copies: 2 }).unwrap();
        assert!(report.storage_high_water_objects <= report.generated * 2);
        assert!(run_routing(&line(), RoutingAlgorithm::SprayAndWait { copies: 0 }).is_err());
        let mut invalid = line();
        invalid.contacts[0].success_permyriad = 10_001;
        assert!(compare_routing(&invalid).is_err());
    }
}
