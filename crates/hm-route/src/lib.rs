//! Bayesian directed contact graph and delay-tolerant route selection.
//!
//! Link evidence decays over time and route scoring uses a conservative Beta
//! posterior quantile. Payload routing is deterministic; random exploration
//! remains a bearer-selection concern.

mod beta;
mod graph;
mod routing;

pub use graph::{
    BeaconObservation, Bearer, Contact, ContactGraph, ContactKey, ContactSource, EdgeKey, Evidence,
    GraphConfig, GraphError, LiveContact, Merge, ScheduledContact,
};
pub use routing::{
    plan_routes, release_active, reserve_active, Route, RouteError, RouteHop, RoutePlan, RouteRequest,
    RoutingPolicy,
};
