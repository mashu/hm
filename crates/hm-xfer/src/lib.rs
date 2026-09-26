//! Transfer of whole objects (signed envelopes, attachments) from one station
//! to another over a half-duplex radio link.
//!
//! One over from the sender carries an OFFER and a burst of RaptorQ symbols
//! (RFC 6330). The receiver needs any K of them, where K = ceil(len / symbol
//! size), so lost frames are never retransmitted: the next over simply
//! carries fresh repair symbols. After each over the receiver sends one ACK
//! saying how many symbols it still needs; the sender sizes its next burst from
//! that and from its running estimate of the link's loss rate.
//!
//! Every DATA frame says how many frames remain in the over, so the receiver
//! knows when the sender will stop transmitting and it may answer. The object
//! hash in the OFFER is checked after decoding; a mismatch (for example a
//! frame corrupted in a way the modem's CRC missed) discards the decoder state
//! instead of delivering bad data. Completed transfers are remembered for a
//! while, so a lost final ACK causes a re-ACK, never a second delivery.
//!
//! The final ACK carries a receipt: the receiver's Ed25519 signature over
//! `"hm/xfer-receipt/v0" || receiver || sender || session || object id`. The
//! channel is broadcast and cleartext, so anyone may hear the object; only the
//! receiver's key can prove it was the receiver who got it. When the sender
//! knows the receiver's key, an ACK without a valid receipt is ignored as forged.
//!
//! The airtime budget (duty cycle with a burst allowance) protects the
//! transmitter's finals. Channel access (CSMA) is not done here; on the KISS
//! path the TNC does it, and the built-in modem will get its own MAC.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use hm_core::{DetRng, Input, Machine, Millis, Output, Port};
use hm_ident::{Identity, PublicKey};
use hm_wire::{
    Ack, Callsign, DataPreamble, Dest, FrameHeader, FrameType, ObjectId, Offer, CTRL_OFFER,
    DATA_PREAMBLE_LEN, HEADER_LEN, MAX_INDEX, MAX_OBJECT_LEN, NEED_OFFER,
};
use raptorq::{Decoder, EncodingPacket, ObjectTransmissionInformation, PayloadId, SourceBlockEncoder};

/// BLAKE3 `derive_key` context for the object hash carried in OFFER.
pub const HASH_CONTEXT: &str = "hm-net 2026-09 xfer v0";
/// RaptorQ symbol alignment; symbol sizes must be multiples of it.
pub const SYMBOL_ALIGNMENT: u16 = 8;
/// Most source symbols per object, bounding decoder memory and time.
pub const MAX_SOURCE_SYMBOLS: u32 = 8192;
/// Loss estimates are capped here so burst sizes stay finite.
const MAX_LOSS_PERMILLE: u32 = 800;
/// Largest "frames remaining" believed when predicting the end of a peer's over,
/// so one corrupted byte cannot hold our answers back for minutes.
const MAX_REMAINING_TRUSTED: u8 = 64;
/// Cap on the exponent of the retry backoff.
const MAX_BACKOFF_DOUBLINGS: u32 = 5;
/// Bursts are sized to deliver enough symbols with this probability. An extra
/// symbol costs one frame of airtime, a failed over costs a turnaround and
/// another over, so aiming for certainty wastes airtime.
pub const BURST_SUCCESS_TARGET: f64 = 0.9;

/// `P[X >= k]` for `X ~ Binomial(n, q)`.
fn prob_at_least(n: u32, k: u32, q: f64) -> f64 {
    if k == 0 {
        return 1.0;
    }
    if k > n {
        return 0.0;
    }
    if q >= 1.0 {
        return 1.0;
    }
    if q <= 0.0 {
        return 0.0;
    }
    // pmf(0) = (1 - q)^n, then pmf(i + 1) = pmf(i) * (n - i) / (i + 1) * q / (1 - q).
    let mut pmf = 1.0;
    for _ in 0..n {
        pmf *= 1.0 - q;
    }
    let ratio = q / (1.0 - q);
    let mut below = 0.0;
    for i in 0..k {
        below += pmf;
        pmf *= (n - i) as f64 / (i + 1) as f64 * ratio;
    }
    (1.0 - below).max(0.0)
}

/// Smallest burst `n >= need` (at most `cap`) that delivers `need` symbols with
/// probability [`BURST_SUCCESS_TARGET`] when each frame is lost with `loss_permille`.
pub fn burst_size(need: u32, loss_permille: u32, cap: u32) -> u32 {
    let q = 1.0 - loss_permille.min(MAX_LOSS_PERMILLE) as f64 / 1000.0;
    let mut n = need.max(1);
    while n < cap && prob_at_least(n, need, q) < BURST_SUCCESS_TARGET {
        n += 1;
    }
    n.min(cap)
}

/// Domain prefix of the receipt signature.
pub const RECEIPT_PREFIX: &[u8] = b"hm/xfer-receipt/v0";

/// The statement a receiver signs to prove it holds object `id`.
pub fn receipt_statement(receiver: Callsign, sender: Callsign, session: u16, id: &ObjectId) -> Vec<u8> {
    let mut m = Vec::with_capacity(RECEIPT_PREFIX.len() + 6 + 6 + 2 + 32);
    m.extend_from_slice(RECEIPT_PREFIX);
    m.extend_from_slice(&receiver.to_bytes());
    m.extend_from_slice(&sender.to_bytes());
    m.extend_from_slice(&session.to_be_bytes());
    m.extend_from_slice(&id.0);
    m
}

/// The hash a transfer is verified against.
pub fn object_id(bytes: &[u8]) -> ObjectId {
    ObjectId(blake3::derive_key(HASH_CONTEXT, bytes))
}

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
    /// Frame loss assumed at least, per mille, when sizing bursts.
    pub redundancy_permille: u32,
    /// Overs per object before giving up.
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
            redundancy_permille: 20,
            max_rounds: 12,
            ack_guard: Millis(1500),
            max_object_len: 256 * 1024,
            max_incoming: 8,
            max_incoming_per_sender: 2,
            idle_timeout: Millis::from_secs(300),
            done_ttl: Millis::from_secs(1800),
            duty_cycle_permille: 500,
            bucket: Millis::from_secs(120),
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
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
    fn air(&self, frames: usize, len: usize) -> Millis {
        let bits = frames as u64 * (len as u64 + self.frame_overhead_bytes as u64) * 8;
        let bits = bits + (bits * self.stuffing_permille as u64).div_ceil(1000);
        Millis((bits * 1000).div_ceil(self.bitrate_bps as u64))
    }

    fn data_frame_len(&self, symbol_size: usize) -> usize {
        HEADER_LEN + DATA_PREAMBLE_LEN + symbol_size
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    /// Transfer `object` to `to`. Precedence as in bundles, 0 routine to 3 flash;
    /// higher precedence is sent first.
    Send {
        to: Callsign,
        object: Vec<u8>,
        precedence: u8,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Failure {
    /// No complete acknowledgement within `max_rounds` overs.
    NoAnswer,
    TooLarge,
    Empty,
    /// Sending to ourselves.
    SelfAddressed,
}

/// How far a delivery confirmation could be checked.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Receipt {
    /// Signed by the receiver's known key.
    Verified,
    /// We have no key for the receiver, so the confirmation could have been forged.
    Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    /// A complete object arrived and matched its hash. Emitted once per object
    /// per `done_ttl`, however many times it is sent.
    Received {
        from: Callsign,
        id: ObjectId,
        object: Vec<u8>,
    },
    /// The receiver confirmed the whole object.
    Delivered {
        to: Callsign,
        id: ObjectId,
        rounds: u8,
        receipt: Receipt,
    },
    Failed {
        to: Callsign,
        id: ObjectId,
        reason: Failure,
    },
}

struct Pending {
    to: Callsign,
    object: Vec<u8>,
    precedence: u8,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum OutState {
    /// Send the next over at or after this time.
    Ready { at: Millis },
    /// Over sent; waiting for the ACK until this time.
    Waiting { until: Millis },
}

struct Outgoing {
    to: Callsign,
    session: u16,
    id: ObjectId,
    len: u32,
    precedence: u8,
    encoder: SourceBlockEncoder,
    source: Vec<EncodingPacket>,
    k: u32,
    next_esi: u32,
    rounds: u8,
    offer_next: bool,
    /// Receiver's last reported deficit (K before any ACK).
    need: u32,
    /// Next over is a short probe after a missing ACK.
    probe: bool,
    /// Missing ACKs in a row; drives the random backoff.
    timeouts: u32,
    /// Airtime of the last over, the unit of the backoff.
    last_cost: Millis,
    sent_last_round: u32,
    state: OutState,
}

struct Incoming {
    id: Option<ObjectId>,
    len: u32,
    symbol_size: u16,
    k: u32,
    decoder: Decoder,
    esis: BTreeSet<u32>,
    decoded: Option<Vec<u8>>,
    done: bool,
    /// Our signature proving completion, sent in every final ACK.
    receipt: Option<[u8; 64]>,
    ack_at: Option<Millis>,
    last_heard: Millis,
}

impl Incoming {
    fn new(len: u32, symbol_size: u16, k: u32, now: Millis) -> Incoming {
        Incoming {
            id: None,
            len,
            symbol_size,
            k,
            decoder: Decoder::new(oti(len, symbol_size)),
            esis: BTreeSet::new(),
            decoded: None,
            done: false,
            receipt: None,
            ack_at: None,
            last_heard: now,
        }
    }

    fn reset_decoder(&mut self) {
        self.decoder = Decoder::new(oti(self.len, self.symbol_size));
        self.esis.clear();
        self.decoded = None;
    }

    /// Eviction order when slots run out: least valuable first.
    fn value(&self) -> (bool, bool, usize, Millis) {
        (self.done, self.id.is_some(), self.esis.len(), self.last_heard)
    }
}

/// Only called with parameters that passed [`Xfer::check_params`].
fn oti(len: u32, symbol_size: u16) -> ObjectTransmissionInformation {
    ObjectTransmissionInformation::new(len as u64, symbol_size, 1, 1, SYMBOL_ALIGNMENT as u8)
}

/// The transfer engine of one station on one radio port.
pub struct Xfer {
    cfg: Config,
    identity: Identity,
    /// Public keys of peers, by base callsign, for checking their receipts.
    keys: BTreeMap<Callsign, PublicKey>,
    /// ACKs claiming completion without a valid receipt from a known key.
    rejected_receipts: u64,
    rng: DetRng,
    queue: VecDeque<Pending>,
    active: Option<Outgoing>,
    incoming: BTreeMap<(Callsign, u16), Incoming>,
    /// Objects delivered to our application recently, with their expiry.
    seen: BTreeMap<ObjectId, Millis>,
    /// Estimated frame loss towards each peer, per mille.
    loss: BTreeMap<Callsign, u32>,
    tokens_ms: i64,
    tokens_at: Millis,
}

impl Xfer {
    /// `identity` signs our receipts; it should be the key bound to `cfg.me`.
    pub fn new(cfg: Config, identity: Identity, rng: DetRng) -> Result<Xfer, &'static str> {
        cfg.validate()?;
        let tokens_ms = cfg.bucket.0 as i64;
        Ok(Xfer {
            cfg,
            identity,
            keys: BTreeMap::new(),
            rejected_receipts: 0,
            rng,
            queue: VecDeque::new(),
            active: None,
            incoming: BTreeMap::new(),
            seen: BTreeMap::new(),
            loss: BTreeMap::new(),
            tokens_ms,
            tokens_at: Millis::ZERO,
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Trust `key` for `call` (any SSID): its receipts are then required and checked.
    pub fn trust(&mut self, call: Callsign, key: PublicKey) {
        self.keys.insert(call.base(), key);
    }

    /// Completion ACKs ignored because their receipt did not verify.
    pub fn rejected_receipts(&self) -> u64 {
        self.rejected_receipts
    }

    /// Estimated frame loss towards `peer`, per mille.
    pub fn loss_estimate(&self, peer: Callsign) -> u32 {
        self.loss.get(&peer).copied().unwrap_or(0)
    }

    /// Transfers waiting or in progress.
    pub fn outgoing_count(&self) -> usize {
        self.queue.len() + self.active.is_some() as usize
    }

    fn check_params(&self, len: u32, symbol_size: usize) -> Option<(u16, u32)> {
        if len == 0 || len > self.cfg.max_object_len || len > MAX_OBJECT_LEN {
            return None;
        }
        let t = u16::try_from(symbol_size).ok()?;
        if t == 0 || !t.is_multiple_of(SYMBOL_ALIGNMENT) {
            return None;
        }
        let k = len.div_ceil(t as u32);
        (k <= MAX_SOURCE_SYMBOLS).then_some((t, k))
    }

    fn frame(&self, ftype: FrameType, to: Callsign, session: u16, index: u32, payload: &[u8]) -> Vec<u8> {
        FrameHeader {
            ftype,
            src: self.cfg.me,
            dst: Dest::Station(to),
            session,
            index,
        }
        .frame(payload)
        .expect("index is below 2^24 by construction")
    }

    fn transmit(&self, data: Vec<u8>, out: &mut Vec<Output<Event>>) {
        out.push(Output::Transmit {
            port: self.cfg.port,
            data,
        });
    }

    // ---- airtime budget -------------------------------------------------

    fn refill(&mut self, now: Millis) {
        let elapsed = now.saturating_sub(self.tokens_at).0 as i64;
        let gained = elapsed * self.cfg.duty_cycle_permille as i64 / 1000;
        self.tokens_ms = (self.tokens_ms + gained).min(self.cfg.bucket.0 as i64);
        self.tokens_at = self.tokens_at.max(now);
    }

    /// When `cost` ms of airtime will be affordable. A full bucket always is,
    /// so a single frame longer than the bucket cannot block forever.
    fn affordable_at(&self, now: Millis, cost: Millis) -> Millis {
        let short = cost.0.min(self.cfg.bucket.0) as i64 - self.tokens_ms;
        if short <= 0 || self.cfg.duty_cycle_permille >= 1000 {
            now
        } else {
            now + Millis((short as u64 * 1000).div_ceil(self.cfg.duty_cycle_permille as u64))
        }
    }

    // ---- sending --------------------------------------------------------

    fn enqueue(
        &mut self,
        now: Millis,
        to: Callsign,
        object: Vec<u8>,
        precedence: u8,
        out: &mut Vec<Output<Event>>,
    ) {
        let id = object_id(&object);
        let reason = if to == self.cfg.me {
            Some(Failure::SelfAddressed)
        } else if object.is_empty() {
            Some(Failure::Empty)
        } else if object.len() as u64 > self.cfg.max_object_len as u64
            || self
                .check_params(object.len() as u32, self.cfg.symbol_size as usize)
                .is_none()
        {
            Some(Failure::TooLarge)
        } else {
            None
        };
        if let Some(reason) = reason {
            out.push(Output::Event(Event::Failed { to, id, reason }));
            return;
        }
        // Stable: after everything of equal or higher precedence.
        let pos = self
            .queue
            .iter()
            .position(|p| p.precedence < precedence)
            .unwrap_or(self.queue.len());
        self.queue.insert(
            pos,
            Pending {
                to,
                object,
                precedence,
            },
        );
        let _ = now;
    }

    fn start_next(&mut self, now: Millis) {
        if self.active.is_some() {
            return;
        }
        let Some(p) = self.queue.pop_front() else { return };
        let len = p.object.len() as u32;
        let (t, k) = self
            .check_params(len, self.cfg.symbol_size as usize)
            .expect("checked when queued");
        // The block encoder wants whole symbols; the decoder truncates to `len` again.
        let mut padded = p.object;
        padded.resize(k as usize * t as usize, 0);
        let encoder = SourceBlockEncoder::new(0, &oti(len, t), &padded);
        padded.truncate(len as usize);
        let id = object_id(&padded);
        let source = encoder.source_packets();
        self.active = Some(Outgoing {
            to: p.to,
            session: self.rng.next_u64() as u16,
            id,
            len,
            precedence: p.precedence,
            encoder,
            source,
            k,
            next_esi: 0,
            rounds: 0,
            offer_next: true,
            need: k,
            probe: false,
            timeouts: 0,
            last_cost: Millis::ZERO,
            sent_last_round: 0,
            state: OutState::Ready { at: now },
        });
    }

    fn symbol(o: &Outgoing, esi: u32) -> Vec<u8> {
        if esi < o.k {
            o.source[esi as usize].data().to_vec()
        } else {
            o.encoder.repair_packets(esi - o.k, 1).remove(0).split().1
        }
    }

    fn receiving(&self) -> bool {
        self.incoming.values().any(|i| i.ack_at.is_some())
    }

    /// Start the next over if one is due and the channel is ours.
    fn pump(&mut self, now: Millis, out: &mut Vec<Output<Event>>) {
        self.start_next(now);
        if self.receiving() {
            return; // a peer's over is still under way; answer it first
        }
        let Some(o) = self.active.as_ref() else { return };
        let OutState::Ready { at } = o.state else { return };
        if at > now {
            return;
        }
        if o.rounds >= self.cfg.max_rounds {
            let o = self.active.take().expect("checked");
            out.push(Output::Event(Event::Failed {
                to: o.to,
                id: o.id,
                reason: Failure::NoAnswer,
            }));
            self.start_next(now);
            return self.pump(now, out);
        }

        let loss = self.loss_estimate(o.to).max(self.cfg.redundancy_permille);
        let cap = (self.cfg.max_burst as u32)
            .min(MAX_INDEX - o.next_esi.min(MAX_INDEX))
            .max(1);
        let n = if o.probe {
            o.need.clamp(1, 2).min(cap)
        } else {
            burst_size(o.need, loss, cap)
        };
        let t = self.cfg.symbol_size as usize;
        let frame_air = self.cfg.air(1, self.cfg.data_frame_len(t));
        let mut fixed = self.cfg.txdelay;
        if o.offer_next {
            fixed += self.cfg.air(1, HEADER_LEN + hm_wire::OFFER_LEN);
        }
        // An over never costs more than the bucket holds (but always carries a symbol).
        let n = if self.cfg.duty_cycle_permille < 1000 {
            let room = self.cfg.bucket.0.saturating_sub(fixed.0) / frame_air.0.max(1);
            n.min(room.max(1) as u32)
        } else {
            n
        };
        let cost = fixed + Millis(frame_air.0 * n as u64);
        self.refill(now);
        let when = self.affordable_at(now, cost);
        if when > now {
            self.active.as_mut().expect("checked").state = OutState::Ready { at: when };
            return;
        }

        let o = self.active.as_ref().expect("checked");
        let mut frames = Vec::with_capacity(n as usize + 1);
        if o.offer_next {
            let offer = Offer {
                hash: o.id.0,
                object_len: o.len,
                symbol_size: t as u16,
                precedence: o.precedence,
                remaining: n as u8,
            };
            frames.push(self.frame(
                FrameType::Ctrl,
                o.to,
                o.session,
                0,
                &offer.to_bytes().expect("len checked"),
            ));
        }
        for i in 0..n {
            let esi = o.next_esi + i;
            let pre = DataPreamble {
                object_len: o.len,
                remaining: (n - 1 - i) as u8,
            };
            let mut payload = pre.to_bytes().expect("len checked").to_vec();
            payload.extend_from_slice(&Self::symbol(o, esi));
            frames.push(self.frame(FrameType::Data, o.to, o.session, esi, &payload));
        }
        for f in frames {
            self.transmit(f, out);
        }
        self.tokens_ms -= cost.0 as i64;

        // The peer answers after our over: its guard, its key-up, a full ACK, our slack.
        let ack_air = self.cfg.txdelay + self.cfg.air(1, HEADER_LEN + 7 + 8 + hm_wire::RECEIPT_LEN);
        let jitter = Millis(self.rng.below(self.cfg.ack_guard.0 + 1));
        let until = now + cost + self.cfg.ack_guard + ack_air + self.cfg.ack_guard + jitter;
        let o = self.active.as_mut().expect("checked");
        o.next_esi += n;
        o.rounds += 1;
        o.offer_next = false;
        o.probe = false;
        o.last_cost = cost;
        o.sent_last_round = n;
        o.state = OutState::Waiting { until };
    }

    fn on_ack(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        payload: &[u8],
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok(ack) = Ack::decode(payload) else { return };
        let Some(o) = self.active.as_mut() else { return };
        if o.to != from || o.session != session {
            return;
        }
        o.timeouts = 0;
        if ack.need == 0 || ack.completed.contains(&o.id.prefix8()) {
            let receipt = match self.keys.get(&o.to.base()) {
                None => Receipt::Unverified,
                Some(key) => {
                    let statement = receipt_statement(o.to, self.cfg.me, o.session, &o.id);
                    match ack.receipt {
                        Some(sig) if key.verify(&statement, &sig).is_ok() => Receipt::Verified,
                        _ => {
                            // Forged, or signed by another key: carry on as if unheard.
                            self.rejected_receipts += 1;
                            return;
                        }
                    }
                }
            };
            let o = self.active.take().expect("checked");
            let p = self.loss.entry(o.to).or_insert(0);
            *p = *p * 9 / 10;
            out.push(Output::Event(Event::Delivered {
                to: o.to,
                id: o.id,
                rounds: o.rounds,
                receipt,
            }));
            return;
        }
        if ack.need == NEED_OFFER {
            o.offer_next = true;
        } else {
            let new_need = ack.need as u32;
            let sent = o.sent_last_round;
            if sent > 0 && new_need <= o.need {
                let got = (o.need - new_need).min(sent);
                let observed = (sent - got) * 1000 / sent;
                let p = self.loss.entry(o.to).or_insert(0);
                *p = (*p * 7 + observed * 3) / 10;
            }
            o.need = new_need;
        }
        o.state = OutState::Ready { at: now };
    }

    /// No ACK: probe again after a random, exponentially growing backoff, so
    /// stations that cannot hear each other stop colliding in lockstep.
    fn on_ack_timeout(&mut self, now: Millis) {
        let Some(o) = self.active.as_mut() else { return };
        if let OutState::Waiting { until } = o.state {
            if now >= until {
                let p = self.loss.entry(o.to).or_insert(0);
                *p = ((*p * 7 + 1000 * 3) / 10).min(MAX_LOSS_PERMILLE);
                o.timeouts += 1;
                let window = (o.last_cost.0 + self.cfg.ack_guard.0) << o.timeouts.min(MAX_BACKOFF_DOUBLINGS);
                let backoff = Millis(self.rng.below(window + 1));
                o.offer_next = true;
                o.probe = true;
                o.state = OutState::Ready { at: now + backoff };
            }
        }
    }

    // ---- receiving ------------------------------------------------------

    fn ack_at(&self, now: Millis, remaining: u8, symbol_size: usize) -> Millis {
        let remaining = remaining.min(MAX_REMAINING_TRUSTED) as usize;
        now + self.cfg.air(remaining, self.cfg.data_frame_len(symbol_size)) + self.cfg.ack_guard
    }

    /// We answer once every over we are hearing has ended, so an ACK never
    /// talks over another station's over.
    fn answer_at(&self) -> Option<Millis> {
        self.incoming.values().filter_map(|i| i.ack_at).max()
    }

    /// Make room for a new transfer from `sender`. A sender at its own limit
    /// loses its least valuable transfer; otherwise, when all slots are taken, the
    /// least valuable transfer overall goes. Value: finished, then OFFER seen, then
    /// symbols collected, then most recently heard.
    fn make_room(&mut self, now: Millis, sender: Callsign) {
        let idle = self.cfg.idle_timeout;
        self.incoming.retain(|_, i| i.done || i.last_heard + idle > now);
        let from_sender = self.incoming.keys().filter(|(c, _)| *c == sender).count();
        let victim = if from_sender >= self.cfg.max_incoming_per_sender {
            self.incoming
                .iter()
                .filter(|((c, _), _)| *c == sender)
                .min_by_key(|(_, i)| i.value())
        } else if self.incoming.len() >= self.cfg.max_incoming {
            self.incoming.iter().min_by_key(|(_, i)| i.value())
        } else {
            None
        };
        if let Some(k) = victim.map(|(k, _)| *k) {
            self.incoming.remove(&k);
        }
    }

    fn slot(
        &mut self,
        now: Millis,
        key: (Callsign, u16),
        len: u32,
        t: u16,
        k: u32,
        authoritative: bool,
    ) -> bool {
        match self.incoming.get(&key) {
            Some(i) if i.len == len && i.symbol_size == t => return true,
            Some(_) if !authoritative => return false,
            Some(_) => {}
            None => self.make_room(now, key.0),
        }
        self.incoming.insert(key, Incoming::new(len, t, k, now));
        true
    }

    fn on_offer(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        payload: &[u8],
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok(offer) = hm_wire::Offer::decode(payload) else {
            return;
        };
        let Some((t, k)) = self.check_params(offer.object_len, offer.symbol_size as usize) else {
            return;
        };
        let key = (from, session);
        let id = ObjectId(offer.hash);
        if self
            .incoming
            .get(&key)
            .is_some_and(|i| i.id.is_some_and(|known| known != id))
        {
            self.incoming.remove(&key); // a new object on a reused session
        }
        if !self.slot(now, key, offer.object_len, t, k, true) {
            return;
        }
        let ack_at = self.ack_at(now, offer.remaining, t as usize);
        let already = self.seen.contains_key(&id);
        let receipt = self.sign_receipt(key, &id);
        let inc = self.incoming.get_mut(&key).expect("slot ensured");
        inc.id = Some(id);
        inc.last_heard = now;
        inc.ack_at = Some(ack_at);
        if already {
            inc.done = true;
            inc.receipt = Some(receipt);
        } else if let Some(data) = inc.decoded.take() {
            self.finish(now, key, data, out);
        }
    }

    fn on_data(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        esi: u32,
        payload: &[u8],
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok((pre, symbol)) = DataPreamble::decode(payload) else {
            return;
        };
        let Some((t, k)) = self.check_params(pre.object_len, symbol.len()) else {
            return;
        };
        let key = (from, session);
        if !self.slot(now, key, pre.object_len, t, k, false) {
            return;
        }
        let ack_at = self.ack_at(now, pre.remaining, t as usize);
        let inc = self.incoming.get_mut(&key).expect("slot ensured");
        inc.last_heard = now;
        inc.ack_at = Some(ack_at);
        if inc.done || inc.decoded.is_some() || !inc.esis.insert(esi) {
            return;
        }
        let packet = EncodingPacket::new(PayloadId::new(0, esi), symbol.to_vec());
        if let Some(data) = inc.decoder.decode(packet) {
            if inc.id.is_some() {
                self.finish(now, key, data, out);
            } else {
                inc.decoded = Some(data); // hold until the OFFER tells us the hash
            }
        }
    }

    fn sign_receipt(&self, key: (Callsign, u16), id: &ObjectId) -> [u8; 64] {
        self.identity
            .sign(&receipt_statement(self.cfg.me, key.0, key.1, id))
    }

    /// Check a decoded object against the offered hash and deliver it.
    fn finish(&mut self, now: Millis, key: (Callsign, u16), data: Vec<u8>, out: &mut Vec<Output<Event>>) {
        let ttl = self.cfg.done_ttl;
        let id = self.incoming[&key].id.expect("caller checked");
        if object_id(&data) != id {
            // A bad symbol got through; start over.
            self.incoming
                .get_mut(&key)
                .expect("caller holds the key")
                .reset_decoder();
            return;
        }
        let receipt = self.sign_receipt(key, &id);
        let inc = self.incoming.get_mut(&key).expect("caller holds the key");
        inc.receipt = Some(receipt);
        inc.done = true;
        inc.last_heard = now;
        if self.seen.insert(id, now + ttl).is_none() {
            out.push(Output::Event(Event::Received {
                from: key.0,
                id,
                object: data,
            }));
        }
    }

    fn send_due_acks(&mut self, now: Millis, out: &mut Vec<Output<Event>>) {
        if self.answer_at().is_none_or(|t| t > now) {
            return;
        }
        let due: Vec<(Callsign, u16)> = self
            .incoming
            .iter()
            .filter(|(_, i)| i.ack_at.is_some())
            .map(|(k, _)| *k)
            .collect();
        for key in due {
            let inc = self.incoming.get_mut(&key).expect("collected above");
            inc.ack_at = None;
            let ack = if inc.done {
                Ack {
                    need: 0,
                    completed: inc.id.iter().map(ObjectId::prefix8).collect(),
                    receipt: inc.receipt,
                    ..Ack::default()
                }
            } else if inc.id.is_none() {
                Ack {
                    need: NEED_OFFER,
                    ..Ack::default()
                }
            } else {
                let have = inc.esis.len() as u32;
                let need = if have < inc.k { inc.k - have } else { 1 };
                Ack {
                    need: need.min(NEED_OFFER as u32 - 1) as u16,
                    ..Ack::default()
                }
            };
            let payload = ack.to_vec().expect("fields in range");
            let f = self.frame(FrameType::Ack, key.0, key.1, 0, &payload);
            self.transmit(f, out);
        }
    }

    fn expire(&mut self, now: Millis) {
        let (idle, ttl) = (self.cfg.idle_timeout, self.cfg.done_ttl);
        self.incoming
            .retain(|_, i| i.last_heard + if i.done { ttl } else { idle } > now);
        self.seen.retain(|_, until| *until > now);
    }

    fn on_frame(&mut self, now: Millis, data: &[u8], out: &mut Vec<Output<Event>>) {
        let Ok((h, payload)) = FrameHeader::decode(data) else {
            return;
        };
        if h.dst != Dest::Station(self.cfg.me) || h.src == self.cfg.me {
            return;
        }
        match h.ftype {
            FrameType::Data => self.on_data(now, h.src, h.session, h.index, payload, out),
            FrameType::Ctrl if payload.first() == Some(&CTRL_OFFER) => {
                self.on_offer(now, h.src, h.session, payload, out)
            }
            FrameType::Ack => self.on_ack(now, h.src, h.session, payload, out),
            _ => {}
        }
    }
}

impl Machine for Xfer {
    type Input = Input<Command>;
    type Output = Output<Event>;

    fn handle(&mut self, now: Millis, input: Self::Input, out: &mut Vec<Self::Output>) {
        match input {
            Input::Command(Command::Send {
                to,
                object,
                precedence,
            }) => self.enqueue(now, to, object, precedence, out),
            Input::Frame { port, data } if port == self.cfg.port => self.on_frame(now, &data, out),
            Input::Frame { .. } => {}
        }
        self.pump(now, out);
    }

    fn on_deadline(&mut self, now: Millis, out: &mut Vec<Self::Output>) {
        self.send_due_acks(now, out);
        self.on_ack_timeout(now);
        self.expire(now);
        self.pump(now, out);
    }

    fn next_deadline(&self) -> Option<Millis> {
        let mut d: Option<Millis> = None;
        let mut take = |t: Millis| d = Some(d.map_or(t, |x| x.min(t)));
        let receiving = self.receiving();
        if let Some(t) = self.answer_at() {
            take(t);
        }
        for i in self.incoming.values() {
            take(
                i.last_heard
                    + if i.done {
                        self.cfg.done_ttl
                    } else {
                        self.cfg.idle_timeout
                    },
            );
        }
        if let Some(t) = self.seen.values().min() {
            take(*t);
        }
        match self.active.as_ref().map(|o| o.state) {
            Some(OutState::Waiting { until }) => take(until),
            // While a peer's over is under way its ACK deadline wakes us instead.
            Some(OutState::Ready { at }) if !receiving => take(at),
            _ => {}
        }
        d
    }
}

#[cfg(test)]
mod tests;
