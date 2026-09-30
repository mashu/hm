//! Deterministic discrete-event simulator.
//!
//! Simulated stations run the real [`hm_core::Machine`] implementations. The
//! simulator models the physics of one or more radio channels:
//!
//! - **Channels and ports**: each channel has its own bitrate and TXDELAY. A
//!   station attaches radios (ports) to channels, e.g. port 0 on the VHF access
//!   channel and port 1 on an HF backbone channel. Channels never interfere.
//! - **Airtime**: `txdelay + txtail + ((frame + phy overhead) * 8 + stuffed bits) / bitrate`,
//!   rounded up to 1 ms; stuffed bits are counted on HDLC channels only.
//!   Each radio sends its queued frames back to back; a frame queued while the
//!   radio is still transmitting follows without a new TXDELAY (PTT stays keyed).
//! - **Half-duplex**: a radio that is transmitting hears nothing on its channel.
//! - **Collisions**: two transmissions overlapping at a receiver destroy each
//!   other (no capture effect), including hidden-terminal cases.
//! - **Loss**: per directed link: Bernoulli, Gilbert–Elliott (bursty), a
//!   per-UTC-hour table for HF band openings (sim time 0 = 00:00 UTC), a
//!   real modem's measured loss by SNR and frame length ([`Loss::afsk_1200`]),
//!   or that modem under flat fading, as on an HF path ([`Loss::Fading`]).
//! - **Faults**: stations going down and up, links cut and restored
//!   (partitions), per-station clock offset and drift, and frames delivered
//!   with undetected bit errors.
//! - **Channel access**: off by default (a radio keys up when its machine asks,
//!   so the simulator measures whatever MAC the protocol implements), or per
//!   radio p-persistent CSMA on carrier detect ([`Csma`]), as the link below the
//!   machine does it on air. Carrier is detected after a delay, and only from
//!   stations the radio can hear, so hidden terminals still collide.
//!
//! Same seed and same inputs give a byte-identical run; [`Report::trace`]
//! hashes every delivery, fault and application event.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::hash::{Hash, Hasher};

use hm_core::{DetRng, Input, Machine, Millis, Output, Port};

pub mod afsk_1200;
pub mod curve;
pub mod metrics;
pub mod routing;
pub mod toy;

mod air;
mod fading;
mod log;
mod radio;
mod setup;
mod stats;

pub use curve::LossCurve;
use fading::Fade;
pub use log::{LogEntry, Outcome};
pub use radio::{Clock, Csma, Loss, RadioParams};
pub use stats::{classify_hm_frame, Airtime, FrameClass, NodeStats, Report, Stats};

pub type NodeId = usize;

/// A radio channel (one frequency and mode).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub usize);

struct Radio {
    channel: ChannelId,
    free_at: Millis,
    /// Start of the current or last key-up.
    keyed_at: Millis,
    csma: Option<Csma>,
    /// Frames waiting for the channel, with when the machine asked to send them.
    waiting: Vec<(Vec<u8>, Millis)>,
    /// Bumped to cancel a scheduled channel-access attempt.
    access_gen: u64,
    access_pending: bool,
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
    /// Start of the key-up this frame belongs to: its carrier began then.
    keyed_at: Millis,
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
    Access {
        node: NodeId,
        port: Port,
        gen: u64,
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
    /// Fading state of each path with [`Loss::Fading`], by channel and the
    /// two stations in ascending order: both directions share it.
    fades: BTreeMap<(ChannelId, NodeId, NodeId), Fade>,
    queue: BinaryHeap<Reverse<Scheduled<C>>>,
    seq: u64,
    rng: DetRng,
    seed_rng: DetRng,
    txs: Vec<Tx>,
    max_airtime: Millis,
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
            fades: BTreeMap::new(),
            queue: BinaryHeap::new(),
            seq: 0,
            rng: root.fork(0),
            seed_rng: root.fork(1),
            txs: Vec::new(),
            max_airtime: Millis::ZERO,
            trace: Fnv::new(),
            events: Vec::new(),
            log: None,
        }
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
                    for r in n.radios.values_mut() {
                        r.waiting.clear();
                        r.access_pending = false;
                        r.access_gen += 1;
                    }
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
            Ev::Access { node, port, gen } => self.access(node, port, gen),
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

    /// Tell `node` when its radio on `ch` has sent everything it was asked
    /// to: the frame that just ended was the last of its key-up and nothing
    /// waits for the channel.
    fn drained(&mut self, node: NodeId, ch: ChannelId, end: Millis) {
        let n = &mut self.nodes[node];
        let Some(port) = n.port_on(ch) else { return };
        let r = &n.radios[&port];
        if !n.up || r.free_at > end || !r.waiting.is_empty() {
            return;
        }
        let local = n.clock.local(end);
        n.machine.transmitted(local, port);
        self.reschedule_timer(node);
    }
}

#[cfg(test)]
mod tests;
