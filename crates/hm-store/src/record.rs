//! What the store knows about one message, and the states it goes through.

use hm_wire::{Callsign, ObjectId};
use minicbor::{Decode, Encode};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum Direction {
    #[n(0)]
    In,
    #[n(1)]
    Out,
    /// A bundle held in custody for another station.
    #[n(2)]
    Relay,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum State {
    /// Inbound, not yet marked read.
    #[n(0)]
    Unread,
    #[n(1)]
    Read,
    /// Outbound, waiting for its next attempt.
    #[n(2)]
    Queued,
    /// Outbound, confirmed by the receiver (see `verified` for the receipt).
    #[n(3)]
    Delivered,
    /// Outbound, abandoned after the retry limit.
    #[n(4)]
    Failed,
    /// A next hop accepted custody; only an end-to-end receipt completes it.
    #[n(5)]
    InTransit,
    /// Removed from the local queue by the operator.
    #[n(6)]
    Cancelled,
    /// Hop custody was transferred but no destination receipt returned; delivery
    /// may have succeeded.
    #[n(7)]
    DeliveredUnconfirmed,
}

/// What the store knows about one message.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Record {
    #[n(0)]
    pub id: ObjectId,
    #[n(1)]
    pub direction: Direction,
    /// Inbound: the station it came from over the air. Outbound: the destination.
    #[n(2)]
    pub peer: Callsign,
    /// Unix seconds it was received or queued.
    #[n(3)]
    pub at: u64,
    #[n(4)]
    pub state: State,
    #[n(5)]
    pub precedence: u8,
    #[n(6)]
    pub attempts: u32,
    /// Outbound: Unix seconds of the next attempt.
    #[n(7)]
    pub next_attempt: u64,
    /// Inbound: sender's signature verified. Outbound: receiver's receipt verified.
    #[n(8)]
    pub verified: bool,
    #[n(9)]
    pub seq: u64,
    /// Last failure reason, for the operator.
    #[n(10)]
    pub note: Option<String>,
    /// Outbound: the bearer that delivered it ("radio", "internet").
    #[n(11)]
    pub by: Option<String>,
    /// Final station recipient. Absent on legacy records; then `peer` is final.
    #[n(12)]
    pub final_peer: Option<Callsign>,
    /// Current route's immediate next hop.
    #[n(13)]
    pub next_hop: Option<Callsign>,
    /// Mutable hop count from the routing wrapper.
    #[n(14)]
    pub hop_count: Option<u8>,
    /// Mutable visited path from the routing wrapper.
    #[n(15)]
    pub visited: Option<Vec<Callsign>>,
    /// Station that most recently accepted custody from this node.
    #[n(16)]
    pub custody_by: Option<Callsign>,
    /// Outbound: the destination's verified receipt, once it came.
    /// Inbound: the receipt this station answered with last.
    #[n(17)]
    pub e2e_receipt: Option<ObjectId>,
    /// Bundle expiry in Unix seconds, for relay admission and cleanup.
    #[n(18)]
    pub expires_at: Option<u64>,
    /// Previous custodian, for relayed records.
    #[n(19)]
    pub custody_from: Option<Callsign>,
    /// Signed bundle hop limit, absent on legacy records.
    #[n(20)]
    pub max_hops: Option<u8>,
    /// At most two custodians for Immediate/Flash traffic.
    #[n(21)]
    pub custody_copies: Option<Vec<Callsign>>,
    /// Handoffs currently queued or in flight.
    #[n(22)]
    pub next_hops: Option<Vec<Callsign>>,
    /// Retain bytes / advertise holdings until this Unix second after handoff.
    #[n(23)]
    pub shadow_until: Option<u64>,
    /// Sender-assigned conversation sequence from the wire bundle (chat).
    #[n(24)]
    pub wire_seq: Option<u64>,
    /// When `custody_by` took custody, Unix seconds.
    #[n(25)]
    pub custody_at: Option<u64>,
    /// Queued, waiting to leave through this station when its link is
    /// forecast to open: tried sooner if the station is heard first.
    #[n(26)]
    pub waiting_for: Option<Callsign>,
    /// When the route `custody_by` took it on planned it to arrive.
    #[n(27)]
    pub custody_eta: Option<u64>,
    /// The first custody reclaimed from, kept so a receipt that comes late
    /// after all is still credited to it.
    #[n(28)]
    pub first_custody: Option<Handed>,
}

/// Custody handed to `custodian` at `at`, on a route planned to arrive at
/// `eta`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Handed {
    #[n(0)]
    pub custodian: Callsign,
    #[n(1)]
    pub at: u64,
    #[n(2)]
    pub eta: u64,
}

impl Record {
    pub fn final_destination(&self) -> Callsign {
        self.final_peer.unwrap_or(self.peer)
    }

    /// The custody handed over now, if any.
    pub fn handed(&self) -> Option<Handed> {
        let (custodian, at) = self.custody_by.zip(self.custody_at)?;
        Some(Handed {
            custodian,
            at,
            eta: self.custody_eta.unwrap_or(at),
        })
    }

    /// A relay holding handed on to the next custodian, whose part it now is.
    pub fn handed_on(&self) -> bool {
        self.direction == Direction::Relay
            && self.state == State::DeliveredUnconfirmed
            && self.custody_by.is_some()
    }

    pub fn in_shadow(&self, now: u64) -> bool {
        self.state == State::InTransit && self.shadow_until.is_some_and(|until| until > now)
    }
}
