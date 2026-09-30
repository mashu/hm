//! What the station asks of the engine, and what the engine tells it.

use alloc::vec::Vec;
use hm_model::Erasure;
use hm_wire::{Callsign, ObjectId};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Transfer `object` to `to`. Precedence as in bundles, 0 routine to 3 flash;
    /// higher precedence is sent first.
    Send {
        to: Callsign,
        object: Vec<u8>,
        precedence: u8,
    },
    /// Publish `object` once on RF (`Dest::Broadcast`). No ACK wait; listeners
    /// that reconstruct it emit [`Event::Received`] and do not ACK. Completes
    /// locally as [`Event::Delivered`] to [`broadcast_peer`] with
    /// [`Receipt::Unverified`].
    Broadcast { object: Vec<u8>, precedence: u8 },
    /// What the station believes about the link towards `peer`: overs to it
    /// are sized, and silent overs weighed, by this until the next one.
    Belief { peer: Callsign, belief: PeerBelief },
    /// Application durably stored (or refused) a just-received object.
    Accept {
        from: Callsign,
        id: ObjectId,
        accepted: bool,
        /// Zero means refuse permanently; otherwise ask the sender to retry.
        retry_after: u16,
    },
}

/// What the station believes about the link towards a peer.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PeerBelief {
    /// Frame loss while the link is open: overs are sized from it.
    pub erasure: Erasure,
    /// Chance the link is open now.
    pub open: f64,
    /// What a second of airtime costs, in units of what completing the
    /// transfer is worth. An over that brings no answer is evidence that the
    /// link has closed; overs stop once the next one's chance of being
    /// answered is worth less than its airtime.
    pub airtime_cost: f64,
}

impl PeerBelief {
    /// Frame loss `erasure` on a link taken to be open, whose airtime costs
    /// nothing: overs go on until `max_rounds` bring no progress.
    pub fn open(erasure: Erasure) -> PeerBelief {
        PeerBelief {
            erasure,
            open: 1.0,
            airtime_cost: 0.0,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Failure {
    /// Overs brought no progress: `max_rounds` of them in a row, or as many
    /// as silence made worth their airtime.
    NoAnswer,
    TooLarge,
    Empty,
    /// Sending to ourselves.
    SelfAddressed,
    /// The receiver said it will not take objects from us.
    Refused,
}

/// How far a delivery confirmation could be checked.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Receipt {
    /// Signed by the receiver's known key.
    Verified,
    /// We have no key for the receiver, so the confirmation could have been forged.
    Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    /// An ACK said `got` of the `sent` frames of our last over to `to`
    /// arrived: an observation of the link's frame loss.
    Over { to: Callsign, sent: u32, got: u32 },
    /// A complete object arrived and matched its hash. Emitted once per object
    /// per `done_ttl`, however many times it is sent.
    Received {
        from: Callsign,
        id: ObjectId,
        object: Vec<u8>,
    },
    /// The receiver confirmed the whole object.
    Delivered {
        to: Callsign,
        id: ObjectId,
        rounds: u8,
        receipt: Receipt,
    },
    Failed {
        to: Callsign,
        id: ObjectId,
        reason: Failure,
    },
}
