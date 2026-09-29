//! Station and link parameters.

use crate::{MIN_SLOW_SYMBOL, SLOW_FRAME_SECS, SLOW_MAX_OVER, SYMBOL_ALIGNMENT};
use hm_core::{Millis, Port};
use hm_wire::{Callsign, DATA_PREAMBLE_LEN, HEADER_LEN, MAX_OBJECT_LEN};

/// Station and link parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub me: Callsign,
    /// Radio port this engine transmits and listens on.
    pub port: Port,
    /// Bytes per RaptorQ symbol, a multiple of 8.
    pub symbol_size: u16,
    /// Link bitrate and key-up delay, used to predict when overs end.
    pub bitrate_bps: u32,
    pub txdelay: Millis,
    /// Bytes the link adds to every frame on air: 19 for an AX.25 UI frame
    /// (16-byte header, 2-byte frame check, closing flag).
    pub frame_overhead_bytes: u16,
    /// Allowance for HDLC bit stuffing, per mille of the frame's bits (random
    /// data stuffs about 16; 0 on links without stuffing).
    pub stuffing_permille: u16,
    /// Most DATA frames in one over.
    pub max_burst: u8,
    /// Longest over, key-up to key-down, OPEN and OFFER included (an over
    /// always carries at least one symbol). Long overs spread the key-up and
    /// the ACK's round trip over more symbols, and with fountain coding a fade
    /// costs only the frames it overlaps however long the over is; the limit
    /// keeps one station from holding a shared channel too long.
    pub max_over: Millis,
    /// Overs in a row that bring no progress (no ACK, or an ACK that asks for
    /// no fewer symbols than the one before) before a transfer fails. Overs
    /// that do bring progress never count against it, so a large object, or
    /// a slow link that needs many overs, is not abandoned while it gets
    /// through.
    pub max_rounds: u8,
    /// Slack added to every predicted end of an over.
    pub ack_guard: Millis,
    pub max_object_len: u32,
    /// Concurrent incoming transfers.
    pub max_incoming: usize,
    /// Concurrent incoming transfers from one sender.
    pub max_incoming_per_sender: usize,
    /// Drop an incoming transfer not heard from for this long.
    pub idle_timeout: Millis,
    /// Remember completed transfers this long, for re-ACKs and duplicate suppression.
    pub done_ttl: Millis,
    /// Long-run share of time this station may transmit, per mille (1000 = no limit).
    pub duty_cycle_permille: u32,
    /// Airtime that may be used at once before the duty cycle applies.
    pub bucket: Millis,
    /// Send OPEN with the first over to a peer (and answer the peer's).
    pub sessions: bool,
    /// Feature bits sent in OPEN (`hm_wire::FEATURE_*`).
    pub features: u32,
    /// A busy receiver asks senders to come back after this many seconds.
    pub busy_retry_secs: u16,
    /// Repair overs a broadcast may send after its first, when listeners ask
    /// for more (0: publish once and forget, as before repair existed).
    pub broadcast_repairs: u8,
}

impl Config {
    /// AFSK 1200 on an FM transceiver: 200-byte symbols, overs of at most 16
    /// frames (about 24 s), and at most 50% duty cycle over time.
    pub fn vhf_1200(me: Callsign) -> Config {
        Config {
            me,
            port: 0,
            symbol_size: 200,
            bitrate_bps: 1200,
            txdelay: Millis(300),
            frame_overhead_bytes: 19,
            stuffing_permille: 20,
            max_burst: 16,
            max_over: Millis::from_secs(30),
            max_rounds: 12,
            ack_guard: Millis(1500),
            max_object_len: 256 * 1024,
            max_incoming: 8,
            max_incoming_per_sender: 2,
            idle_timeout: Millis::from_secs(300),
            done_ttl: Millis::from_secs(1800),
            duty_cycle_permille: 500,
            bucket: Millis::from_secs(120),
            sessions: true,
            features: 0,
            busy_retry_secs: 60,
            broadcast_repairs: 3,
        }
    }

    /// A link at `bitrate_bps` with key-up delay `txdelay`, starting from the
    /// VHF 1200 parameters. Below 1200 bit/s whatever is measured in airtime
    /// scales with the rate: symbols are sized so a DATA frame takes about
    /// [`SLOW_FRAME_SECS`] on air, overs may last up to [`SLOW_MAX_OVER`], more
    /// loss is assumed before any is measured, and a receiver keeps a partial
    /// transfer longer, because a sender backing off after missed ACKs goes
    /// quiet for longer on a slow link.
    ///
    /// On a simulated 300 bd path with Watterson-style fading (0.5 to 1 Hz
    /// Doppler spread, `hm-sim`'s `Loss::Fading`), 64-byte symbols delivered
    /// more, sooner and with less airtime than 128 or 200 bytes: a shorter
    /// frame is less likely to meet a fade. Overs of up to 60 s did better
    /// than 20 s: fewer key-ups and ACK round trips for the same symbols.
    pub fn for_link(me: Callsign, bitrate_bps: u32, txdelay: Millis) -> Config {
        let mut cfg = Config::vhf_1200(me);
        cfg.bitrate_bps = bitrate_bps.max(1);
        cfg.txdelay = txdelay;
        if cfg.bitrate_bps < 1200 {
            let on_air = (u64::from(cfg.bitrate_bps) * SLOW_FRAME_SECS / 8) as usize;
            let symbol = on_air
                .saturating_sub(HEADER_LEN + DATA_PREAMBLE_LEN + cfg.frame_overhead_bytes as usize)
                .clamp(MIN_SLOW_SYMBOL, cfg.symbol_size as usize);
            cfg.symbol_size = (symbol - symbol % SYMBOL_ALIGNMENT as usize) as u16;
            cfg.max_over = SLOW_MAX_OVER;
            cfg.idle_timeout = Millis::from_secs(900);
        }
        cfg
    }

    /// HF packet at 300 bd through a KISS TNC (Direwolf's `MODEM 300` or a
    /// hardware HF TNC): 64-byte symbols, about 2.9 s per DATA frame, and
    /// overs of up to 60 s.
    pub fn hf_300(me: Callsign) -> Config {
        Config::for_link(me, 300, Millis(300))
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.symbol_size == 0 || !self.symbol_size.is_multiple_of(SYMBOL_ALIGNMENT) {
            return Err("symbol size must be a positive multiple of 8");
        }
        if self.bitrate_bps == 0 || self.max_burst == 0 || self.max_rounds == 0 {
            return Err("bitrate, max_burst and max_rounds must be positive");
        }
        if self.stuffing_permille > 1000 {
            return Err("stuffing allowance must be at most 1000 per mille");
        }
        if self.duty_cycle_permille == 0 || self.duty_cycle_permille > 1000 {
            return Err("duty cycle must be 1..=1000 per mille");
        }
        if self.max_incoming == 0 || self.max_incoming_per_sender == 0 {
            return Err("incoming limits must be positive");
        }
        if self.max_object_len == 0 || self.max_object_len > MAX_OBJECT_LEN {
            return Err("max_object_len out of range");
        }
        Ok(())
    }

    /// Airtime of `frames` frames of `len` bytes with the transmitter already
    /// keyed, link overhead and bit stuffing included.
    pub(crate) fn air(&self, frames: usize, len: usize) -> Millis {
        let bits = frames as u64 * (len as u64 + self.frame_overhead_bytes as u64) * 8;
        let bits = bits + (bits * self.stuffing_permille as u64).div_ceil(1000);
        Millis((bits * 1000).div_ceil(self.bitrate_bps as u64))
    }

    pub(crate) fn data_frame_len(&self, symbol_size: usize) -> usize {
        HEADER_LEN + DATA_PREAMBLE_LEN + symbol_size
    }
}
