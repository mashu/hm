//! What passes between the node and its bearers: commands to the radio and
//! what the radio reports, and how an internet or modem transfer ended.

use hm_model::Erasure;
use hm_wire::{Callsign, Dest, ObjectId};
use hm_xfer::{Failure, Receipt};

use crate::heard;

#[derive(Debug)]
pub enum RadioCmd {
    /// Transfer `object` to `to`, with overs sized from the station's belief
    /// about frame loss on the link (`erasure`).
    Send {
        object: Vec<u8>,
        to: Callsign,
        precedence: u8,
        erasure: Erasure,
    },
    /// RF bulletin: `Dest::Broadcast`, no ACK wait; `erasure`: the station's
    /// belief about frame loss on its radio links in general.
    Broadcast {
        object: Vec<u8>,
        precedence: u8,
        erasure: Erasure,
    },
    Accept {
        from: Callsign,
        xfer_id: ObjectId,
        accepted: bool,
        retry_after: u16,
    },
    Sync {
        to: Dest,
        payload: Vec<u8>,
    },
    /// Whether we hold bundles others may pull: our beacons say so.
    Holding(bool),
}

#[derive(Debug)]
pub enum RadioEvt {
    /// The radio link now in use (`None`: the radio is off).
    Using(Option<String>),
    Up,
    Down(String),
    Received {
        from: Callsign,
        xfer_id: ObjectId,
        object: Vec<u8>,
    },
    Delivered {
        xfer_id: ObjectId,
        to: Callsign,
        receipt: Receipt,
    },
    Failed {
        xfer_id: ObjectId,
        to: Callsign,
        reason: Failure,
    },
    /// An ACK from `to` said `got` of our last `sent` frames arrived.
    Over {
        to: Callsign,
        sent: u32,
        got: u32,
    },
    Sync {
        from: Callsign,
        payload: Vec<u8>,
    },
    Heard(Vec<heard::Station>),
    /// We now beacon every this many seconds (more stations, longer).
    BeaconInterval(u64),
}

/// How a transfer over the internet or through an ARQ modem ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transfer {
    /// The receiver took custody, with a verified receipt.
    Delivered,
    /// The receiver is busy for `retry_after` seconds.
    Busy { retry_after: u64, reason: String },
    /// The receiver said no; a permanent refusal is not retried.
    Refused { reason: String, permanent: bool },
    /// The transfer failed on the way (no link, lost session, no answer).
    Failed(String),
}
