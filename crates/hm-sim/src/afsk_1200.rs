//! Frame loss of the built-in AFSK 1200 modem (`hm-modem-afsk`) in white noise.
//!
//! Generated; do not edit by hand. To measure again and rewrite this file:
//! `HM_WRITE_CURVE=1 cargo test -p hm-sim --release --test afsk afsk_1200_curve -- --ignored`
//! (without `HM_WRITE_CURVE` the same test checks this table against the modem).
//!
//! Each point: 300 AX.25 frames of random bytes, each in its own key-up with
//! 300 ms of TXDELAY flags and 200 ms of silence after, sampled at 48000 Hz, in
//! white Gaussian noise at the given SNR (noise power in a 3 kHz bandwidth),
//! heard continuously by one demodulator. Flat audio: no emphasis tilt, no
//! fading, no FM threshold or squelch effects.

use crate::curve::LossCurve;

pub const CURVE: LossCurve = LossCurve {
    name: "hm-modem-afsk 1200 bd, white noise, 48000 Hz",
    snr_db: &[
        4.0, 4.5, 5.0, 5.5, 6.0, 6.5, 7.0, 7.5, 8.0, 8.5, 9.0, 9.5, 10.0, 10.5, 11.0,
    ],
    len: &[40, 80, 160, 240, 360],
    loss: &[
        &[0.9367, 0.9967, 1.0000, 1.0000, 1.0000], // 4.0 dB
        &[0.7667, 0.9400, 0.9967, 1.0000, 1.0000], // 4.5 dB
        &[0.6600, 0.8400, 0.9867, 0.9967, 1.0000], // 5.0 dB
        &[0.3833, 0.6267, 0.8967, 0.9333, 1.0000], // 5.5 dB
        &[0.2267, 0.3900, 0.6100, 0.8033, 0.9033], // 6.0 dB
        &[0.0900, 0.2300, 0.4333, 0.5500, 0.7000], // 6.5 dB
        &[0.0333, 0.0833, 0.1867, 0.2433, 0.3967], // 7.0 dB
        &[0.0133, 0.0367, 0.0833, 0.1100, 0.2067], // 7.5 dB
        &[0.0033, 0.0167, 0.0133, 0.0367, 0.0467], // 8.0 dB
        &[0.0033, 0.0000, 0.0033, 0.0100, 0.0267], // 8.5 dB
        &[0.0000, 0.0000, 0.0033, 0.0033, 0.0033], // 9.0 dB
        &[0.0000, 0.0000, 0.0000, 0.0000, 0.0000], // 9.5 dB
        &[0.0000, 0.0000, 0.0000, 0.0000, 0.0000], // 10.0 dB
        &[0.0000, 0.0000, 0.0000, 0.0000, 0.0000], // 10.5 dB
        &[0.0000, 0.0000, 0.0000, 0.0000, 0.0000], // 11.0 dB
    ],
    frames_per_point: 300,
};
