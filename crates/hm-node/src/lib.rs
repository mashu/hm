//! A station's decisions, without I/O.
//!
//! [`Node`] is the station as a state machine: what happens goes in as an
//! [`Input`] with the time it happened, and what the station wants done comes
//! out as [`Command`]s (radio commands, internet and modem transfers, SYNC to
//! internet peers). It owns the contact plan, what the station believes about
//! its links, custodians and channel ([`hm_model::Beliefs`]), the control
//! plane and the messages in flight, and keeps its memory in a
//! [`hm_store::Store`].
//!
//! Nothing here reads a clock, opens a socket or starts a thread. The daemon
//! (`hm node`) is a thin shell that feeds a node real events and carries out
//! its commands; a simulation feeds many nodes simulated ones, and runs weeks
//! of HF propagation in seconds.
//!
//! [`Radio`] is the radio side the same way: the transfer engine, beacons,
//! the stations heard and SYNC frames under the control airtime budget, with
//! frames in and out. [`Station`] puts the two together as one machine, for
//! simulators.
//!
//! - [`accept`](mod@accept): the one gate for everything that arrives, for delivery here
//!   or relay custody;
//! - [`control`]: contact adverts, holdings reconciliation, the control
//!   airtime budget;
//! - [`heard`]: stations heard on the radio and their beacons;
//! - [`rf_policy`]: who may use this station's airtime;
//! - [`adverts`]: our signed contact adverts;
//! - [`message`]: received objects, opened and checked against trusted keys;
//! - [`insight`]: what the station knows, as a snapshot for people
//!   ([`Node::insight`]).

pub mod accept;
pub mod adverts;
mod bearer;
pub mod control;
pub mod heard;
pub mod insight;
mod log;
pub mod message;
mod node;
mod radio;
pub mod rf_policy;
mod settings;
mod station;
mod trust;

pub use accept::{accept, Acceptance, AcceptanceGate};
pub use bearer::{ChannelSeen, RadioCmd, RadioEvt, Transfer};
pub use log::{addressed_to_us, as_station, log, set_log, short, utc_clock, Notify};
pub use node::{Command, Input, ModemSpec, Node, NodeIdentity, NodeStatus};
pub use radio::{LinkTiming, Radio, RadioSpec};
pub use settings::{Costs, RelaySettings, Settings};
pub use station::{Station, StationCmd, StationEvent, StationSpec};
pub use trust::Trust;
