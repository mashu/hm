//! What the channel carried: airtime by frame class, per channel and per
//! station, and the report of a run.

use hm_core::Millis;
use hm_wire::{FrameHeader, FrameType, DATA_PREAMBLE_LEN, HEADER_LEN};

/// What a frame's bytes are spent on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrameClass {
    /// Content being transferred (DATA frames).
    Payload,
    /// Protocol machinery (ACK, SYNC, CTRL, BEACON).
    Control,
    /// Not recognised by the classifier; counted whole.
    Unknown,
}

/// The number of header bytes and the class of an hm-wire frame. DATA symbols are payload, every other
/// frame type is control; the 18-byte header and the 4-byte DATA preamble are overhead.
pub fn classify_hm_frame(frame: &[u8]) -> (usize, FrameClass) {
    match FrameHeader::decode(frame) {
        Ok((h, _)) if h.ftype == FrameType::Data => (HEADER_LEN + DATA_PREAMBLE_LEN, FrameClass::Payload),
        Ok(_) => (HEADER_LEN, FrameClass::Control),
        Err(_) => (0, FrameClass::Unknown),
    }
}

/// Airtime split by purpose, in microseconds. Frame bodies are split exactly;
/// the scheduled airtime in [`Stats::airtime_ms`] also includes rounding up to 1 ms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Airtime {
    /// Key-up: TXDELAY and TXTAIL.
    pub txdelay_us: u64,
    /// What the link and modem add to each frame: overhead bytes (AX.25
    /// header, frame check, flag) and bit stuffing.
    pub link_us: u64,
    /// Frame headers (as the classifier counts them).
    pub overhead_us: u64,
    pub payload_us: u64,
    pub control_us: u64,
    pub unknown_us: u64,
}

impl Airtime {
    pub fn total_us(&self) -> u64 {
        self.txdelay_us
            + self.link_us
            + self.overhead_us
            + self.payload_us
            + self.control_us
            + self.unknown_us
    }

    /// Share of airtime that carried payload, in `[0, 1]`.
    pub fn payload_share(&self) -> f64 {
        let t = self.total_us();
        if t == 0 {
            0.0
        } else {
            self.payload_us as f64 / t as f64
        }
    }

    pub(crate) fn add(&mut self, o: &Airtime) {
        self.txdelay_us += o.txdelay_us;
        self.link_us += o.link_us;
        self.overhead_us += o.overhead_us;
        self.payload_us += o.payload_us;
        self.control_us += o.control_us;
        self.unknown_us += o.unknown_us;
    }
}

/// Counters for one channel (or, from [`Report::total`], all channels).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub airtime_ms: u64,
    pub airtime: Airtime,
    /// Receiver-side outcomes, one per (transmission, listening neighbour).
    pub delivered: u64,
    /// Delivered with undetected bit errors.
    pub corrupted: u64,
    pub lost_channel: u64,
    pub lost_collision: u64,
    pub lost_half_duplex: u64,
    pub lost_down: u64,
}

impl Stats {
    pub(crate) fn add(&mut self, o: &Stats) {
        self.frames_sent += o.frames_sent;
        self.bytes_sent += o.bytes_sent;
        self.airtime_ms += o.airtime_ms;
        self.airtime.add(&o.airtime);
        self.delivered += o.delivered;
        self.corrupted += o.corrupted;
        self.lost_channel += o.lost_channel;
        self.lost_collision += o.lost_collision;
        self.lost_half_duplex += o.lost_half_duplex;
        self.lost_down += o.lost_down;
    }
}

/// Per-station counters, all ports together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeStats {
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub airtime_ms: u64,
    pub frames_received: u64,
}

/// Summary of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub now: Millis,
    pub channels: Vec<Stats>,
    pub nodes: Vec<NodeStats>,
    /// FNV-1a hash over every delivery, fault and application event, in order.
    pub trace: u64,
}

impl Report {
    pub fn total(&self) -> Stats {
        let mut t = Stats::default();
        for c in &self.channels {
            t.add(c);
        }
        t
    }
}
