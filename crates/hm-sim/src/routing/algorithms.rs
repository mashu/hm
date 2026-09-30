//! What each algorithm decides when a contact opens: contact graph routing
//! on the stations' beliefs, and the baselines it is compared against.

use std::collections::{BTreeMap, BTreeSet};

use hm_model::{Beliefs, CustodianObservation, LinkObservation};
use hm_route::{plan_routes, Bearer, ContactGraph, ContactKey, GraphConfig, RouteRequest, RoutingPolicy};

use super::{
    add_contact, station, ContactOpportunity, Decision, MessageState, RoutingAlgorithm, RoutingScenario,
    RoutingSimError,
};
use crate::NodeId;

const PROPHET_P_INIT: f64 = 0.75;
const PROPHET_BETA: f64 = 0.25;
const PROPHET_GAMMA_PER_HOUR: f64 = 0.98;
const PROPHET_FORWARD_MARGIN: f64 = 0.05;

pub(super) enum Model {
    BayesianCgr {
        graph: Box<ContactGraph>,
        /// What the stations have learned about links from handoffs.
        beliefs: Box<Beliefs>,
        /// Each scenario contact's key, once the graph knows it.
        contacts: Vec<Option<ContactKey>>,
        advertised: BTreeSet<(NodeId, NodeId, Bearer)>,
        /// Contacts become known only when they open.
        live: bool,
    },
    Epidemic,
    SprayAndWait,
    Prophet(ProphetState),
    Meed(MeedState),
}

impl Model {
    pub(super) fn new(
        scenario: &RoutingScenario,
        algorithm: RoutingAlgorithm,
    ) -> Result<Self, RoutingSimError> {
        Ok(match algorithm {
            RoutingAlgorithm::BayesianCgr | RoutingAlgorithm::BayesianCgrLive => {
                let live = algorithm == RoutingAlgorithm::BayesianCgrLive;
                let mut graph = ContactGraph::new(GraphConfig::default())
                    .map_err(|error| RoutingSimError::Route(error.to_string()))?;
                let mut contacts = vec![None; scenario.contacts.len()];
                if !live {
                    for (index, contact) in scenario.contacts.iter().enumerate() {
                        contacts[index] = Some(add_contact(&mut graph, *contact)?);
                    }
                }
                Self::BayesianCgr {
                    graph: Box::new(graph),
                    beliefs: Box::new(Beliefs::new()),
                    contacts,
                    advertised: BTreeSet::new(),
                    live,
                }
            }
            RoutingAlgorithm::Epidemic => Self::Epidemic,
            RoutingAlgorithm::SprayAndWait { .. } => Self::SprayAndWait,
            RoutingAlgorithm::Prophet => Self::Prophet(ProphetState::new(scenario.nodes)),
            RoutingAlgorithm::Meed => Self::Meed(MeedState::default()),
        })
    }

    pub(super) fn observe_contact(
        &mut self,
        contact: ContactOpportunity,
        contact_index: usize,
        nodes: usize,
    ) {
        match self {
            Self::Prophet(state) => state.observe(contact, nodes),
            Self::Meed(state) => state.observe(contact),
            Self::BayesianCgr {
                graph,
                contacts,
                live: true,
                ..
            } => {
                // Every station learns of the contact as it opens: more than
                // a real station knows, which hears only its own and its
                // neighbours' adverts.
                contacts[contact_index] = add_contact(graph, contact).ok();
            }
            _ => {}
        }
    }

    pub(super) fn control_bytes(
        &mut self,
        contact: ContactOpportunity,
        messages: &[MessageState],
        nodes: usize,
    ) -> u64 {
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

    pub(super) fn decision(
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
            Self::BayesianCgr {
                graph,
                beliefs,
                contacts,
                ..
            } => {
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
                    forbidden: hm_model::PerBearer::default(),
                    closed_now: &[],
                    urgent: message.bundle.urgent,
                    hearing_interval_secs: 0,
                };
                let mut estimate = beliefs.mean(contact.start);
                let plan = plan_routes(graph, &mut estimate, &request, RoutingPolicy::default()).ok()?;
                let current = (*contacts.get(contact_index)?)?;
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

    pub(super) fn outcome(&mut self, contact: ContactOpportunity, success: bool) {
        if let Self::BayesianCgr { beliefs, .. } = self {
            let link = hm_model::LinkKey {
                from: station(contact.from).expect("validated node id"),
                to: station(contact.to).expect("validated node id"),
                bearer: contact.bearer,
            };
            beliefs.observe_link(link, contact.start, LinkObservation::Handoff { ok: success });
            if success {
                beliefs.observe_custodian(link.to, contact.start, CustodianObservation::Accepted);
            }
        }
    }
}

pub(super) struct ProphetState {
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
pub(super) struct MeedState {
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
