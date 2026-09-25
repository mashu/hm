//! Deterministic discrete-event simulator.
//!
//! Simulated stations run the real [`hm_core::Machine`] implementations. The
//! simulator models the physics of one shared radio channel:
//!
//! - **Airtime**: `txdelay + (frame + phy overhead) * 8 / bitrate`, rounded up to 1 ms.
//! - **Half-duplex**: a station that is transmitting hears nothing.
//! - **Collisions**: two transmissions overlapping at a receiver destroy each
//!   other (no capture effect), including hidden-terminal cases where the two
//!   senders cannot hear each other.
//! - **Loss**: per directed link, Bernoulli or Gilbert–Elliott (bursty).
//! - **Faults**: stations can be taken down and brought back at scheduled times.
//!
//! Channel access (CSMA) is *not* modelled here: it is the machine's job, so
//! the simulator measures whatever MAC the protocol implements.
//!
//! Same seed and same inputs give a byte-identical run; [`Report::trace`]
//! hashes every delivery and application event so tests can check that.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::hash::{Hash, Hasher};

use hm_core::{DetRng, Input, Machine, Millis, Output};

pub mod toy;

pub type NodeId = usize;

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
}

/// Physical parameters of the simulated channel.
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

    pub fn airtime(&self, frame_len: usize) -> Millis {
        let bits = (frame_len as u64 + self.phy_overhead_bytes as u64) * 8;
        let bps = self.bitrate_bps.max(1) as u64;
        Millis(self.txdelay.0 + (bits * 1000).div_ceil(bps))
    }
}

/// Channel-wide counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub airtime_ms: u64,
    /// Receiver-side outcomes, one per (transmission, neighbour) pair.
    pub delivered: u64,
    pub lost_channel: u64,
    pub lost_collision: u64,
    pub lost_half_duplex: u64,
    pub lost_down: u64,
}

/// Per-station counters.
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
    pub stats: Stats,
    pub nodes: Vec<NodeStats>,
    /// FNV-1a hash over every delivery and application event, in order.
    pub trace: u64,
}

struct Node<M> {
    machine: M,
    up: bool,
    radio_free_at: Millis,
    /// Deadline currently scheduled in the queue, with its generation.
    timer: Option<Millis>,
    gen: u64,
    same_time_firings: u32,
    last_firing: Millis,
    stats: NodeStats,
}

struct Link {
    loss: Loss,
    bad: bool,
}

struct Tx {
    id: u64,
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

/// FNV-1a, used for the run trace. Stable across Rust versions, unlike `DefaultHasher`.
struct Fnv(u64);

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

/// A simulated channel with stations of machine type `M`.
pub struct Sim<M, C, E>
where
    M: Machine<Input = Input<C>, Output = Output<E>>,
{
    now: Millis,
    radio: RadioParams,
    nodes: Vec<Node<M>>,
    links: BTreeMap<(NodeId, NodeId), Link>,
    queue: BinaryHeap<Reverse<Scheduled<C>>>,
    seq: u64,
    rng: DetRng,
    seed_rng: DetRng,
    txs: Vec<Tx>,
    max_airtime: Millis,
    stats: Stats,
    trace: Fnv,
    events: Vec<(Millis, NodeId, E)>,
}

impl<M, C, E> Sim<M, C, E>
where
    M: Machine<Input = Input<C>, Output = Output<E>>,
    E: Hash,
{
    pub fn new(seed: u64, radio: RadioParams) -> Self {
        let root = DetRng::from_seed(seed);
        Sim {
            now: Millis::ZERO,
            radio,
            nodes: Vec::new(),
            links: BTreeMap::new(),
            queue: BinaryHeap::new(),
            seq: 0,
            rng: root.fork(0),
            seed_rng: root.fork(1),
            txs: Vec::new(),
            max_airtime: Millis::ZERO,
            stats: Stats::default(),
            trace: Fnv(0xCBF2_9CE4_8422_2325),
            events: Vec::new(),
        }
    }

    /// A deterministic RNG for building machine `stream` (use the node index).
    pub fn machine_rng(&self, stream: u64) -> DetRng {
        self.seed_rng.fork(stream)
    }

    pub fn add_node(&mut self, machine: M) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            machine,
            up: true,
            radio_free_at: Millis::ZERO,
            timer: None,
            gen: 0,
            same_time_firings: 0,
            last_firing: Millis::ZERO,
            stats: NodeStats::default(),
        });
        self.reschedule_timer(id);
        id
    }

    /// `a` and `b` hear each other, both directions with the same loss model.
    pub fn link(&mut self, a: NodeId, b: NodeId, loss: Loss) {
        self.link_one_way(a, b, loss);
        self.link_one_way(b, a, loss);
    }

    /// `to` hears `from`.
    pub fn link_one_way(&mut self, from: NodeId, to: NodeId, loss: Loss) {
        assert!(from != to && from < self.nodes.len() && to < self.nodes.len());
        self.links.insert((from, to), Link { loss, bad: false });
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

    /// Application events emitted so far: `(time, station, event)`.
    pub fn events(&self) -> &[(Millis, NodeId, E)] {
        &self.events
    }

    pub fn report(&self) -> Report {
        Report {
            now: self.now,
            stats: self.stats.clone(),
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

    fn dispatch(&mut self, ev: Ev<C>) {
        match ev {
            Ev::Timer { node, gen } => self.fire_timer(node, gen),
            Ev::Command { node, cmd } => {
                if self.nodes[node].up {
                    let mut out = Vec::new();
                    self.nodes[node]
                        .machine
                        .handle(self.now, Input::Command(cmd), &mut out);
                    self.apply(node, out);
                }
            }
            Ev::SetUp { node, up } => {
                self.nodes[node].up = up;
                (self.now, node, up as u8, 0xFAu8).hash(&mut self.trace);
                if up {
                    self.reschedule_timer(node);
                } else {
                    let n = &mut self.nodes[node];
                    n.gen += 1;
                    n.timer = None;
                }
            }
            Ev::TxEnd {
                id,
                from,
                start,
                frame,
            } => self.finish_tx(id, from, start, frame),
        }
    }

    fn fire_timer(&mut self, node: NodeId, gen: u64) {
        let now = self.now;
        let n = &mut self.nodes[node];
        if !n.up || n.gen != gen {
            return; // stale timer
        }
        n.timer = None;
        match n.machine.next_deadline() {
            Some(d) if d <= now => {}
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
        n.machine.on_deadline(now, &mut out);
        self.apply(node, out);
    }

    fn apply(&mut self, node: NodeId, out: Vec<Output<E>>) {
        for o in out {
            match o {
                Output::Transmit(frame) => self.start_tx(node, frame),
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
            self.push(d.max(now), Ev::Timer { node, gen });
        }
    }

    fn start_tx(&mut self, node: NodeId, frame: Vec<u8>) {
        if !self.nodes[node].up {
            return;
        }
        let airtime = self.radio.airtime(frame.len());
        let n = &mut self.nodes[node];
        let start = n.radio_free_at.max(self.now);
        let end = start + airtime;
        n.radio_free_at = end;
        n.stats.frames_sent += 1;
        n.stats.bytes_sent += frame.len() as u64;
        n.stats.airtime_ms += airtime.0;
        self.stats.frames_sent += 1;
        self.stats.bytes_sent += frame.len() as u64;
        self.stats.airtime_ms += airtime.0;
        self.max_airtime = self.max_airtime.max(airtime);
        self.seq += 1;
        let id = self.seq;
        self.txs.push(Tx {
            id,
            from: node,
            start,
            end,
        });
        self.push(
            end,
            Ev::TxEnd {
                id,
                from: node,
                start,
                frame,
            },
        );
    }

    fn finish_tx(&mut self, id: u64, from: NodeId, start: Millis, frame: Vec<u8>) {
        let end = self.now;
        let overlaps = |t: &Tx| t.start < end && start < t.end;
        let receivers: Vec<NodeId> = self
            .links
            .range((from, 0)..=(from, usize::MAX))
            .map(|(&(_, to), _)| to)
            .collect();
        for r in receivers {
            if !self.nodes[r].up {
                self.stats.lost_down += 1;
                continue;
            }
            if self.txs.iter().any(|t| t.from == r && overlaps(t)) {
                self.stats.lost_half_duplex += 1;
                continue;
            }
            let links = &self.links;
            if self
                .txs
                .iter()
                .any(|t| t.id != id && t.from != from && links.contains_key(&(t.from, r)) && overlaps(t))
            {
                self.stats.lost_collision += 1;
                continue;
            }
            let link = self
                .links
                .get_mut(&(from, r))
                .expect("receiver comes from link table");
            if lose(&mut self.rng, link) {
                self.stats.lost_channel += 1;
                continue;
            }
            self.stats.delivered += 1;
            self.nodes[r].stats.frames_received += 1;
            (self.now, from, r, 0xD1u8).hash(&mut self.trace);
            frame.hash(&mut self.trace);
            let mut out = Vec::new();
            self.nodes[r]
                .machine
                .handle(self.now, Input::Frame(frame.clone()), &mut out);
            self.apply(r, out);
        }
        let horizon = self.max_airtime;
        let now = self.now;
        self.txs.retain(|t| t.end + horizon > now);
    }
}

fn lose(rng: &mut DetRng, link: &mut Link) -> bool {
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
    }
}

#[cfg(test)]
mod tests;
