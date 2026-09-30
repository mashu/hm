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
use hm_model::{Bearer, Erasure, LinkPrior, Prior, QUIET_BUSY};
use hm_wire::{
    Callsign, Close, Dest, FrameHeader, FrameType, ObjectId, Open, CTRL_CLOSE, CTRL_OFFER, CTRL_OPEN,
};

mod airtime;
mod answer;
pub mod beacon;
mod command;
mod config;
mod over;
mod receipt;
mod receive;
mod send;
mod session;
mod symbols;

pub use command::{Command, Event, Failure, PeerBelief, Receipt};
pub use config::Config;
pub use receipt::{object_id, receipt_statement, HASH_CONTEXT, RECEIPT_PREFIX};
pub use symbols::fit_symbol;

use receive::Incoming;
use send::{OutState, Outgoing, Pending};

/// RaptorQ symbol alignment; symbol sizes must be multiples of it.
pub const SYMBOL_ALIGNMENT: u16 = 8;
/// Most source symbols per object, bounding decoder memory and time.
pub const MAX_SOURCE_SYMBOLS: u32 = 8192;
/// Fade dispersion assumed at a broadcast's unknown listeners.
const LOSS_PRIOR_DISPERSION: f64 = 0.1;
/// Listeners a broadcast is sized for until the station says how many share
/// the channel.
const DEFAULT_LISTENERS: u32 = 4;
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
pub const SLOW_FRAME_SECS: u64 = 2;
/// Smallest symbol a slow link is given.
const MIN_SLOW_SYMBOL: usize = 32;
/// Longest over on a link below 1200 bit/s.
pub const SLOW_MAX_OVER: Millis = Millis::from_secs(60);

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
    beliefs: BTreeMap<Callsign, PeerBelief>,
    /// Congestion window towards each peer: most symbols in one over. Halved
    /// when an ACK does not come, grown by one with each ACK that does.
    window: BTreeMap<Callsign, u32>,
    /// Share of the time other stations keep the channel busy, as the
    /// station believes it: every over we add waits for the channel to clear.
    channel_busy: f64,
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
            beliefs: BTreeMap::new(),
            window: BTreeMap::new(),
            channel_busy: QUIET_BUSY,
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
    /// the belief the station gave for them, or a prior of 30 % loss.
    pub fn erasure(&self, peer: Callsign) -> Erasure {
        self.beliefs.get(&peer).map(|b| b.erasure).unwrap_or_else(|| {
            if peer == broadcast_peer() {
                Erasure::from_prior(BROADCAST_LOSS, LOSS_PRIOR_DISPERSION)
            } else {
                let prior = LinkPrior::for_bearer(Bearer::Radio);
                Erasure::from_prior(prior.erasure, prior.dispersion)
            }
        })
    }

    /// What the station told us about the link towards `peer`, with frame
    /// loss as ACKs have said since; for a peer it told us nothing about, a
    /// link taken to be open.
    fn belief(&self, peer: Callsign) -> PeerBelief {
        self.beliefs
            .get(&peer)
            .copied()
            .unwrap_or_else(|| PeerBelief::open(self.erasure(peer)))
    }

    /// Airtime the station spent at `now` outside transfers (beacons, control
    /// frames): it counts against the duty cycle like an over, so an over
    /// cannot follow it straight into a key-up longer than the bucket.
    pub fn spend(&mut self, now: Millis, airtime: Millis) {
        self.charge(now, airtime);
    }

    /// What the station believes about the channel: the share of the time
    /// others keep it busy, and how many stations share it (whom a broadcast
    /// is sized for).
    pub fn set_channel(&mut self, busy: f64, listeners: u32) {
        self.channel_busy = busy.clamp(0.0, 1.0);
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

    fn on_frame(&mut self, now: Millis, data: &[u8], out: &mut Vec<Output<Event>>) {
        let Ok((h, payload)) = FrameHeader::decode(data) else {
            return;
        };
        if h.src == self.cfg.me {
            return;
        }
        self.hear_traffic(now, &h, payload);
        if h.dst == Dest::Station(self.cfg.me) {
            // Anything from a peer we are sending to shows the link open.
            for o in self.active.iter_mut().filter(|o| o.to == h.src) {
                o.open = o.open.heard();
                o.open_at = now;
            }
        }
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
            Input::Command(Command::Belief { peer, belief }) => {
                self.beliefs.insert(peer, belief);
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
