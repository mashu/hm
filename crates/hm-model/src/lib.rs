//! Generative models of what a station cannot see directly, and the decisions
//! they drive.
//!
//! Three models cover what the stack estimates:
//!
//! | model | about | answers |
//! |---|---|---|
//! | [`LinkModel`] | one directed link | is it open now, at `t`? how lossy? does a handoff complete? |
//! | [`CustodianModel`] | a station holding custody | does it accept? deliver? how long does it take? |
//! | [`ChannelModel`] | a shared radio channel | how many stations contend? how busy is it? |
//!
//! Each has a generative story (see its module) and takes observations as
//! likelihoods: a beacon heard, one that did not come, an over's ACK count, a
//! handoff, a refusal, an end-to-end receipt. Each kind of news moves only the
//! parts of the model it bears on, so a refusal never makes a link look bad
//! and a local radio fault is not news about any link.
//!
//! Decisions are functions of the posterior and a cost, not thresholds:
//!
//! * route choice: [`Estimate`] by posterior mean or by Thompson sampling
//!   (see `hm-route`);
//! * over size: [`burst_size`] minimises expected airtime under the
//!   Beta-binomial predictive of frame arrivals;
//! * when to reclaim custody: [`CustodianModel::suspect_after`];
//! * CSMA persistence and control-plane sharing: [`ChannelModel`].
//!
//! Priors are the population's ([`Beliefs`]): a new link is expected to behave
//! like the links of its bearer that this station already knows.
//!
//! `no_std`: the transfer engine sizes bursts with it on the radio thread.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use hm_wire::{Callsign, ContactBearer};
use minicbor::{Decode, Encode};

mod beliefs;
mod channel;
mod custodian;
pub mod diurnal;
mod erasure;
mod evidence;
mod link;
pub mod math;

pub use beliefs::{
    Beliefs, Estimate, LinkEstimate, Mean, RestoreError, Thompson, FORGET_AFTER, MISS_HORIZON,
};
pub use channel::{clear_slot, ChannelModel, ChannelObservation, CHANNEL_HALF_LIFE};
pub use custodian::{CustodianModel, CustodianObservation, CustodianPrior, CUSTODIAN_HALF_LIFE};
pub use erasure::{burst_size, plan_overs, Erasure, OverCost};
pub use evidence::{Beta, Evidence, Prior};
pub use link::{LinkModel, LinkObservation, LinkPrior, SampledLink, ERASURE_HALF_LIFE, HANDOFF_HALF_LIFE};

/// How a link carries bundles.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum Bearer {
    /// The radio thread's own link (KISS TNC or the built-in modem).
    #[n(0)]
    Radio,
    #[n(1)]
    Internet,
    /// An ARQ modem: VARA, Mercury or ARDOP.
    #[n(2)]
    Modem,
}

impl Bearer {
    pub const ALL: [Bearer; 3] = [Bearer::Radio, Bearer::Internet, Bearer::Modem];

    pub fn name(self) -> &'static str {
        match self {
            Bearer::Radio => "radio",
            Bearer::Internet => "internet",
            Bearer::Modem => "modem",
        }
    }

    pub fn from_name(name: &str) -> Option<Bearer> {
        Bearer::ALL.into_iter().find(|b| b.name() == name)
    }

    /// Whether the bearer goes over the air (and follows propagation).
    pub fn on_air(self) -> bool {
        matches!(self, Bearer::Radio | Bearer::Modem)
    }

    pub const fn index(self) -> usize {
        match self {
            Bearer::Radio => 0,
            Bearer::Internet => 1,
            Bearer::Modem => 2,
        }
    }

    pub fn from_index(index: u8) -> Option<Bearer> {
        Bearer::ALL.get(usize::from(index)).copied()
    }
}

impl From<ContactBearer> for Bearer {
    fn from(value: ContactBearer) -> Self {
        match value {
            ContactBearer::Radio => Bearer::Radio,
            ContactBearer::Internet => Bearer::Internet,
            ContactBearer::Modem => Bearer::Modem,
        }
    }
}

impl From<Bearer> for ContactBearer {
    fn from(value: Bearer) -> Self {
        match value {
            Bearer::Radio => ContactBearer::Radio,
            Bearer::Internet => ContactBearer::Internet,
            Bearer::Modem => ContactBearer::Modem,
        }
    }
}

/// A directed link: frames or bundles from `from` to `to` over `bearer`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkKey {
    pub from: Callsign,
    pub to: Callsign,
    pub bearer: Bearer,
}
