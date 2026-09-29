//! Transfer of whole objects (signed envelopes, attachments) from one station
//! to another over a half-duplex radio link.
//!
//! One over from the sender carries an OFFER and a burst of RaptorQ symbols
//! (RFC 6330). The receiver needs any K of them, where K = ceil(len / symbol
//! size), so lost frames are never retransmitted: the next over simply
//! carries fresh repair symbols. After each over the receiver sends one ACK
//! saying how many symbols it still needs; the sender sizes its next burst from
//! that and from its belief about the link's frame loss ([`hm_model::Erasure`]):
//! the burst that minimises the expected airtime to finish, under the
//! Beta-binomial predictive of how many frames arrive. Each ACK's count is
//! reported ([`Event::Over`]) so the station's link model learns from it, and
//! the station hands its belief back ([`Command::Belief`]) for the next
//! transfer.
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
//! Before the first transfer to a peer, the first over also carries an OPEN:
//! what this station offers (feature bits) and the largest object, symbol and
//! number of parallel transfers it accepts. The peer answers with its own OPEN
//! next to its ACK, so neither costs a turnaround. A receiver that cannot take
//! a transfer says so with a CLOSE (too large, busy until a given time, or
//! refused) instead of leaving the sender to retry into silence.
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
use hm_model::{Bearer, ChannelModel, ChannelObservation, Erasure, LinkPrior, OverCost, Prior};
use hm_wire::{
    Ack, Callsign, Close, CloseReason, DataPreamble, Dest, FrameHeader, FrameType, ObjectId, Offer, Open,
    CTRL_CLOSE, CTRL_OFFER, CTRL_OPEN, DATA_PREAMBLE_LEN, HEADER_LEN, MAX_INDEX, MAX_OBJECT_LEN, NEED_OFFER,
    OPEN_LEN, OPEN_REPLY,
};
use raptorq::{Decoder, EncodingPacket, ObjectTransmissionInformation, PayloadId, SourceBlockEncoder};

pub mod beacon;

/// BLAKE3 `derive_key` context for the object hash carried in OFFER.
pub const HASH_CONTEXT: &str = "hm-net 2026-09 xfer v0";
/// RaptorQ symbol alignment; symbol sizes must be multiples of it.
pub const SYMBOL_ALIGNMENT: u16 = 8;
/// Most source symbols per object, bounding decoder memory and time.
pub const MAX_SOURCE_SYMBOLS: u32 = 8192;
/// Fade dispersion assumed at a broadcast's unknown listeners.
const LOSS_PRIOR_DISPERSION: f64 = 0.1;
/// Listeners a broadcast is sized for until the station says how many share
/// the channel.
const DEFAULT_LISTENERS: u32 = 4;
/// Channel occupancy is taken into the belief at most this often.
const OCCUPANCY_EVERY: Millis = Millis::from_secs(10);
/// Largest "frames remaining" believed when predicting the end of a peer's over,
/// so one corrupted byte cannot hold our answers back for minutes.
const MAX_REMAINING_TRUSTED: u8 = 64;
/// Largest symbol size a receiver can take (a multiple of 8 that fits in u16).
const MAX_SYMBOL_SIZE: u16 = u16::MAX - u16::MAX % SYMBOL_ALIGNMENT;
/// Cap on the exponent of the retry backoff.
const MAX_BACKOFF_DOUBLINGS: u32 = 5;
/// Smallest congestion window: a sender that keeps missing ACKs still sends
/// overs of this many symbols once a probe gets through.
const MIN_WINDOW: u32 = 2;
/// Most transfers under way at once, to different peers.
const MAX_ACTIVE: usize = 4;
/// Below 1200 bit/s, DATA frames are sized to take about this long on air.
pub const SLOW_FRAME_SECS: u64 = 3;
/// Smallest symbol a slow link is given.
const MIN_SLOW_SYMBOL: usize = 32;
/// Longest over on a link below 1200 bit/s.
pub const SLOW_MAX_OVER: Millis = Millis::from_secs(60);

/// The smallest symbol size, a multiple of 8 and at most `max`, that keeps an
/// object of `len` bytes in as few symbols as `max` would. Every symbol,
/// repair symbols included, is sent whole, so the padding after the object is
/// airtime: a 119-byte chat bundle goes out as one 120-byte symbol, not a
/// 200-byte one.
pub fn fit_symbol(len: u32, max: u16) -> u16 {
    let align = u32::from(SYMBOL_ALIGNMENT);
    let max = u32::from(max.max(SYMBOL_ALIGNMENT)) / align * align;
    if len == 0 {
        return max as u16;
    }
    let k = len.div_ceil(max);
    (len.div_ceil(k).div_ceil(align) * align).min(max) as u16
}

/// Domain prefix of the receipt signature.
pub const RECEIPT_PREFIX: &[u8] = b"hm/xfer-receipt/v0";

/// Bookkeeping callsign for RF broadcast transfers (bulletins). Frames use
/// [`Dest::Broadcast`]; this name appears only in store/events.
pub fn broadcast_peer() -> Callsign {
    Callsign::parse("ALL").expect("ALL is a valid callsign")
}

/// Frame loss at a broadcast's listeners, until the station says what it has
/// learned of its links: listeners are stations in range, whose loss rates
/// spread moderately around the channel's (a Beta worth thirty frames: about
/// ±8 %), not anywhere between none and all.
const BROADCAST_LOSS: Prior = Prior::new(0.3, 30.0);
/// Most overs a broadcast publish may take before it listens for repair requests.
const BROADCAST_MAX_ROUNDS: u8 = 2;
/// A listener missing symbols of a broadcast waits a random part of this many
/// ACK airtimes before asking, so that one request, heard by the others,
/// stands for them all.
const NACK_SPREAD_ACKS: u64 = 3;
/// Requests for at least as much, heard from other listeners, before a
/// listener keeps its own to itself: one heard request may itself have been
/// lost on the way to the broadcaster.
const NACK_SUPPRESS_AFTER: u8 = 2;

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

/// The key for a station: its own, else its base callsign's.
fn key_for(keys: &BTreeMap<Callsign, PublicKey>, call: Callsign) -> Option<&PublicKey> {
    keys.get(&call).or_else(|| keys.get(&call.base()))
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

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Transfer `object` to `to`. Precedence as in bundles, 0 routine to 3 flash;
    /// higher precedence is sent first.
    Send {
        to: Callsign,
        object: Vec<u8>,
        precedence: u8,
    },
    /// Publish `object` once on RF (`Dest::Broadcast`). No ACK wait; listeners
    /// that reconstruct it emit [`Event::Received`] and do not ACK. Completes
    /// locally as [`Event::Delivered`] to [`broadcast_peer`] with
    /// [`Receipt::Unverified`].
    Broadcast { object: Vec<u8>, precedence: u8 },
    /// The station's belief about frame loss towards `peer`: overs to it are
    /// sized from this until the next one.
    Belief { peer: Callsign, erasure: Erasure },
    /// Application durably stored (or refused) a just-received object.
    Accept {
        from: Callsign,
        id: ObjectId,
        accepted: bool,
        /// Zero means refuse permanently; otherwise ask the sender to retry.
        retry_after: u16,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Failure {
    /// `max_rounds` overs in a row brought no progress.
    NoAnswer,
    TooLarge,
    Empty,
    /// Sending to ourselves.
    SelfAddressed,
    /// The receiver said it will not take objects from us.
    Refused,
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
    /// An ACK said `got` of the `sent` frames of our last over to `to`
    /// arrived: an observation of the link's frame loss.
    Over { to: Callsign, sent: u32, got: u32 },
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
    /// RF bulletin: frames use [`Dest::Broadcast`], no ACK.
    broadcast: bool,
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
    /// Symbol size of this transfer.
    t: u16,
    precedence: u8,
    encoder: SourceBlockEncoder,
    source: Vec<EncodingPacket>,
    k: u32,
    next_esi: u32,
    /// Overs sent (saturating).
    rounds: u8,
    /// Overs in a row with no progress: counted when an over goes out, cleared
    /// by an ACK that asks for fewer symbols.
    stalls: u8,
    offer_next: bool,
    /// The next over starts with our OPEN (the peer has none from us lately).
    open_next: bool,
    /// Receiver's last reported deficit (K before any ACK).
    need: u32,
    /// Next over is a short probe after a missing ACK.
    probe: bool,
    /// Missing ACKs in a row; drives the random backoff.
    timeouts: u32,
    /// Airtime of the last over, the unit of the backoff.
    last_cost: Millis,
    sent_last_round: u32,
    /// The last over carried an OPEN, so the answer may carry one too.
    opened: bool,
    /// RF bulletin publish: no ACK; listeners may ask for repair.
    broadcast: bool,
    /// Broadcast: repair overs sent, and the most symbols a listener asked
    /// for since the last over.
    repairs: u8,
    asked: u32,
    /// Broadcast: repair windows in a row that brought no request. Two
    /// requests sent at once collide unheard, so one quiet window is not
    /// proof that nobody is missing anything.
    quiet_windows: u8,
    /// Broadcast: nothing more to send; report it done.
    finished: bool,
    /// How long to wait for the ACK once our last over has left the air: the
    /// peer's guard and answer, our slack and a random part.
    ack_wait: Millis,
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
    awaiting_application: bool,
    /// Our signature proving completion, sent in every final ACK.
    receipt: Option<[u8; 64]>,
    ack_at: Option<Millis>,
    last_heard: Millis,
    /// Heard on `Dest::Broadcast`: store locally, never ACK; ask for repair.
    broadcast: bool,
    /// Other listeners' repair requests heard since the last over that ask
    /// for at least as much as ours would.
    covering_nacks: u8,
    /// Repair requests we may still repeat if no repair follows ours.
    nack_retries: u8,
}

impl Incoming {
    fn new(len: u32, symbol_size: u16, k: u32, now: Millis, broadcast: bool) -> Incoming {
        Incoming {
            id: None,
            len,
            symbol_size,
            k,
            decoder: Decoder::new(oti(len, symbol_size)),
            esis: BTreeSet::new(),
            decoded: None,
            done: false,
            awaiting_application: false,
            receipt: None,
            ack_at: None,
            last_heard: now,
            broadcast,
            covering_nacks: 0,
            nack_retries: 0,
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
    /// Transfers under way: at most one per peer and [`MAX_ACTIVE`] in all.
    /// One over is outstanding at a time (the channel is half duplex), but
    /// while one transfer backs off after a missed ACK, another may go, so a
    /// station that does not answer cannot hold up traffic to the others.
    active: Vec<Outgoing>,
    incoming: BTreeMap<(Callsign, u16), Incoming>,
    /// Objects delivered to our application recently, with their expiry.
    seen: BTreeMap<ObjectId, Millis>,
    /// Belief about frame loss towards each peer: the station's, updated with
    /// what each ACK says arrived. A missed ACK is not counted as loss: it may
    /// as well mean a busy or colliding channel, where larger overs would make
    /// things worse.
    erasure: BTreeMap<Callsign, Erasure>,
    /// Congestion window towards each peer: most symbols in one over. Halved
    /// when an ACK does not come, grown by one with each ACK that does.
    window: BTreeMap<Callsign, u32>,
    /// How busy other stations keep the channel, from the frames we hear:
    /// every over we add waits for the channel to clear.
    channel: ChannelModel,
    /// Airtime of others' frames heard since `busy_since`.
    busy_ms: u64,
    busy_since: Millis,
    /// Stations a broadcast is sized for.
    listeners: u32,
    tokens_ms: i64,
    tokens_at: Millis,
    /// Each peer's latest OPEN, and when we heard it.
    peers: BTreeMap<Callsign, (Open, Millis)>,
    /// Peers whose OPEN we answer with our next frames to them.
    open_replies: BTreeSet<Callsign>,
    /// CLOSEs to send once the peer's over ends: (peer, session) -> (close, when).
    closes: BTreeMap<(Callsign, u16), (Close, Millis)>,
    /// Delay the custody receipt until the application confirms durable storage.
    application_ack: bool,
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
            active: Vec::new(),
            incoming: BTreeMap::new(),
            seen: BTreeMap::new(),
            erasure: BTreeMap::new(),
            window: BTreeMap::new(),
            channel: ChannelModel::default(),
            busy_ms: 0,
            busy_since: Millis::ZERO,
            listeners: DEFAULT_LISTENERS,
            tokens_ms,
            tokens_at: Millis::ZERO,
            peers: BTreeMap::new(),
            open_replies: BTreeSet::new(),
            closes: BTreeMap::new(),
            application_ack: false,
        })
    }

    /// Change the feature bits sent in OPEN (for example once the link is known).
    pub fn set_features(&mut self, features: u32) {
        self.cfg.features = features;
    }

    /// Require [`Command::Accept`] before signing the final custody ACK.
    pub fn set_application_ack(&mut self, enabled: bool) {
        self.application_ack = enabled;
    }

    /// What `peer` told us in its latest OPEN, if it sent one lately.
    pub fn peer(&self, peer: Callsign) -> Option<Open> {
        self.peers.get(&peer).map(|(o, _)| *o)
    }

    /// Our OPEN, as sent to `to`.
    fn our_open(&self, reply: bool) -> Open {
        Open {
            flags: if reply { OPEN_REPLY } else { 0 },
            features: self.cfg.features,
            max_object: self.cfg.max_object_len.min(MAX_OBJECT_LEN),
            max_symbol: MAX_SYMBOL_SIZE,
            max_parallel: self.cfg.max_incoming_per_sender.min(255) as u8,
        }
    }

    /// Whether `peer`'s OPEN is recent enough to rely on.
    fn knows(&self, peer: Callsign, now: Millis) -> bool {
        self.peers
            .get(&peer)
            .is_some_and(|(_, at)| *at + self.cfg.done_ttl > now)
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Trust `key` for `call`: its receipts are then required and checked. A
    /// callsign without an SSID covers every SSID that has no key of its own.
    pub fn trust(&mut self, call: Callsign, key: PublicKey) {
        self.keys.insert(call, key);
    }

    /// Replace every trusted key, for example after the trusted stations changed.
    pub fn set_trust(&mut self, keys: impl IntoIterator<Item = (Callsign, PublicKey)>) {
        self.keys = keys.into_iter().collect();
    }

    /// Completion ACKs ignored because their receipt did not verify.
    pub fn rejected_receipts(&self) -> u64 {
        self.rejected_receipts
    }

    /// Belief about frame loss towards `peer`: what the station told us and
    /// ACKs have said since, or, for a peer it told us nothing about, the
    /// population prior for radio links. Broadcasts ([`broadcast_peer`]) use
    /// the belief the station gave for them, or [`BROADCAST_LOSS`].
    pub fn erasure(&self, peer: Callsign) -> Erasure {
        self.erasure.get(&peer).copied().unwrap_or_else(|| {
            if peer == broadcast_peer() {
                Erasure::from_prior(BROADCAST_LOSS, LOSS_PRIOR_DISPERSION)
            } else {
                let prior = LinkPrior::for_bearer(Bearer::Radio);
                Erasure::from_prior(prior.erasure, prior.dispersion)
            }
        })
    }

    /// Stations sharing the channel, whom a broadcast is sized for.
    pub fn set_listeners(&mut self, listeners: u32) {
        self.listeners = listeners.max(1);
    }

    /// Expected share of frames lost towards `peer`, per mille.
    pub fn loss_estimate(&self, peer: Callsign) -> u32 {
        (self.erasure(peer).mean() * 1000.0 + 0.5) as u32
    }

    /// The congestion window towards `peer`: most symbols the next over to it
    /// may carry. A missed ACK halves it (to no less than 2), each ACK that
    /// arrives grows it by one, up to `max_burst`.
    pub fn window(&self, peer: Callsign) -> u32 {
        self.window
            .get(&peer)
            .copied()
            .unwrap_or(self.cfg.max_burst as u32)
    }

    /// Transfers waiting or in progress.
    pub fn outgoing_count(&self) -> usize {
        self.queue.len() + self.active.len()
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

    fn frame(
        &self,
        ftype: FrameType,
        to: Callsign,
        session: u16,
        index: u32,
        payload: &[u8],
        broadcast: bool,
    ) -> Vec<u8> {
        FrameHeader {
            ftype,
            src: self.cfg.me,
            dst: if broadcast {
                Dest::Broadcast
            } else {
                Dest::Station(to)
            },
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
        broadcast: bool,
        out: &mut Vec<Output<Event>>,
    ) {
        let id = object_id(&object);
        let reason = if !broadcast && to == self.cfg.me {
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
                broadcast,
            },
        );
        let _ = now;
    }

    /// Start queued transfers, highest precedence first, for peers that have
    /// none under way, while there is room.
    fn start_next(&mut self, now: Millis, out: &mut Vec<Output<Event>>) {
        while self.active.len() < MAX_ACTIVE {
            let busy = |p: &Pending| {
                self.active
                    .iter()
                    .any(|o| o.to == p.to && o.broadcast == p.broadcast)
            };
            let Some(i) = self.queue.iter().position(|p| !busy(p)) else {
                return;
            };
            let p = self.queue.remove(i).expect("index from position");
            self.start(now, p, out);
        }
    }

    fn start(&mut self, now: Millis, p: Pending, out: &mut Vec<Output<Event>>) {
        let len = p.object.len() as u32;
        // What the peer told us it takes: larger objects fail at once, and
        // symbols are no larger than it accepts. Broadcast has no peer OPEN.
        let mut t = self.cfg.symbol_size;
        if !p.broadcast {
            if let Some((open, _)) = self.peers.get(&p.to).filter(|_| self.knows(p.to, now)) {
                if len > open.max_object {
                    out.push(Output::Event(Event::Failed {
                        to: p.to,
                        id: object_id(&p.object),
                        reason: Failure::TooLarge,
                    }));
                    return;
                }
                let theirs = open.max_symbol - open.max_symbol % SYMBOL_ALIGNMENT;
                if theirs >= SYMBOL_ALIGNMENT {
                    t = t.min(theirs);
                }
            }
        }
        let (t, k) = match self.check_params(len, fit_symbol(len, t) as usize) {
            Some(tk) => tk,
            // Too many symbols at the peer's size: ours was checked when queued.
            None => self
                .check_params(len, fit_symbol(len, self.cfg.symbol_size) as usize)
                .expect("checked when queued"),
        };
        // The block encoder wants whole symbols; the decoder truncates to `len` again.
        let mut padded = p.object;
        padded.resize(k as usize * t as usize, 0);
        let encoder = SourceBlockEncoder::new(0, &oti(len, t), &padded);
        padded.truncate(len as usize);
        let id = object_id(&padded);
        let source = encoder.source_packets();
        let session = self.new_session();
        self.active.push(Outgoing {
            to: p.to,
            session,
            id,
            len,
            t,
            precedence: p.precedence,
            encoder,
            source,
            k,
            next_esi: 0,
            rounds: 0,
            stalls: 0,
            offer_next: true,
            open_next: !p.broadcast && self.cfg.sessions && !self.knows(p.to, now),
            need: k,
            probe: false,
            timeouts: 0,
            last_cost: Millis::ZERO,
            sent_last_round: 0,
            opened: false,
            broadcast: p.broadcast,
            repairs: 0,
            asked: 0,
            quiet_windows: 0,
            finished: false,
            ack_wait: Millis::ZERO,
            state: OutState::Ready { at: now },
        });
    }

    /// A random session id: never 0, which a CLOSE uses for "every transfer",
    /// and not one of ours already under way.
    fn new_session(&mut self) -> u16 {
        loop {
            let s = self.rng.next_u64() as u16;
            if s != 0 && self.active.iter().all(|o| o.session != s) {
                return s;
            }
        }
    }

    fn symbol(o: &Outgoing, esi: u32) -> Vec<u8> {
        if esi < o.k {
            o.source[esi as usize].data().to_vec()
        } else {
            o.encoder.repair_packets(esi - o.k, 1).remove(0).split().1
        }
    }

    fn receiving(&self) -> bool {
        self.incoming.values().any(|i| i.ack_at.is_some() && !i.broadcast)
    }

    /// One of our overs is waiting for its ACK: nothing else goes out, since
    /// the answer comes back on the same channel.
    fn waiting(&self) -> bool {
        self.active
            .iter()
            .any(|o| matches!(o.state, OutState::Waiting { .. }))
    }

    /// The transfer whose over goes next among those due now: highest
    /// precedence first, then the one due longest.
    fn next_due(&self, now: Millis) -> Option<usize> {
        self.active
            .iter()
            .enumerate()
            .filter_map(|(i, o)| match o.state {
                OutState::Ready { at } if at <= now => Some((i, o.precedence, at)),
                _ => None,
            })
            .min_by_key(|&(i, precedence, at)| (core::cmp::Reverse(precedence), at, i))
            .map(|(i, _, _)| i)
    }

    /// Start the next over if one is due and the channel is ours.
    fn pump(&mut self, now: Millis, out: &mut Vec<Output<Event>>) {
        self.start_next(now, out);
        if self.receiving() || self.waiting() {
            return; // a peer's over, or the answer to ours, is still to come
        }
        // Transfers out of rounds end before anything more is sent.
        let before = self.active.len();
        let (max_rounds, mut ended) = (self.cfg.max_rounds, Vec::new());
        self.active.retain(|o| {
            let due = matches!(o.state, OutState::Ready { at } if at <= now);
            let done = due
                && if o.broadcast {
                    o.finished
                } else {
                    o.stalls >= max_rounds
                };
            if done {
                ended.push(if o.broadcast {
                    Event::Delivered {
                        to: o.to,
                        id: o.id,
                        rounds: o.rounds,
                        receipt: Receipt::Unverified,
                    }
                } else {
                    Event::Failed {
                        to: o.to,
                        id: o.id,
                        reason: Failure::NoAnswer,
                    }
                });
            }
            !done
        });
        out.extend(ended.into_iter().map(Output::Event));
        if self.active.len() < before {
            return self.pump(now, out);
        }
        while let Some(i) = self.next_due(now) {
            if self.send_over(now, i, out) {
                return;
            }
        }
    }

    /// Send the next over of transfer `i`, or put it off until the airtime
    /// budget allows it. True when the over went out.
    fn send_over(&mut self, now: Millis, i: usize, out: &mut Vec<Output<Event>>) -> bool {
        let o = &self.active[i];
        let (to, session, broadcast) = (o.to, o.session, o.broadcast);
        let erasure = self.erasure(o.to);
        let window = if o.broadcast {
            self.cfg.max_burst as u32
        } else {
            self.window(o.to)
        };
        let cap = (self.cfg.max_burst as u32)
            .min(window)
            .min(MAX_INDEX - o.next_esi.min(MAX_INDEX))
            .max(1);
        let t = o.t as usize;
        let frame_air = self.cfg.air(1, self.cfg.data_frame_len(t));
        let (need, probe, is_broadcast) = (o.need, o.probe, o.broadcast);
        self.note_occupancy(now);
        let over_cost = self.over_cost(now, frame_air, cap, is_broadcast);
        let n = if probe {
            need.clamp(1, 2).min(cap)
        } else if is_broadcast {
            hm_model::broadcast_burst(need, cap, &erasure, over_cost, self.listeners)
        } else {
            hm_model::burst_size(need, cap, &erasure, over_cost)
        };
        let o = &self.active[i];
        let mut fixed = self.cfg.txdelay;
        if o.offer_next {
            fixed += self.cfg.air(1, HEADER_LEN + hm_wire::OFFER_LEN);
        }
        let open = !o.broadcast && ((o.offer_next && o.open_next) || self.open_replies.contains(&o.to));
        if open {
            fixed += self.open_air();
        }
        // An over never lasts longer than `max_over`, nor costs more than the
        // bucket holds, but it always carries a symbol.
        let room = |limit: u64| (limit.saturating_sub(fixed.0) / frame_air.0.max(1)).max(1) as u32;
        let mut n = n.min(room(self.cfg.max_over.0));
        if self.cfg.duty_cycle_permille < 1000 {
            n = n.min(room(self.cfg.bucket.0));
        }
        let cost = fixed + Millis(frame_air.0 * n as u64);
        self.refill(now);
        let when = self.affordable_at(now, cost);
        if when > now {
            self.active[i].state = OutState::Ready { at: when };
            return false;
        }

        let mut frames = Vec::with_capacity(n as usize + 2);
        if open {
            let reply = self.open_replies.remove(&to);
            let ours = self.our_open(reply).to_bytes().expect("in range");
            frames.push(self.frame(FrameType::Ctrl, to, session, 0, &ours, false));
        }
        let o = &self.active[i];
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
                broadcast,
            ));
        }
        for j in 0..n {
            let esi = o.next_esi + j;
            let pre = DataPreamble {
                object_len: o.len,
                remaining: (n - 1 - j) as u8,
            };
            let mut payload = pre.to_bytes().expect("len checked").to_vec();
            payload.extend_from_slice(&Self::symbol(o, esi));
            frames.push(self.frame(FrameType::Data, o.to, o.session, esi, &payload, broadcast));
        }
        for f in frames {
            self.transmit(f, out);
        }
        self.tokens_ms -= cost.0 as i64;

        // The peer answers after our over: its guard, its key-up, a full ACK
        // (and its OPEN, if we sent ours), our slack.
        let ack_air = self.ack_air() + if open { self.open_air() } else { Millis::ZERO };
        let jitter = Millis(self.rng.below(self.cfg.ack_guard.0 + 1));
        let ack_wait = self.cfg.ack_guard + ack_air + self.cfg.ack_guard + jitter;
        let o = &mut self.active[i];
        o.next_esi += n;
        o.rounds = o.rounds.saturating_add(1);
        o.stalls = o.stalls.saturating_add(1);
        o.offer_next = false;
        o.open_next = false;
        o.opened = open;
        o.probe = false;
        o.last_cost = cost;
        o.sent_last_round = n;
        if !o.broadcast {
            o.ack_wait = ack_wait;
            o.state = OutState::Waiting {
                until: now + cost + ack_wait,
            };
            return true;
        }
        // A broadcast has no ACK. Once enough source symbols went out (or the
        // round cap is reached) it listens for listeners asking for more, and
        // answers with fresh symbols: any new symbol helps every listener.
        if o.next_esi < o.k && o.rounds < BROADCAST_MAX_ROUNDS {
            o.state = OutState::Ready { at: now + cost };
        } else if o.repairs < self.cfg.broadcast_repairs {
            let repair_wait = self.repair_wait();
            let o = &mut self.active[i];
            o.asked = 0;
            o.ack_wait = repair_wait;
            o.state = OutState::Waiting {
                until: now + cost + repair_wait,
            };
        } else {
            let o = self.active.remove(i);
            out.push(Output::Event(Event::Delivered {
                to: o.to,
                id: o.id,
                rounds: o.rounds,
                receipt: Receipt::Unverified,
            }));
            self.pump(now, out);
        }
        true
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
        // A listener asking for more of our broadcast (a NACK).
        if let Some(o) = self
            .active
            .iter_mut()
            .find(|o| o.broadcast && o.session == session)
        {
            let need = if ack.need == NEED_OFFER {
                1
            } else {
                u32::from(ack.need)
            };
            o.asked = o.asked.max(need);
            return;
        }
        let Some(i) = self
            .active
            .iter()
            .position(|o| !o.broadcast && o.to == from && o.session == session)
        else {
            return;
        };
        // An answer came: the channel carried our over and the reply.
        let grown = self.window(from).saturating_add(1).min(self.cfg.max_burst as u32);
        let o = &mut self.active[i];
        o.timeouts = 0;
        if ack.need == 0 || ack.completed.contains(&o.id.prefix8()) {
            let receipt = match key_for(&self.keys, o.to) {
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
            let o = self.active.remove(i);
            // The last over brought at least what was needed; how many more
            // arrived is not known, so only an over that was all needed counts.
            if o.sent_last_round > 0 && o.sent_last_round <= o.need {
                self.observe_over(o.to, o.sent_last_round, o.sent_last_round, out);
            }
            self.window.insert(o.to, grown);
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
            if new_need < o.need {
                o.stalls = 0; // the over got something through
            }
            let sent = o.sent_last_round;
            // Every symbol that arrives is one fewer needed.
            let got = (sent > 0 && new_need <= o.need).then(|| (o.need - new_need).min(sent));
            o.need = new_need;
            o.state = OutState::Ready { at: now };
            if let Some(got) = got {
                self.observe_over(from, sent, got, out);
            }
        }
        if let Some(o) = self.active.get_mut(i) {
            o.state = OutState::Ready { at: now };
        }
        self.window.insert(from, grown);
    }

    /// No ACK: probe again after a random, exponentially growing backoff, so
    /// stations that cannot hear each other stop colliding in lockstep, and
    /// halve the congestion window. The loss estimate stays as the ACKs left
    /// it: a missing answer may mean a busy or colliding channel, where larger
    /// overs, to make up for "loss", would only make it worse.
    fn on_ack_timeout(&mut self, now: Millis) {
        for i in 0..self.active.len() {
            let OutState::Waiting { until } = self.active[i].state else {
                continue;
            };
            if now < until {
                continue;
            }
            if self.active[i].broadcast {
                // The repair window closed: answer the largest request, or finish.
                let repairs = self.cfg.broadcast_repairs;
                let repair_wait = self.repair_wait();
                let o = &mut self.active[i];
                if o.asked > 0 && o.repairs < repairs {
                    o.repairs += 1;
                    o.need = o.asked;
                    o.offer_next = true;
                    o.quiet_windows = 0;
                    o.state = OutState::Ready { at: now };
                } else if o.asked == 0 && o.quiet_windows == 0 && o.repairs < repairs {
                    // Listen once more: a request may have been lost in a collision.
                    o.quiet_windows = 1;
                    o.state = OutState::Waiting {
                        until: now + repair_wait,
                    };
                } else {
                    o.finished = true;
                    o.state = OutState::Ready { at: now };
                }
                o.asked = 0;
                continue;
            }
            let to = self.active[i].to;
            let halved = (self.window(to) / 2).max(MIN_WINDOW);
            self.window.insert(to, halved);
            let o = &mut self.active[i];
            o.timeouts += 1;
            let window = (o.last_cost.0 + self.cfg.ack_guard.0) << o.timeouts.min(MAX_BACKOFF_DOUBLINGS);
            let backoff = Millis(self.rng.below(window + 1));
            o.offer_next = true;
            o.open_next = o.opened;
            o.probe = true;
            o.state = OutState::Ready { at: now + backoff };
        }
    }

    /// The link has put on air everything we gave it, the last frame ending at
    /// `now`. A link that waits for a clear channel may have held our over
    /// back, so the wait for its ACK starts from here, not from when we
    /// handed the over to the link.
    fn on_transmitted(&mut self, now: Millis) {
        for o in &mut self.active {
            if let OutState::Waiting { .. } = o.state {
                o.state = OutState::Waiting {
                    until: now + o.ack_wait,
                };
            }
        }
    }

    // ---- receiving ------------------------------------------------------

    fn ack_at(&self, now: Millis, remaining: u8, symbol_size: usize) -> Millis {
        let remaining = remaining.min(MAX_REMAINING_TRUSTED) as usize;
        now + self.cfg.air(remaining, self.cfg.data_frame_len(symbol_size)) + self.cfg.ack_guard
    }

    /// When to ask for more of an unfinished broadcast: after the over, at a
    /// random moment, so the first request heard silences the others. None
    /// when this station does not ask for repairs.
    fn nack_at(&mut self, now: Millis, remaining: u8, symbol_size: usize) -> Option<Millis> {
        if self.cfg.broadcast_repairs == 0 {
            return None;
        }
        let spread = self.ack_air().0 * NACK_SPREAD_ACKS;
        Some(self.ack_at(now, remaining, symbol_size) + Millis(self.rng.below(spread + 1)))
    }

    /// Symbols an unfinished broadcast still lacks, as a repair request says.
    fn broadcast_need(inc: &Incoming) -> u32 {
        let have = inc.esis.len() as u32;
        if have < inc.k {
            inc.k - have
        } else {
            1
        }
    }

    /// Another listener asked the broadcaster `to` for more of `session`: if
    /// it asked for at least as much as we would, the answer covers us too.
    fn overhear_nack(&mut self, to: Callsign, session: u16, payload: &[u8]) {
        let Ok(ack) = Ack::decode(payload) else { return };
        let Some(inc) = self.incoming.get_mut(&(to, session)) else {
            return;
        };
        if !inc.broadcast || inc.done || inc.ack_at.is_none() {
            return;
        }
        let asked = if ack.need == NEED_OFFER {
            1
        } else {
            u32::from(ack.need)
        };
        if asked >= Self::broadcast_need(inc) {
            inc.covering_nacks = inc.covering_nacks.saturating_add(1);
            if inc.covering_nacks >= NACK_SUPPRESS_AFTER {
                inc.ack_at = None;
            }
        }
    }

    /// We answer once every over we are hearing has ended, so an ACK never
    /// talks over another station's over.
    fn answer_at(&self) -> Option<Millis> {
        let acks = self
            .incoming
            .values()
            .filter(|i| !i.broadcast)
            .filter_map(|i| i.ack_at);
        acks.chain(self.closes.values().map(|(_, at)| *at)).max()
    }

    /// The next moment we ask a broadcaster for more (independent of our
    /// answers to unicast senders, which must not wait for it).
    fn nack_due(&self) -> Option<Millis> {
        self.incoming
            .values()
            .filter(|i| i.broadcast)
            .filter_map(|i| i.ack_at)
            .min()
    }

    /// Whether a new transfer from `sender` would have to push out work in
    /// progress from other stations: every slot holds a live, offered,
    /// unfinished transfer from someone else. Then we ask it to come back later.
    fn busy_for(&self, now: Millis, sender: Callsign) -> bool {
        let idle = self.cfg.idle_timeout;
        let live = |i: &Incoming| !i.done && i.id.is_some() && i.last_heard + idle > now;
        let others = self.incoming.iter().filter(|((c, _), _)| *c != sender);
        let from_sender = self.incoming.keys().filter(|(c, _)| *c == sender).count();
        from_sender < self.cfg.max_incoming_per_sender
            && self.incoming.len() >= self.cfg.max_incoming
            && others.clone().count() == self.incoming.len()
            && others.map(|(_, i)| i).all(live)
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

    #[allow(clippy::too_many_arguments)]
    fn slot(
        &mut self,
        now: Millis,
        key: (Callsign, u16),
        len: u32,
        t: u16,
        k: u32,
        authoritative: bool,
        broadcast: bool,
    ) -> bool {
        match self.incoming.get(&key) {
            Some(i) if i.len == len && i.symbol_size == t => {
                if broadcast {
                    self.incoming.get_mut(&key).expect("checked").broadcast = true;
                }
                return true;
            }
            Some(_) if !authoritative => return false,
            Some(_) => {}
            None => self.make_room(now, key.0),
        }
        self.incoming
            .insert(key, Incoming::new(len, t, k, now, broadcast));
        true
    }

    fn on_offer(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        payload: &[u8],
        broadcast: bool,
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok(offer) = hm_wire::Offer::decode(payload) else {
            return;
        };
        let key = (from, session);
        let answer = self.ack_at(now, offer.remaining, offer.symbol_size as usize);
        if offer.object_len > self.cfg.max_object_len {
            if !broadcast {
                let close = Close {
                    reason: CloseReason::TooLarge,
                    retry_after: 0,
                };
                self.closes.insert(key, (close, answer));
            }
            return;
        }
        let Some((t, k)) = self.check_params(offer.object_len, offer.symbol_size as usize) else {
            return;
        };
        let id = ObjectId(offer.hash);
        if !self.incoming.contains_key(&key) && !self.seen.contains_key(&id) && self.busy_for(now, from) {
            if !broadcast {
                let close = Close {
                    reason: CloseReason::Busy,
                    retry_after: self.cfg.busy_retry_secs,
                };
                self.closes.insert(key, (close, answer));
            }
            return;
        }
        if self
            .incoming
            .get(&key)
            .is_some_and(|i| i.id.is_some_and(|known| known != id))
        {
            self.incoming.remove(&key); // a new object on a reused session
        }
        if !self.slot(now, key, offer.object_len, t, k, true, broadcast) {
            return;
        }
        let already = self.seen.contains_key(&id);
        let receipt = self.sign_receipt(key, &id);
        let ack_at = if broadcast {
            let done = already || self.incoming.get(&key).is_some_and(|i| i.done);
            if done {
                None
            } else {
                self.nack_at(now, offer.remaining, offer.symbol_size as usize)
            }
        } else {
            Some(self.ack_at(now, offer.remaining, t as usize))
        };
        let inc = self.incoming.get_mut(&key).expect("slot ensured");
        inc.id = Some(id);
        inc.last_heard = now;
        inc.broadcast = broadcast;
        // Broadcast listeners never ACK (would storm the channel).
        inc.ack_at = ack_at;
        if already {
            inc.done = true;
            if !broadcast {
                inc.receipt = Some(receipt);
            }
        } else if let Some(data) = inc.decoded.take() {
            self.finish(now, key, data, out);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_data(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        esi: u32,
        payload: &[u8],
        broadcast: bool,
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok((pre, symbol)) = DataPreamble::decode(payload) else {
            return;
        };
        let Some((t, k)) = self.check_params(pre.object_len, symbol.len()) else {
            return;
        };
        let key = (from, session);
        // Every DATA frame repeats the object length: if it fits after all, the
        // OFFER that seemed too large was corrupted on the way. Take back the CLOSE.
        if let Some((close, _)) = self.closes.get(&key) {
            if close.reason == CloseReason::TooLarge && pre.object_len <= self.cfg.max_object_len {
                self.closes.remove(&key);
            }
        }
        // Symbols for a transfer we are turning away, or would have to make
        // room for by dropping others' work, are not collected.
        if self.closes.contains_key(&key) || (!self.incoming.contains_key(&key) && self.busy_for(now, from)) {
            return;
        }
        if !self.slot(now, key, pre.object_len, t, k, false, broadcast) {
            return;
        }
        let ack_at = if broadcast {
            if self.incoming.get(&key).is_some_and(|i| i.done) {
                None
            } else {
                self.nack_at(now, pre.remaining, t as usize)
            }
        } else {
            Some(self.ack_at(now, pre.remaining, t as usize))
        };
        let inc = self.incoming.get_mut(&key).expect("slot ensured");
        inc.last_heard = now;
        if broadcast {
            inc.broadcast = true;
            // A new over: requests heard after it are counted afresh.
            inc.covering_nacks = 0;
            inc.nack_retries = 1;
        }
        inc.ack_at = ack_at;
        if inc.done || inc.awaiting_application || inc.decoded.is_some() || !inc.esis.insert(esi) {
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
        let id = self.incoming[&key].id.expect("caller checked");
        if object_id(&data) != id {
            // A bad symbol got through; start over.
            self.incoming
                .get_mut(&key)
                .expect("caller holds the key")
                .reset_decoder();
            return;
        }
        if self.application_ack {
            let inc = self.incoming.get_mut(&key).expect("caller holds the key");
            inc.awaiting_application = true;
            inc.last_heard = now;
            out.push(Output::Event(Event::Received {
                from: key.0,
                id,
                object: data,
            }));
        } else {
            self.complete_incoming(now, key, id);
            out.push(Output::Event(Event::Received {
                from: key.0,
                id,
                object: data,
            }));
        }
    }

    fn complete_incoming(&mut self, now: Millis, key: (Callsign, u16), id: ObjectId) {
        let broadcast = self.incoming[&key].broadcast;
        let receipt = if broadcast {
            None
        } else {
            Some(self.sign_receipt(key, &id))
        };
        let inc = self.incoming.get_mut(&key).expect("caller holds the key");
        inc.receipt = receipt;
        inc.done = true;
        inc.awaiting_application = false;
        inc.last_heard = now;
        if broadcast {
            inc.ack_at = None;
        }
        self.seen.insert(id, now + self.cfg.done_ttl);
    }

    fn application_verdict(
        &mut self,
        now: Millis,
        from: Callsign,
        id: ObjectId,
        accepted: bool,
        retry_after: u16,
    ) {
        let key = self
            .incoming
            .iter()
            .find(|((peer, _), incoming)| {
                *peer == from && incoming.id == Some(id) && incoming.awaiting_application
            })
            .map(|(key, _)| *key);
        let Some(key) = key else { return };
        if accepted {
            let broadcast = self.incoming[&key].broadcast;
            self.complete_incoming(now, key, id);
            if !broadcast {
                self.incoming.get_mut(&key).expect("completed above").ack_at = Some(now);
            }
            return;
        }
        self.incoming.remove(&key);
        self.closes.insert(
            key,
            (
                Close {
                    reason: if retry_after == 0 {
                        CloseReason::Refused
                    } else {
                        CloseReason::Busy
                    },
                    retry_after,
                },
                now,
            ),
        );
    }

    fn send_due_acks(&mut self, now: Millis, out: &mut Vec<Output<Event>>) {
        let repair_wait = self.repair_wait();
        let nacks: Vec<(Callsign, u16)> = self
            .incoming
            .iter()
            .filter(|(_, i)| i.broadcast && i.ack_at.is_some_and(|at| at <= now))
            .map(|(k, _)| *k)
            .collect();
        for key in nacks {
            let inc = self.incoming.get_mut(&key).expect("collected above");
            inc.ack_at = None;
            if inc.done {
                continue;
            }
            let need = Self::broadcast_need(inc);
            // If no repair follows, ask once more a window later.
            if inc.nack_retries > 0 {
                inc.nack_retries -= 1;
                inc.ack_at = Some(now + repair_wait);
            }
            let ack = Ack {
                need: need.min(NEED_OFFER as u32 - 1) as u16,
                ..Ack::default()
            };
            let payload = ack.to_vec().expect("fields in range");
            let f = self.frame(FrameType::Ack, key.0, key.1, 0, &payload, false);
            self.transmit(f, out);
        }
        if self.answer_at().is_none_or(|t| t > now) {
            return;
        }
        let due: Vec<(Callsign, u16)> = self
            .incoming
            .iter()
            .filter(|(_, i)| i.ack_at.is_some() && !i.broadcast)
            .map(|(k, _)| *k)
            .collect();

        let closes: Vec<((Callsign, u16), Close)> = core::mem::take(&mut self.closes)
            .into_iter()
            .map(|(k, (c, _))| (k, c))
            .collect();
        for (key, close) in closes {
            // Our OPEN first, so the sender learns the limit it ran into.
            if self.open_replies.remove(&key.0) {
                let ours = self.our_open(true).to_bytes().expect("in range");
                let f = self.frame(FrameType::Ctrl, key.0, key.1, 0, &ours, false);
                self.transmit(f, out);
            }
            let f = self.frame(FrameType::Ctrl, key.0, key.1, 0, &close.to_bytes(), false);
            self.transmit(f, out);
        }
        for key in due {
            if self.open_replies.remove(&key.0) {
                let ours = self.our_open(true).to_bytes().expect("in range");
                let f = self.frame(FrameType::Ctrl, key.0, key.1, 0, &ours, false);
                self.transmit(f, out);
            }
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
            let f = self.frame(FrameType::Ack, key.0, key.1, 0, &payload, false);
            self.transmit(f, out);
        }
    }

    fn expire(&mut self, now: Millis) {
        let (idle, ttl) = (self.cfg.idle_timeout, self.cfg.done_ttl);
        self.incoming
            .retain(|_, i| i.last_heard + if i.done { ttl } else { idle } > now);
        self.seen.retain(|_, until| *until > now);
    }

    /// While we wait for an ACK, other traffic on the channel means our over
    /// may not have gone out yet: the link waits for a clear channel before
    /// keying up, and we are not told when it does. Wait as if the over starts
    /// when that traffic ends. If it had gone out already, this costs at most
    /// one over's time, while the channel is busy anyway.
    fn hear_traffic(&mut self, now: Millis, h: &FrameHeader, payload: &[u8]) {
        let traffic_end = match (h.ftype, DataPreamble::decode(payload)) {
            (FrameType::Data, Ok((pre, symbol))) => {
                let remaining = pre.remaining.min(MAX_REMAINING_TRUSTED) as usize;
                now + self.cfg.air(remaining, self.cfg.data_frame_len(symbol.len()))
            }
            _ => now,
        };
        let ack_air = self.ack_air();
        let guard = self.cfg.ack_guard;
        let me = Dest::Station(self.cfg.me);
        for o in &mut self.active {
            if h.src == o.to && h.dst == me {
                continue; // our peer answering us
            }
            if let OutState::Waiting { until } = o.state {
                let later = traffic_end + guard + o.last_cost + guard + ack_air + guard;
                o.state = OutState::Waiting {
                    until: until.max(later),
                };
            }
        }
    }

    fn on_open(&mut self, now: Millis, from: Callsign, payload: &[u8]) {
        let Ok(open) = Open::decode(payload) else { return };
        self.peers.insert(from, (open, now));
        if !open.is_reply() && self.cfg.sessions {
            self.open_replies.insert(from);
        }
    }

    fn on_close(
        &mut self,
        now: Millis,
        from: Callsign,
        session: u16,
        payload: &[u8],
        out: &mut Vec<Output<Event>>,
    ) {
        let Ok(close) = Close::decode(payload) else { return };
        if close.reason == CloseReason::Done {
            // The sender is finished with us: forget its finished transfers.
            self.incoming
                .retain(|(c, s), i| !(*c == from && (session == 0 || *s == session) && i.done));
            return;
        }
        let Some(i) = self
            .active
            .iter()
            .position(|o| !o.broadcast && o.to == from && (session == 0 || session == o.session))
        else {
            return;
        };
        let o = &mut self.active[i];
        let reason = match close.reason {
            CloseReason::Busy => {
                // Not a failure: come back when asked, starting afresh.
                let wait = Millis::from_secs(u64::from(close.retry_after.max(1)));
                o.offer_next = true;
                o.timeouts = 0;
                o.state = OutState::Ready { at: now + wait };
                return;
            }
            // Believed only when the peer's own OPEN agrees; otherwise the
            // CLOSE answered a corrupted OFFER, so offer again.
            CloseReason::TooLarge
                if self
                    .peers
                    .get(&from)
                    .is_none_or(|(open, _)| o.len <= open.max_object) =>
            {
                o.offer_next = true;
                o.open_next = true;
                o.state = OutState::Ready { at: now };
                return;
            }
            CloseReason::TooLarge => Failure::TooLarge,
            _ => Failure::Refused,
        };
        let o = self.active.remove(i);
        out.push(Output::Event(Event::Failed {
            to: o.to,
            id: o.id,
            reason,
        }));
    }

    /// How long a broadcaster listens for repair requests after an over: the
    /// listeners' guard, the spread of their moments, one request, our slack.
    fn repair_wait(&self) -> Millis {
        let ack_air = self.ack_air();
        self.cfg.ack_guard + Millis(ack_air.0 * NACK_SPREAD_ACKS) + ack_air + self.cfg.ack_guard
    }

    /// Airtime of an OPEN frame.
    fn open_air(&self) -> Millis {
        self.cfg.air(1, HEADER_LEN + OPEN_LEN)
    }

    /// Airtime of an ACK with a receipt, key-up included.
    /// What an over costs besides its DATA frames: our key-up, the peer's
    /// key-up and ACK, the guard between (for a broadcast, the repair window
    /// listeners ask in), and the wait for the others on the channel to
    /// finish, whose overs are taken to be as long as a full one of ours.
    fn over_cost(&self, now: Millis, frame_air: Millis, cap: u32, broadcast: bool) -> OverCost {
        let ack = self.ack_air().0 as f64;
        let answer = if broadcast {
            (NACK_SPREAD_ACKS + 1) as f64 * ack
        } else {
            ack
        };
        let full_over = (self.cfg.txdelay.0 + frame_air.0 * u64::from(cap)) as f64;
        let wait = self.channel.access_wait(now.0 / 1_000, full_over);
        OverCost {
            frame_ms: frame_air.0 as f64,
            turnaround_ms: 2.0 * self.cfg.txdelay.0 as f64 + self.cfg.ack_guard.0 as f64 + answer + wait,
        }
    }

    /// Take the airtime of others' frames heard lately into the channel belief.
    fn note_occupancy(&mut self, now: Millis) {
        if now < self.busy_since + OCCUPANCY_EVERY {
            return;
        }
        self.channel.observe(
            now.0 / 1_000,
            ChannelObservation::Occupancy {
                busy_ms: self.busy_ms,
                total_ms: now.0 - self.busy_since.0,
            },
        );
        self.busy_ms = 0;
        self.busy_since = now;
    }

    /// Take an ACK's count into the loss belief and report it.
    fn observe_over(&mut self, to: Callsign, sent: u32, got: u32, out: &mut Vec<Output<Event>>) {
        let mut erasure = self.erasure(to);
        erasure.observe(sent, got);
        self.erasure.insert(to, erasure);
        out.push(Output::Event(Event::Over { to, sent, got }));
    }

    fn ack_air(&self) -> Millis {
        self.cfg.txdelay + self.cfg.air(1, HEADER_LEN + 7 + 8 + hm_wire::RECEIPT_LEN)
    }

    fn on_frame(&mut self, now: Millis, data: &[u8], out: &mut Vec<Output<Event>>) {
        let Ok((h, payload)) = FrameHeader::decode(data) else {
            return;
        };
        if h.src == self.cfg.me {
            return;
        }
        self.busy_ms += self.cfg.air(1, data.len()).0;
        self.hear_traffic(now, &h, payload);
        let for_me = h.dst == Dest::Station(self.cfg.me);
        let broadcast = h.dst == Dest::Broadcast;
        if let (FrameType::Ack, Dest::Station(to)) = (h.ftype, h.dst) {
            if !for_me {
                self.overhear_nack(to, h.session, payload);
            }
        }
        if !for_me && !broadcast {
            return;
        }
        match h.ftype {
            FrameType::Data if for_me || broadcast => {
                self.on_data(now, h.src, h.session, h.index, payload, broadcast, out)
            }
            FrameType::Ctrl if payload.first() == Some(&CTRL_OFFER) && (for_me || broadcast) => {
                self.on_offer(now, h.src, h.session, payload, broadcast, out)
            }
            FrameType::Ctrl if payload.first() == Some(&CTRL_OPEN) && for_me => {
                self.on_open(now, h.src, payload)
            }
            FrameType::Ctrl if payload.first() == Some(&CTRL_CLOSE) && for_me => {
                self.on_close(now, h.src, h.session, payload, out)
            }
            FrameType::Ack if for_me => self.on_ack(now, h.src, h.session, payload, out),
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
            }) => self.enqueue(now, to, object, precedence, false, out),
            Input::Command(Command::Broadcast { object, precedence }) => {
                self.enqueue(now, broadcast_peer(), object, precedence, true, out)
            }
            Input::Command(Command::Accept {
                from,
                id,
                accepted,
                retry_after,
            }) => self.application_verdict(now, from, id, accepted, retry_after),
            Input::Command(Command::Belief { peer, erasure }) => {
                self.erasure.insert(peer, erasure);
            }
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

    fn transmitted(&mut self, now: Millis, port: Port) {
        if port == self.cfg.port {
            self.on_transmitted(now);
        }
    }

    fn next_deadline(&self) -> Option<Millis> {
        let mut d: Option<Millis> = None;
        let mut take = |t: Millis| d = Some(d.map_or(t, |x| x.min(t)));
        let receiving = self.receiving();
        if let Some(t) = self.answer_at() {
            take(t);
        }
        if let Some(t) = self.nack_due() {
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
        let waiting = self.waiting();
        for o in &self.active {
            match o.state {
                OutState::Waiting { until } => take(until),
                // While a peer's over, or the answer to ours, is under way,
                // those deadlines wake us instead.
                OutState::Ready { at } if !receiving && !waiting => take(at),
                OutState::Ready { .. } => {}
            }
        }
        d
    }
}

#[cfg(test)]
mod tests;
