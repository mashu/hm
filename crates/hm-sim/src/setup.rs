//! Building a scenario: channels, stations and their radios, links and
//! their loss, faults, and commands at set times.

use std::collections::BTreeMap;
use std::hash::Hash;

use hm_core::{DetRng, Input, Machine, Millis, Output, Port};

use crate::{
    ChannelId, Clock, Csma, Ev, Link, LogEntry, Loss, Node, NodeId, NodeStats, Radio, RadioParams, Sim, Stats,
};

impl<M, C, E> Sim<M, C, E>
where
    M: Machine<Input = Input<C>, Output = Output<E>>,
    E: Hash,
{
    pub fn add_channel(&mut self, radio: RadioParams) -> ChannelId {
        self.channels.push(radio);
        self.channel_stats.push(Stats::default());
        ChannelId(self.channels.len() - 1)
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
                keyed_at: Millis::ZERO,
                csma: None,
                waiting: Vec::new(),
                access_gen: 0,
                access_pending: false,
            },
        );
    }

    /// Channel access for the radio on `port` of `node`: `None` (the default)
    /// keys up as soon as the machine asks, `Some` waits for a clear channel.
    pub fn set_csma(&mut self, node: NodeId, port: Port, csma: Option<Csma>) {
        let r = self.nodes[node]
            .radios
            .get_mut(&port)
            .unwrap_or_else(|| panic!("station {node} has no radio on port {port}"));
        r.csma = csma;
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
}
