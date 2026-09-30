//! What a radio channel is like: its loss model, bitrate and timing, channel
//! access, and the stations' clocks.

use hm_core::Millis;

use crate::LossCurve;

/// Loss model of one directed link.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Loss {
    None,
    /// Each frame lost independently with probability `p`.
    Bernoulli(f64),
    /// Two-state Markov chain stepped once per frame, then a loss draw in the
    /// current state. Long-run loss = `πg·loss_good + πb·loss_bad` with
    /// `πb = p_good_to_bad / (p_good_to_bad + p_bad_to_good)`.
    GilbertElliott {
        p_good_to_bad: f64,
        p_bad_to_good: f64,
        loss_good: f64,
        loss_bad: f64,
    },
    /// Frame loss probability by UTC hour (index 0 = 00:00–00:59).
    /// 1.0 models a closed band.
    Hourly([f64; 24]),
    /// A real modem at a fixed SNR, from its measured curve: longer frames
    /// (bytes on air, the channel's per-frame overhead included) are lost more often.
    Measured {
        curve: &'static LossCurve,
        snr_db: f64,
    },
    /// The same modem on a fading path, as on HF. The signal's complex gain
    /// fades with a Gaussian Doppler spectrum, as in Watterson's HF channel
    /// model (CCIR 520, ITU-R F.1487): `doppler_spread_hz` is twice its
    /// standard deviation, 0.1 Hz on a quiet ionospheric path ("good"), 0.5 Hz
    /// ("moderate"), 1 Hz ("poor"), 10 Hz with flutter. A steady part is set
    /// by the Rician factor `rician_k` (0: pure Rayleigh fading). A frame is
    /// judged at the weakest SNR it meets on air, through the measured curve,
    /// so a fade anywhere in a long frame loses it. Both directions of a path
    /// share the fading: over seconds the channel is reciprocal, so an ACK
    /// tends to fail when the over did.
    ///
    /// The gain is a sum of 16 sinusoids with Doppler shifts drawn from the
    /// spectrum. Not modelled: multipath delay spread, which smears symbols
    /// and costs a real HF modem more than the white-noise curve says, and
    /// noise that differs between the two ends.
    Fading {
        curve: &'static LossCurve,
        /// Mean SNR in dB, noise in a 3 kHz bandwidth.
        mean_snr_db: f64,
        doppler_spread_hz: f64,
        rician_k: f64,
    },
}

impl Loss {
    /// The built-in AFSK 1200 modem at `snr_db` (3 kHz noise bandwidth), as
    /// measured in white noise at 48 kHz: see [`crate::afsk_1200::CURVE`].
    pub const fn afsk_1200(snr_db: f64) -> Loss {
        Loss::Measured {
            curve: &crate::afsk_1200::CURVE,
            snr_db,
        }
    }
}

/// Physical parameters of a channel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RadioParams {
    pub bitrate_bps: u32,
    /// Key-up delay before data (TXDELAY), counted as airtime.
    pub txdelay: Millis,
    /// Flags or carrier after the last frame of a key-up (TXTAIL), counted as
    /// airtime. Charged to the frame that keys up, like TXDELAY.
    pub txtail: Millis,
    /// Extra bytes the link and modem add per frame: link header, frame check,
    /// closing flag, or preamble, sync word and FEC parity.
    pub phy_overhead_bytes: u32,
    /// HDLC bit stuffing: a 0 bit after every five 1 bits of the frame. Counted
    /// exactly on the frame's own bytes; the overhead bytes are not stuffed.
    pub hdlc: bool,
}

impl RadioParams {
    /// AFSK 1200 on an FM transceiver through AX.25, as the built-in modem and
    /// Direwolf send it: 300 ms TXDELAY, a 16-byte UI header, a 2-byte frame
    /// check and a flag after each frame, HDLC bit stuffing, and two tail flags.
    /// Checked against the modulator in `tests/afsk.rs`.
    pub const VHF_1200: RadioParams = RadioParams {
        bitrate_bps: 1200,
        txdelay: Millis(300),
        txtail: Millis(14),
        phy_overhead_bytes: 19,
        hdlc: true,
    };

    /// HF packet at 300 bd through an SSB transceiver (Direwolf's `MODEM 300`
    /// or a hardware HF TNC): AX.25 in HDLC as at 1200 bd, 300 ms TXDELAY and
    /// two tail flags.
    pub const HF_300: RadioParams = RadioParams {
        bitrate_bps: 300,
        txdelay: Millis(300),
        txtail: Millis(54),
        phy_overhead_bytes: 19,
        hdlc: true,
    };

    /// A channel that sends exactly the frame's bytes, with only a TXDELAY.
    pub const fn raw(bitrate_bps: u32, txdelay: Millis) -> RadioParams {
        RadioParams {
            bitrate_bps,
            txdelay,
            txtail: Millis(0),
            phy_overhead_bytes: 0,
            hdlc: false,
        }
    }

    /// Airtime of a frame of `frame_len` bytes that keys up the transmitter,
    /// without bit stuffing (a lower bound on HDLC channels).
    pub fn airtime(&self, frame_len: usize) -> Millis {
        self.txdelay + self.txtail + self.airtime_keyed(frame_len)
    }

    /// Airtime of a frame sent while the transmitter is already keyed, without bit stuffing.
    pub fn airtime_keyed(&self, frame_len: usize) -> Millis {
        self.bits_ms((frame_len as u64 + self.phy_overhead_bytes as u64) * 8)
    }

    /// Bits `frame` occupies on air, overhead and stuffing included.
    pub fn frame_bits(&self, frame: &[u8]) -> u64 {
        let stuffed = if self.hdlc { stuffed_bits(frame) } else { 0 };
        (frame.len() as u64 + self.phy_overhead_bytes as u64) * 8 + stuffed
    }

    /// Airtime of `frame`, with the key-up (TXDELAY and TXTAIL) when `keyup`.
    pub fn airtime_of(&self, frame: &[u8], keyup: bool) -> Millis {
        let body = self.bits_ms(self.frame_bits(frame));
        if keyup {
            self.txdelay + self.txtail + body
        } else {
            body
        }
    }

    fn bits_ms(&self, bits: u64) -> Millis {
        Millis((bits * 1000).div_ceil(self.bitrate_bps.max(1) as u64))
    }

    pub(crate) fn bits_us(&self, bits: u64) -> u64 {
        bits * 1_000_000 / self.bitrate_bps.max(1) as u64
    }
}

/// Zero bits HDLC inserts into `bytes` (sent least significant bit first):
/// one after every run of five 1 bits, the run count starting at zero.
pub fn stuffed_bits(bytes: &[u8]) -> u64 {
    let (mut ones, mut stuffed) = (0u32, 0u64);
    for &b in bytes {
        for i in 0..8 {
            if (b >> i) & 1 == 1 {
                ones += 1;
                if ones == 5 {
                    stuffed += 1;
                    ones = 0;
                }
            } else {
                ones = 0;
            }
        }
    }
    stuffed
}

/// Carrier-sense channel access for one radio: p-persistent CSMA on carrier
/// detect, as the built-in modem's link and Direwolf do it. Frames the machine
/// asks to send while the radio is idle wait for a clear channel: while a
/// carrier is heard, wait a slot; when clear, key up with probability
/// (persist + 1) / 256, else wait a slot. They then go out in one key-up.
/// Frames asked for while the radio is keyed follow in the same key-up.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Csma {
    pub persist: u8,
    pub slot: Millis,
    /// How long after another station keys up its carrier is detected. A
    /// station that starts within this time of another does not hear it.
    pub dcd_delay: Millis,
}

impl Csma {
    /// The built-in modem's defaults (`--persist 63 --slottime 100`). Carrier
    /// detect: its demodulator rises 68–101 ms after key-up at 7–20 dB SNR
    /// (`tests/afsk.rs`), and the link reads audio in 20 ms chunks.
    pub const DEFAULT: Csma = Csma {
        persist: 63,
        slot: Millis(100),
        dcd_delay: Millis(125),
    };
}

/// A station's clock relative to simulation time:
/// `local = offset + global + floor(global * ppm / 1e6)`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub offset: Millis,
    /// Drift in parts per million; |ppm| must be below 100,000.
    pub ppm: i32,
}

impl Clock {
    pub fn local(&self, global: Millis) -> Millis {
        let g = global.0 as i128;
        let drift = (g * self.ppm as i128).div_euclid(1_000_000);
        Millis((self.offset.0 as i128 + g + drift) as u64)
    }

    /// Earliest global time at which this clock reads `local` or later.
    pub fn global_for(&self, local: Millis) -> Millis {
        if local <= self.local(Millis::ZERO) {
            return Millis::ZERO;
        }
        let target = local.0 as i128 - self.offset.0 as i128;
        let mut g = ((target * 1_000_000).div_euclid(1_000_000 + self.ppm as i128)).max(0);
        while self.local(Millis(g as u64)) < local {
            g += 1;
        }
        while g > 0 && self.local(Millis((g - 1) as u64)) >= local {
            g -= 1;
        }
        Millis(g as u64)
    }
}
