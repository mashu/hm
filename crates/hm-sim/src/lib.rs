//! Deterministic discrete-event simulator.
//!
//! Simulated stations run the real [`hm_core::Machine`] implementations. The
//! simulator models the physics of one or more radio channels:
//!
//! - **Channels and ports**: each channel has its own bitrate and TXDELAY. A
//!   station attaches radios (ports) to channels, e.g. port 0 on the VHF access
//!   channel and port 1 on an HF backbone channel. Channels never interfere.
//! - **Airtime**: `txdelay + (frame + phy overhead) * 8 / bitrate`, rounded up to 1 ms.
//!   Each radio sends its queued frames back to back; a frame queued while the
//!   radio is still transmitting follows without a new TXDELAY (PTT stays keyed).
//! - **Half-duplex**: a radio that is transmitting hears nothing on its channel.
//! - **Collisions**: two transmissions overlapping at a receiver destroy each
//!   other (no capture effect), including hidden-terminal cases.
//! - **Loss**: per directed link: Bernoulli, Gilbert–Elliott (bursty) or a
//!   per-UTC-hour table for HF band openings (sim time 0 = 00:00 UTC).
//! - **Faults**: stations going down and up, links cut and restored
//!   (partitions), per-station clock offset and drift, and frames delivered
//!   with undetected bit errors.
//!
//! Channel access (CSMA) is *not* modelled: it is the machine's job, so the
//! simulator measures whatever MAC the protocol implements.
//!
//! Same seed and same inputs give a byte-identical run; [`Report::trace`]
//! hashes every delivery, fault and application event.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::hash::{Hash, Hasher};

use hm_core::{DetRng, Input, Machine, Millis, Output, Port};
use hm_wire::{FrameHeader, FrameType, DATA_PREAMBLE_LEN, HEADER_LEN};

pub mod metrics;
pub mod toy;

pub type NodeId = usize;

/// A radio channel (one frequency and mode).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub usize);

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
}

/// Physical parameters of a channel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RadioParams {
    pub bitrate_bps: u32,
    /// Key-up delay before data (TXDELAY), counted as airtime.
    pub txdelay: Millis,
    /// Extra bytes the modem adds per frame (preamble, sync word, FEC parity).
    pub phy_overhead_bytes: u32,
}

impl RadioParams {
    /// AFSK 1200 on an FM transceiver with a typical 300 ms TXDELAY.
    pub const VHF_1200: RadioParams = RadioParams {
        bitrate_bps: 1200,
        txdelay: Millis(300),
        phy_overhead_bytes: 0,
    };

    /// Airtime of a frame that keys up the transmitter.
    pub fn airtime(&self, frame_len: usize) -> Millis {
        self.txdelay + self.airtime_keyed(frame_len)
    }

    /// Airtime of a frame sent while the transmitter is already keyed.
    pub fn airtime_keyed(&self, frame_len: usize) -> Millis {
        let bits = (frame_len as u64 + self.phy_overhead_bytes as u64) * 8;
        Millis((bits * 1000).div_ceil(self.bitrate_bps.max(1) as u64))
    }

    fn bits_us(&self, bits: u64) -> u64 {
        bits * 1_000_000 / self.bitrate_bps.max(1) as u64
    }
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

/// Returns the number of header bytes and the class of a frame.
pub type Classifier = fn(&[u8]) -> (usize, FrameClass);

/// Default classifier: hm-wire frames. DATA symbols are payload, every other
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
    pub txdelay_us: u64,
    /// Modem overhead bytes plus frame headers.
    pub overhead_us: u64,
    pub payload_us: u64,
    pub control_us: u64,
    pub unknown_us: u64,
}

impl Airtime {
    pub fn total_us(&self) -> u64 {
        self.txdelay_us + self.overhead_us + self.payload_us + self.control_us + self.unknown_us
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

    fn add(&mut self, o: &Airtime) {
        self.txdelay_us += o.txdelay_us;
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
    fn add(&mut self, o: &Stats) {
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

/// What happened to one transmission at one receiver.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    Delivered,
    Corrupted,
    LostChannel,
    LostCollision,
    LostHalfDuplex,
    LostDown,
}

/// Optional detailed log, in processing order, for independent checking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogEntry {
    /// `queued_at` is when the machine asked to transmit; `keyup` is false when
    /// the frame followed the previous one without a new TXDELAY.
    Tx {
        id: u64,
        channel: ChannelId,
        from: NodeId,
        port: Port,
        queued_at: Millis,
        start: Millis,
        end: Millis,
        keyup: bool,
        len: usize,
        digest: u64,
    },
    /// Transmission `tx` ended and is being evaluated at every listening neighbour;
    /// its `Rx` entries follow immediately.
    Eval {
        tx: u64,
        at: Millis,
    },
    Rx {
        tx: u64,
        channel: ChannelId,
        to: NodeId,
        port: Port,
        at: Millis,
        outcome: Outcome,
        digest: u64,
    },
    Up {
        node: NodeId,
        at: Millis,
        up: bool,
    },
    Link {
        channel: ChannelId,
        from: NodeId,
        to: NodeId,
        at: Millis,
        enabled: bool,
    },
}

struct Radio {
    channel: ChannelId,
    free_at: Millis,
}

struct Node<M> {
    machine: M,
    up: bool,
    radios: BTreeMap<Port, Radio>,
    clock: Clock,
    /// Local deadline currently scheduled in the queue.
    timer: Option<Millis>,
    gen: u64,
    same_time_firings: u32,
    last_firing: Millis,
    stats: NodeStats,
}

impl<M> Node<M> {
    fn port_on(&self, ch: ChannelId) -> Option<Port> {
        self.radios.iter().find(|(_, r)| r.channel == ch).map(|(p, _)| *p)
    }
}

struct Link {
    loss: Loss,
    bad: bool,
    enabled: bool,
    corrupt: f64,
}

struct Tx {
    id: u64,
    channel: ChannelId,
    from: NodeId,
    start: Millis,
    end: Millis,
}

enum Ev<C> {
    Timer {
        node: NodeId,
        gen: u64,
    },
    TxEnd {
        id: u64,
        channel: ChannelId,
        from: NodeId,
        start: Millis,
        frame: Vec<u8>,
    },
    Command {
        node: NodeId,
        cmd: C,
    },
    SetUp {
        node: NodeId,
        up: bool,
    },
    SetLink {
        channel: ChannelId,
        from: NodeId,
        to: NodeId,
        enabled: bool,
    },
}

struct Scheduled<C> {
    at: Millis,
    seq: u64,
    ev: Ev<C>,
}

impl<C> PartialEq for Scheduled<C> {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl<C> Eq for Scheduled<C> {}
impl<C> PartialOrd for Scheduled<C> {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl<C> Ord for Scheduled<C> {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.at, self.seq).cmp(&(o.at, o.seq))
    }
}

/// FNV-1a. Stable across Rust versions, unlike `DefaultHasher`.
pub struct Fnv(pub u64);

impl Fnv {
    pub fn new() -> Fnv {
        Fnv(0xCBF2_9CE4_8422_2325)
    }

    pub fn digest(bytes: &[u8]) -> u64 {
        let mut h = Fnv::new();
        h.write(bytes);
        h.finish()
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Fnv::new()
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

/// Guard against machines that never advance their deadline.
const MAX_SAME_TIME_FIRINGS: u32 = 10_000;

/// Simulated channels with stations of machine type `M`.
pub struct Sim<M, C, E>
where
    M: Machine<Input = Input<C>, Output = Output<E>>,
{
    now: Millis,
    channels: Vec<RadioParams>,
    channel_stats: Vec<Stats>,
    nodes: Vec<Node<M>>,
    links: BTreeMap<(ChannelId, NodeId, NodeId), Link>,
    queue: BinaryHeap<Reverse<Scheduled<C>>>,
    seq: u64,
    rng: DetRng,
    seed_rng: DetRng,
    txs: Vec<Tx>,
    max_airtime: Millis,
    classifier: Classifier,
    trace: Fnv,
    events: Vec<(Millis, NodeId, E)>,
    log: Option<Vec<LogEntry>>,
}

impl<M, C, E> Sim<M, C, E>
where
    M: Machine<Input = Input<C>, Output = Output<E>>,
    E: Hash,
{
    /// A simulator with one channel, `ChannelId(0)`, using `radio`.
    pub fn new(seed: u64, radio: RadioParams) -> Self {
        let root = DetRng::from_seed(seed);
        Sim {
            now: Millis::ZERO,
            channels: vec![radio],
            channel_stats: vec![Stats::default()],
            nodes: Vec::new(),
            links: BTreeMap::new(),
            queue: BinaryHeap::new(),
            seq: 0,
            rng: root.fork(0),
            seed_rng: root.fork(1),
            txs: Vec::new(),
            max_airtime: Millis::ZERO,
            classifier: classify_hm_frame,
            trace: Fnv::new(),
            events: Vec::new(),
            log: None,
        }
    }

    pub fn add_channel(&mut self, radio: RadioParams) -> ChannelId {
        self.channels.push(radio);
        self.channel_stats.push(Stats::default());
        ChannelId(self.channels.len() - 1)
    }

    pub fn channel_params(&self, ch: ChannelId) -> RadioParams {
        self.channels[ch.0]
    }

    /// Replace the frame classifier used for the airtime split.
    pub fn set_classifier(&mut self, c: Classifier) {
        self.classifier = c;
    }

    /// Record a [`LogEntry`] for every transmission, reception and fault.
    pub fn enable_log(&mut self) {
        self.log.get_or_insert_with(Vec::new);
    }

    pub fn log(&self) -> &[LogEntry] {
        self.log.as_deref().unwrap_or(&[])
    }

    /// A deterministic RNG for building machine `stream` (use the node index).
    pub fn machine_rng(&self, stream: u64) -> DetRng {
        self.seed_rng.fork(stream)
    }

    /// Add a station with port 0 attached to channel 0.
    pub fn add_node(&mut self, machine: M) -> NodeId {
        self.add_node_on(machine, &[(0, ChannelId(0))])
    }

    /// Add a station with the given `(port, channel)` radios.
    pub fn add_node_on(&mut self, machine: M, radios: &[(Port, ChannelId)]) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            machine,
            up: true,
            radios: BTreeMap::new(),
            clock: Clock::default(),
            timer: None,
            gen: 0,
            same_time_firings: 0,
            last_firing: Millis::ZERO,
            stats: NodeStats::default(),
        });
        for &(port, ch) in radios {
            self.attach(id, port, ch);
        }
        self.reschedule_timer(id);
        id
    }

    /// Attach a radio on `port` of `node` to channel `ch`.
    pub fn attach(&mut self, node: NodeId, port: Port, ch: ChannelId) {
        assert!(ch.0 < self.channels.len(), "no channel {ch:?}");
        let n = &mut self.nodes[node];
        assert!(
            n.port_on(ch).is_none(),
            "station {node} already has a radio on {ch:?}"
        );
        assert!(
            !n.radios.contains_key(&port),
            "station {node} port {port} already attached"
        );
        n.radios.insert(
            port,
            Radio {
                channel: ch,
                free_at: Millis::ZERO,
            },
        );
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn set_clock(&mut self, node: NodeId, clock: Clock) {
        assert!(clock.ppm.abs() < 100_000, "clock drift must be below 100,000 ppm");
        let n = &mut self.nodes[node];
        n.clock = clock;
        n.timer = None;
        n.gen += 1;
        self.reschedule_timer(node);
    }

    /// `a` and `b` hear each other on channel 0.
    pub fn link(&mut self, a: NodeId, b: NodeId, loss: Loss) {
        self.link_on(ChannelId(0), a, b, loss);
    }

    /// `to` hears `from` on channel 0.
    pub fn link_one_way(&mut self, from: NodeId, to: NodeId, loss: Loss) {
        self.link_one_way_on(ChannelId(0), from, to, loss);
    }

    pub fn link_on(&mut self, ch: ChannelId, a: NodeId, b: NodeId, loss: Loss) {
        self.link_one_way_on(ch, a, b, loss);
        self.link_one_way_on(ch, b, a, loss);
    }

    pub fn link_one_way_on(&mut self, ch: ChannelId, from: NodeId, to: NodeId, loss: Loss) {
        assert!(from != to, "a station cannot link to itself");
        assert!(
            self.nodes[from].port_on(ch).is_some() && self.nodes[to].port_on(ch).is_some(),
            "attach both stations to {ch:?} before linking them"
        );
        self.links.insert(
            (ch, from, to),
            Link {
                loss,
                bad: false,
                enabled: true,
                corrupt: 0.0,
            },
        );
    }

    /// Frames `to` receives from `from` on `ch` carry 1–3 flipped bits with probability `p`.
    pub fn set_corruption(&mut self, ch: ChannelId, from: NodeId, to: NodeId, p: f64) {
        self.links
            .get_mut(&(ch, from, to))
            .expect("link must exist")
            .corrupt = p;
    }

    /// Enable or disable the link between `a` and `b` (both directions) at time `at`.
    pub fn set_link_at(&mut self, at: Millis, ch: ChannelId, a: NodeId, b: NodeId, enabled: bool) {
        for (from, to) in [(a, b), (b, a)] {
            if self.links.contains_key(&(ch, from, to)) {
                self.push(
                    at,
                    Ev::SetLink {
                        channel: ch,
                        from,
                        to,
                        enabled,
                    },
                );
            }
        }
    }

    /// Cut every link on `ch` between the two groups at time `at`.
    pub fn partition_at(&mut self, at: Millis, ch: ChannelId, side_a: &[NodeId], side_b: &[NodeId]) {
        for &a in side_a {
            for &b in side_b {
                self.set_link_at(at, ch, a, b, false);
            }
        }
    }

    /// Restore every link on `ch` between the two groups at time `at`.
    pub fn heal_at(&mut self, at: Millis, ch: ChannelId, side_a: &[NodeId], side_b: &[NodeId]) {
        for &a in side_a {
            for &b in side_b {
                self.set_link_at(at, ch, a, b, true);
            }
        }
    }

    pub fn command_at(&mut self, at: Millis, node: NodeId, cmd: C) {
        self.push(at, Ev::Command { node, cmd });
    }

    pub fn set_up_at(&mut self, at: Millis, node: NodeId, up: bool) {
        self.push(at, Ev::SetUp { node, up });
    }

    pub fn now(&self) -> Millis {
        self.now
    }

    pub fn node(&self, id: NodeId) -> &M {
        &self.nodes[id].machine
    }

    pub fn machines(&self) -> impl Iterator<Item = &M> {
        self.nodes.iter().map(|n| &n.machine)
    }

    /// Application events emitted so far: `(global time, station, event)`.
    pub fn events(&self) -> &[(Millis, NodeId, E)] {
        &self.events
    }

    pub fn report(&self) -> Report {
        Report {
            now: self.now,
            channels: self.channel_stats.clone(),
            nodes: self.nodes.iter().map(|n| n.stats.clone()).collect(),
            trace: self.trace.finish(),
        }
    }

    /// Process every event up to and including `until`.
    pub fn run_until(&mut self, until: Millis) {
        while let Some(Reverse(top)) = self.queue.peek() {
            if top.at > until {
                break;
            }
            let Reverse(s) = self.queue.pop().expect("peeked");
            self.now = s.at;
            self.dispatch(s.ev);
        }
        self.now = self.now.max(until);
    }

    fn push(&mut self, at: Millis, ev: Ev<C>) {
        self.seq += 1;
        self.queue.push(Reverse(Scheduled {
            at,
            seq: self.seq,
            ev,
        }));
    }

    fn record(&mut self, e: LogEntry) {
        if let Some(log) = self.log.as_mut() {
            log.push(e);
        }
    }

    fn dispatch(&mut self, ev: Ev<C>) {
        match ev {
            Ev::Timer { node, gen } => self.fire_timer(node, gen),
            Ev::Command { node, cmd } => {
                if self.nodes[node].up {
                    let local = self.nodes[node].clock.local(self.now);
                    let mut out = Vec::new();
                    self.nodes[node]
                        .machine
                        .handle(local, Input::Command(cmd), &mut out);
                    self.apply(node, out);
                }
            }
            Ev::SetUp { node, up } => {
                self.nodes[node].up = up;
                (self.now, node, up as u8, 0xFAu8).hash(&mut self.trace);
                self.record(LogEntry::Up {
                    node,
                    at: self.now,
                    up,
                });
                if up {
                    self.reschedule_timer(node);
                } else {
                    let n = &mut self.nodes[node];
                    n.gen += 1;
                    n.timer = None;
                }
            }
            Ev::SetLink {
                channel,
                from,
                to,
                enabled,
            } => {
                if let Some(l) = self.links.get_mut(&(channel, from, to)) {
                    l.enabled = enabled;
                }
                (self.now, channel.0, from, to, enabled as u8, 0xFBu8).hash(&mut self.trace);
                self.record(LogEntry::Link {
                    channel,
                    from,
                    to,
                    at: self.now,
                    enabled,
                });
            }
            Ev::TxEnd {
                id,
                channel,
                from,
                start,
                frame,
            } => self.finish_tx(id, channel, from, start, frame),
        }
    }

    fn fire_timer(&mut self, node: NodeId, gen: u64) {
        let now = self.now;
        let n = &mut self.nodes[node];
        if !n.up || n.gen != gen {
            return; // stale timer
        }
        n.timer = None;
        let local = n.clock.local(now);
        match n.machine.next_deadline() {
            Some(d) if d <= local => {}
            _ => {
                self.reschedule_timer(node);
                return;
            }
        }
        if n.last_firing == now {
            n.same_time_firings += 1;
            assert!(
                n.same_time_firings < MAX_SAME_TIME_FIRINGS,
                "station {node} keeps asking for a deadline at {now:?} without advancing it"
            );
        } else {
            n.last_firing = now;
            n.same_time_firings = 0;
        }
        let mut out = Vec::new();
        n.machine.on_deadline(local, &mut out);
        self.apply(node, out);
    }

    fn apply(&mut self, node: NodeId, out: Vec<Output<E>>) {
        for o in out {
            match o {
                Output::Transmit { port, data } => self.start_tx(node, port, data),
                Output::Event(e) => {
                    (self.now, node, 0xEEu8).hash(&mut self.trace);
                    e.hash(&mut self.trace);
                    self.events.push((self.now, node, e));
                }
            }
        }
        self.reschedule_timer(node);
    }

    fn reschedule_timer(&mut self, node: NodeId) {
        let now = self.now;
        let n = &mut self.nodes[node];
        if !n.up {
            return;
        }
        let want = n.machine.next_deadline();
        if want == n.timer {
            return;
        }
        n.gen += 1;
        n.timer = want;
        if let Some(d) = want {
            let gen = n.gen;
            let at = n.clock.global_for(d).max(now);
            self.push(at, Ev::Timer { node, gen });
        }
    }

    fn start_tx(&mut self, node: NodeId, port: Port, frame: Vec<u8>) {
        if !self.nodes[node].up {
            return;
        }
        let ch = match self.nodes[node].radios.get(&port) {
            Some(r) => r.channel,
            None => panic!("station {node} transmitted on port {port}, which has no radio"),
        };
        let radio = self.channels[ch.0];
        let keyup = self.nodes[node].radios[&port].free_at <= self.now;
        let airtime = if keyup {
            radio.airtime(frame.len())
        } else {
            radio.airtime_keyed(frame.len())
        };
        let (header, class) = (self.classifier)(&frame);
        let header = header.min(frame.len());
        let body_bits = (frame.len() - header) as u64 * 8;
        let split = Airtime {
            txdelay_us: if keyup { radio.txdelay.0 * 1000 } else { 0 },
            overhead_us: radio.bits_us((radio.phy_overhead_bytes as u64 + header as u64) * 8),
            payload_us: if class == FrameClass::Payload {
                radio.bits_us(body_bits)
            } else {
                0
            },
            control_us: if class == FrameClass::Control {
                radio.bits_us(body_bits)
            } else {
                0
            },
            unknown_us: if class == FrameClass::Unknown {
                radio.bits_us(body_bits)
            } else {
                0
            },
        };

        let r = self.nodes[node].radios.get_mut(&port).expect("checked above");
        let start = if keyup { self.now } else { r.free_at };
        let end = start + airtime;
        r.free_at = end;
        let n = &mut self.nodes[node];
        n.stats.frames_sent += 1;
        n.stats.bytes_sent += frame.len() as u64;
        n.stats.airtime_ms += airtime.0;
        let s = &mut self.channel_stats[ch.0];
        s.frames_sent += 1;
        s.bytes_sent += frame.len() as u64;
        s.airtime_ms += airtime.0;
        s.airtime.add(&split);
        self.max_airtime = self.max_airtime.max(airtime);
        self.seq += 1;
        let id = self.seq;
        self.txs.push(Tx {
            id,
            channel: ch,
            from: node,
            start,
            end,
        });
        let digest = Fnv::digest(&frame);
        let queued_at = self.now;
        self.record(LogEntry::Tx {
            id,
            channel: ch,
            from: node,
            port,
            queued_at,
            start,
            end,
            keyup,
            len: frame.len(),
            digest,
        });
        self.push(
            end,
            Ev::TxEnd {
                id,
                channel: ch,
                from: node,
                start,
                frame,
            },
        );
    }

    fn finish_tx(&mut self, id: u64, ch: ChannelId, from: NodeId, start: Millis, frame: Vec<u8>) {
        let end = self.now;
        let overlaps = |t: &Tx| t.channel == ch && t.start < end && start < t.end;
        let receivers: Vec<NodeId> = self
            .links
            .range((ch, from, 0)..=(ch, from, usize::MAX))
            .filter(|(_, l)| l.enabled)
            .map(|(&(_, _, to), _)| to)
            .collect();
        let original = Fnv::digest(&frame);
        self.record(LogEntry::Eval { tx: id, at: end });
        for r in receivers {
            let port = self.nodes[r]
                .port_on(ch)
                .expect("linked stations have a radio on the channel");
            let outcome = if !self.nodes[r].up {
                Outcome::LostDown
            } else if self.txs.iter().any(|t| t.from == r && overlaps(t)) {
                Outcome::LostHalfDuplex
            } else if self.txs.iter().any(|t| {
                t.id != id
                    && t.from != from
                    && overlaps(t)
                    && self.links.get(&(ch, t.from, r)).is_some_and(|l| l.enabled)
            }) {
                Outcome::LostCollision
            } else {
                let now = self.now;
                let link = self
                    .links
                    .get_mut(&(ch, from, r))
                    .expect("receiver comes from link table");
                if lose(&mut self.rng, link, now) {
                    Outcome::LostChannel
                } else if !frame.is_empty() && self.rng.chance(link.corrupt) {
                    Outcome::Corrupted
                } else {
                    Outcome::Delivered
                }
            };
            let mut data = frame.clone();
            if outcome == Outcome::Corrupted {
                // 1-3 distinct bits, so the frame always differs from what was sent.
                let bits = data.len() as u64 * 8;
                let flips = (1 + self.rng.below(3)).min(bits);
                let mut chosen: Vec<u64> = Vec::with_capacity(flips as usize);
                while (chosen.len() as u64) < flips {
                    let bit = self.rng.below(bits);
                    if !chosen.contains(&bit) {
                        chosen.push(bit);
                    }
                }
                for bit in chosen {
                    data[(bit / 8) as usize] ^= 1 << (bit % 8);
                }
            }
            let digest = if matches!(outcome, Outcome::Delivered | Outcome::Corrupted) {
                Fnv::digest(&data)
            } else {
                original
            };
            self.record(LogEntry::Rx {
                tx: id,
                channel: ch,
                to: r,
                port,
                at: self.now,
                outcome,
                digest,
            });
            let s = &mut self.channel_stats[ch.0];
            match outcome {
                Outcome::Delivered => s.delivered += 1,
                Outcome::Corrupted => s.corrupted += 1,
                Outcome::LostChannel => s.lost_channel += 1,
                Outcome::LostCollision => s.lost_collision += 1,
                Outcome::LostHalfDuplex => s.lost_half_duplex += 1,
                Outcome::LostDown => s.lost_down += 1,
            }
            if matches!(outcome, Outcome::Delivered | Outcome::Corrupted) {
                self.nodes[r].stats.frames_received += 1;
                (self.now, ch.0, from, r, port, 0xD1u8).hash(&mut self.trace);
                data.hash(&mut self.trace);
                let local = self.nodes[r].clock.local(self.now);
                let mut out = Vec::new();
                self.nodes[r]
                    .machine
                    .handle(local, Input::Frame { port, data }, &mut out);
                self.apply(r, out);
            }
        }
        let horizon = self.max_airtime;
        let now = self.now;
        self.txs.retain(|t| t.end + horizon > now);
    }
}

fn lose(rng: &mut DetRng, link: &mut Link, now: Millis) -> bool {
    match link.loss {
        Loss::None => false,
        Loss::Bernoulli(p) => rng.chance(p),
        Loss::GilbertElliott {
            p_good_to_bad,
            p_bad_to_good,
            loss_good,
            loss_bad,
        } => {
            let flip = if link.bad { p_bad_to_good } else { p_good_to_bad };
            if rng.chance(flip) {
                link.bad = !link.bad;
            }
            rng.chance(if link.bad { loss_bad } else { loss_good })
        }
        Loss::Hourly(table) => rng.chance(table[((now.0 / 3_600_000) % 24) as usize]),
    }
}

#[cfg(test)]
mod tests;
