//! Contact plan and delay-tolerant route selection.
//!
//! [`ContactGraph`] holds which contacts exist when (schedules, links seen
//! live, links that could be tried, adverts). [`plan_routes`] picks the route
//! of greatest expected utility, asking the station's beliefs
//! ([`hm_model::Estimate`]) how likely each hop is to work: by posterior mean
//! for a deterministic plan, or by a Thompson draw so that uncertain links
//! are explored in proportion to the chance that they are the best.

mod graph;
mod routing;

pub use graph::{
    BeaconObservation, Bearer, Contact, ContactGraph, ContactKey, ContactSource, GraphConfig, GraphError,
    LiveContact, Merge, ScheduledContact,
};
pub use routing::{
    plan_routes, release_active, reserve_active, Route, RouteError, RouteHop, RoutePlan, RouteRequest,
    RoutingPolicy, URGENT_HALF_LIFE,
};
