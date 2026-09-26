//! Bearers: how frames reach a radio.
//!
//! - [`kiss`]: KISS framing, the host–TNC protocol spoken by Direwolf,
//!   hardware TNCs (NinoTNC, Mobilinkd) and radios with a built-in TNC.
//! - [`ax25`]: AX.25 UI encapsulation. KISS TNCs expect AX.25 frames, so on
//!   that path every hm frame travels as the information field of a UI frame
//!   addressed to `HMNET` with PID 0xF0. The built-in modems (later in Phase 1)
//!   carry hm frames directly and skip this 16-byte wrapper.
//!
//! Both are sans-IO codecs; TCP and serial adapters build on them.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod ax25;
pub mod kiss;
