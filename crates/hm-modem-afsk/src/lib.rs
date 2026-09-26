//! Bell 202 AFSK 1200 modem, the standard for VHF/UHF packet radio.
//!
//! - [`Modulator`]: frames to audio. HDLC framing (flags, bit stuffing),
//!   CRC-16/X.25 frame check, NRZI, phase-continuous 1200/2200 Hz tones.
//! - [`Demodulator`]: audio to frames, built from the techniques that make the
//!   best software TNCs decode well:
//!   - a band-pass prefilter;
//!   - quadrature correlators for the mark and space tones;
//!   - separate AGC per tone, so the two tones are compared at equal strength;
//!   - several slicers, each weighting space against mark differently, to
//!     cope with the audio tilt of pre-emphasis and de-emphasis;
//!   - a digital PLL per slicer for bit timing, tightening once locked;
//!   - HDLC deframing with frame-check verification;
//!   - one copy of each frame however many slicers decoded it;
//!   - carrier detect from how regular the bit transitions are.
//!
//! Frames are raw HDLC payloads (AX.25 frames in practice) without the frame
//! check sequence; the modem adds and verifies it.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod crc;
mod demod;
mod filter;
pub mod hdlc;
pub mod il2p;
mod modulator;
pub mod rs;

pub use demod::{Demodulator, DemodulatorConfig};
pub use modulator::Modulator;

pub const MARK_HZ: f32 = 1200.0;
pub const SPACE_HZ: f32 = 2200.0;
pub const BAUD: f32 = 1200.0;
