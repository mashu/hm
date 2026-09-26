//! Wire primitives shared by every layer.
//!
//! - [`Callsign`]: base-40 packing of up to 9 characters into 48 bits.
//! - [`FrameHeader`]: the 18-byte header carried by every radio frame.
//! - [`Ack`]: the fixed-layout acknowledgement payload.
//! - [`DataPreamble`] and [`Offer`]: DATA and CTRL payloads used by transfers.
//! - [`Beacon`]: signed presence and identification, broadcast.
//! - [`Locator`]: Maidenhead grid locators, as beacons carry them.
//! - [`ObjectId`]: 32-byte content hash naming bundles, records and attachments.
//!
//! Layouts are specified in `SPEC.md` at the repository root.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod ack;
mod beacon;
mod callsign;
mod data;
mod frame;
mod id;
mod locator;

pub use ack::{Ack, MAX_ACK_COMPLETED, NEED_OFFER, RECEIPT_LEN};
pub use beacon::{Beacon, Heard, BEACON_SIG_PREFIX, FLAG_INTERNET, FLAG_MAILBOX, FLAG_RELAY, MAX_HEARD};
pub use callsign::{Callsign, CALLSIGN_MAX_LEN};
pub use data::{DataPreamble, Offer, CTRL_OFFER, DATA_PREAMBLE_LEN, MAX_OBJECT_LEN, OFFER_LEN};
pub use frame::{Dest, FrameHeader, FrameType, HEADER_LEN, MAX_INDEX, WIRE_VERSION};
pub use id::ObjectId;
pub use locator::Locator;

/// Errors from encoding or decoding wire structures.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Input ended before the structure was complete.
    TooShort,
    /// Bytes remained after a fixed-size structure.
    Trailing,
    /// Frame header carries a version this implementation does not speak.
    BadVersion(u8),
    /// Frame type nibble is not defined.
    UnknownFrameType(u8),
    /// Text or packed value is not a valid base-40 callsign.
    BadCallsign,
    /// A field value is outside its allowed range.
    OutOfRange,
    /// Not a 4- or 6-character Maidenhead locator.
    BadLocator,
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WireError::TooShort => f.write_str("input too short"),
            WireError::Trailing => f.write_str("trailing bytes"),
            WireError::BadVersion(v) => write!(f, "unsupported wire version {v}"),
            WireError::UnknownFrameType(t) => write!(f, "unknown frame type {t}"),
            WireError::BadCallsign => f.write_str("invalid callsign"),
            WireError::OutOfRange => f.write_str("value out of range"),
            WireError::BadLocator => {
                f.write_str("invalid grid locator (4 or 6 characters, like JO89 or JO89ab)")
            }
        }
    }
}

impl core::error::Error for WireError {}
