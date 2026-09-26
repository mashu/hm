//! Radio hardware.
//!
//! - [`AudioPort`]: capture and play audio. [`soundcard`] implements it on real
//!   devices (cpal: ALSA, CoreAudio, WASAPI); [`ether`] implements it as a shared
//!   virtual radio channel for tests, where each station hears the others'
//!   transmissions, plus noise, in real time and hears nothing while it transmits.
//! - [`ptt`]: keying the transmitter: Hamlib's rigctld (CAT), serial RTS/DTR,
//!   CM108 GPIO (AIOC, Digirig and similar cables), or VOX.

pub mod ether;
pub mod ptt;
#[cfg(feature = "soundcard")]
pub mod soundcard;

use std::io;
use std::time::Duration;

/// A duplex audio device.
pub trait AudioPort {
    fn sample_rate(&self) -> u32;
    /// Append samples captured since the last call; waits up to `wait` for some.
    fn capture(&mut self, out: &mut Vec<f32>, wait: Duration) -> io::Result<()>;
    /// Play `samples` and return once the last one has left the device.
    fn play(&mut self, samples: &[f32]) -> io::Result<()>;
}
