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
    /// The station's belief about frame loss towards `peer`: overs to it are
    /// sized from this until the next one.
    Belief { peer: Callsign, erasure: Erasure },
    /// Application durably stored (or refused) a just-received object.
    Accept {
        from: Callsign,
        id: ObjectId,
        accepted: bool,
        /// Zero means refuse permanently; otherwise ask the sender to retry.
        retry_after: u16,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Failure {
    /// `max_rounds` overs in a row brought no progress.
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
