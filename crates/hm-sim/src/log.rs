//! The detailed log of a run, for independent checking.

use hm_core::{Millis, Port};

use crate::{ChannelId, NodeId};

/// What happened to one transmission at one receiver.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    Delivered,
    Corrupted,
    LostChannel,
    LostCollision,
    LostHalfDuplex,
    LostDown,
}

/// Optional detailed log, in processing order, for independent checking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogEntry {
    /// `asked_at` is when the machine asked to transmit and `queued_at` when
    /// the radio took the frame: the same, except on a radio with [`Csma`](crate::Csma),
    /// where it is when the channel was found clear. `keyup` is false when the
    /// frame followed the previous one without a new TXDELAY.
    Tx {
        id: u64,
        channel: ChannelId,
        from: NodeId,
        port: Port,
        asked_at: Millis,
        queued_at: Millis,
        start: Millis,
        end: Millis,
        keyup: bool,
        len: usize,
        digest: u64,
        /// The frame's bytes, so a checker can count bit stuffing itself.
        data: Vec<u8>,
    },
    /// Transmission `tx` ended and is being evaluated at every listening neighbour;
    /// its `Rx` entries follow immediately.
    Eval {
        tx: u64,
        at: Millis,
    },
    Rx {
        tx: u64,
        channel: ChannelId,
        to: NodeId,
        port: Port,
        at: Millis,
        outcome: Outcome,
        digest: u64,
    },
    Up {
        node: NodeId,
        at: Millis,
        up: bool,
    },
    Link {
        channel: ChannelId,
        from: NodeId,
        to: NodeId,
        at: Millis,
        enabled: bool,
    },
}
