//! The latest routing decision about each message, and the route it was made
//! on: why a message went, waits, or is held. Kept for people to see, not
//! decided on; bounded, the oldest decision going first.

use std::collections::BTreeMap;

use hm_route::{Route, RouteError};
use hm_wire::{Callsign, ObjectId};

/// Messages whose latest decision is kept.
pub(super) const DECISIONS_KEPT: usize = 128;

/// What was chosen for a message.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Choice {
    /// Handed to the route's first hop now.
    Send(Route),
    /// Held until the route's first departure.
    Wait(Route),
    /// Held until the route's first hop is heard.
    Hear(Route),
    /// Held: nothing leads there, or nothing worth its cost now.
    Hold(RouteError),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Decision {
    pub at: u64,
    /// Where the message is routed to: its destination, or the station that
    /// asked for it.
    pub to: Callsign,
    pub choice: Choice,
}

#[derive(Default)]
pub(super) struct Decisions {
    latest: BTreeMap<ObjectId, Decision>,
}

impl Decisions {
    pub(super) fn record(&mut self, id: ObjectId, decision: Decision) {
        self.latest.insert(id, decision);
        if self.latest.len() > DECISIONS_KEPT {
            let oldest = self
                .latest
                .iter()
                .min_by_key(|(_, decision)| decision.at)
                .map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                self.latest.remove(&oldest);
            }
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&ObjectId, &Decision)> {
        self.latest.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oldest_decision_goes_first() {
        let to = Callsign::parse("SP5AAA").unwrap();
        let mut decisions = Decisions::default();
        for n in 0..=DECISIONS_KEPT as u64 {
            let mut id = [0; 32];
            id[..8].copy_from_slice(&n.to_be_bytes());
            decisions.record(
                ObjectId(id),
                Decision {
                    at: 1_000 - n,
                    to,
                    choice: Choice::Hold(RouteError::NoRoute),
                },
            );
        }
        assert_eq!(decisions.iter().count(), DECISIONS_KEPT);
        assert!(decisions
            .iter()
            .all(|(_, d)| d.at > 1_000 - DECISIONS_KEPT as u64));
    }
}
