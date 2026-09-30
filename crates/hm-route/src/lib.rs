//! Contact plan and delay-tolerant route selection.
//!
//! [`ContactGraph`] holds the contacts known as windows (schedules, sessions
//! up now, links that could be tried, adverts) and the links the station has
//! seen exist. [`plan_routes`] picks the route of greatest expected utility,
//! now or later: it asks the station's beliefs ([`hm_model::Estimate`]) how
//! likely each hop is to work when it would be taken, by posterior mean for a
//! deterministic plan, or by a Thompson draw so that uncertain links are
//! explored in proportion to the chance that they are the best.

mod contact;
mod graph;
mod routing;

pub use contact::{
    BeaconObservation, Bearer, Contact, ContactKey, ContactSource, KnownLink, LiveContact, ScheduledContact,
};
pub use graph::{ContactGraph, GraphConfig, GraphError, Merge};
pub use routing::{
    plan_routes, Route, RouteError, RouteHop, RoutePlan, RouteRequest, RoutingPolicy, URGENT_HALF_LIFE,
};
