//! The node's decisions, as a state machine: events in, commands out.
//!
//! [`Node`] owns everything the station decides with (the contact plan, what
//! it believes about links and custodians, the control plane, messages in
//! flight) and does no I/O of its own: time comes in with every call, radio,
//! internet and modem results come in as [`Input`]s, and what should be sent
//! goes out as [`Command`]s for the caller to carry out. The daemon's
//! coordinator is a thin shell around it; a simulation can drive many nodes
//! through weeks of simulated time in seconds.
//!
//! The store is the node's memory and stays a handle: it is local, and a
//! simulation gives each node a store of its own.
//!
//! - [`handoff`]: how a transfer ended and what that teaches;
//! - [`radio`]: radio events, beacons heard and not heard;
//! - [`links`]: internet links, contact adverts, holdings SYNC;
//! - [`custody`]: custody taken or reclaimed, receipts, custody failures;
//! - [`deliver`]: routing and handing off what is due.

mod custody;
mod deliver;
mod handoff;
mod links;
mod radio;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hm_core::DetRng;
use hm_ident::{Identity, PublicKey};
use hm_model::{Bearer, Beliefs};
use hm_route::{ContactGraph, GraphConfig, ScheduledContact};
use hm_store::Store;
use hm_wire::{Callsign, ObjectId};

use crate::adverts::advertised_flags;
use crate::control::{live_window_secs, ControlPlane};
use crate::{heard, log, Notify, RadioCmd, RadioEvt, Settings, Transfer};

use handoff::InFlight;
use links::LiveClaim;

/// What an ARQ modem carries.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ModemSpec {
    /// Its rate, in bits per second.
    pub rate_bps: u32,
    /// The largest object it takes, in bytes.
    pub max_object: u64,
}

/// Who the node is and what it has, fixed for its run.
pub struct NodeIdentity {
    pub me: Callsign,
    /// The callsign the node's key is bound to.
    pub key_call: Callsign,
    pub identity: Identity,
    pub schedules: Vec<ScheduledContact>,
    /// Whether the node has an internet endpoint (its adverts say so).
    pub has_internet: bool,
    /// The ARQ modem, if the node has one.
    pub modem: Option<ModemSpec>,
    /// How the radio is reached, if the node has one.
    pub radio_via: Option<String>,
    /// Seed of the node's random choices (route planning draws).
    pub seed: u64,
}

/// What happened, for the node to act on.
pub enum Input {
    /// A second has passed. `links`: internet peers linked now; `modem`: the
    /// ARQ modem's state (up, connected peer), if the node has one.
    Tick {
        links: Vec<Callsign>,
        modem: Option<(bool, Option<Callsign>)>,
    },
    Radio(RadioEvt),
    /// A transfer over the internet ended.
    NetDone {
        id: ObjectId,
        peer: Callsign,
        result: Transfer,
    },
    /// A transfer through the ARQ modem ended.
    ModemDone {
        id: ObjectId,
        peer: Callsign,
        result: Transfer,
    },
    /// SYNC from an internet peer, with the key its link proved.
    NetSync {
        from: Callsign,
        payload: Vec<u8>,
        peer_key: Option<PublicKey>,
    },
    /// The settings changed.
    Settings(Box<Settings>),
}

/// What the node wants done.
#[derive(Debug)]
pub enum Command {
    Radio(RadioCmd),
    /// Transfer `object` to `peer` over the internet; the result comes back
    /// as [`Input::NetDone`].
    NetDeliver {
        id: ObjectId,
        peer: Callsign,
        object: Vec<u8>,
    },
    /// Transfer `object` to `peer` through the ARQ modem; the result comes
    /// back as [`Input::ModemDone`].
    ModemDeliver {
        id: ObjectId,
        peer: Callsign,
        object: Vec<u8>,
    },
    /// Send SYNC `payload` to `peer` over the internet.
    NetSync {
        peer: Callsign,
        payload: Vec<u8>,
    },
}

/// What the node shows of itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeStatus {
    pub radio: Option<bool>,
    pub radio_via: Option<String>,
    pub internet_peers: Vec<Callsign>,
    pub modem: Option<bool>,
    pub modem_peer: Option<Callsign>,
    /// (station, bearer, chance a handoff to it completes now)
    pub estimates: Vec<(Callsign, &'static str, f64)>,
    pub heard: Vec<heard::Station>,
}

pub struct Node {
    id: NodeIdentity,
    settings: Settings,
    store: Arc<Store>,
    notify: Notify,
    /// Each route plan draws from the beliefs once, from its own stream.
    rng: DetRng,
    plans: u64,
    graph: ContactGraph,
    control: ControlPlane,
    beliefs: Beliefs,
    /// Chance, beyond the custodian, of each message handed over lately:
    /// what a missing end-to-end receipt is weighed against.
    handed: BTreeMap<ObjectId, f64>,
    /// The flags our adverts carry, and the base of our schedules' sequence
    /// numbers: when the flags change the schedules are advertised again.
    advertised: u8,
    schedule_base: u32,
    radio_up: bool,
    last_down: Option<String>,
    radio_via: Option<String>,
    in_flight: BTreeMap<(ObjectId, Callsign), InFlight>,
    /// Radio transfers by the id the transfer engine gave them.
    radio_ids: BTreeMap<(ObjectId, Callsign), ObjectId>,
    links: BTreeSet<Callsign>,
    modem: Option<(bool, Option<Callsign>)>,
    last_pairwise_sync: BTreeMap<(Callsign, Bearer), u64>,
    last_sync_ignore: Option<(Callsign, u64)>,
    advertised_live: BTreeMap<(Callsign, Bearer), LiveClaim>,
    /// A beacon heard this long ago no longer shows a live radio contact; it
    /// follows the beacon interval, which grows as more stations share the
    /// channel. Beacons due and not heard are news too.
    live_window: u64,
    beacon_interval: u64,
    holding_sent: Option<bool>,
    next_holding_check: u64,
    /// When the beacon of each station last taken in was heard.
    beacons_seen: BTreeMap<Callsign, u64>,
    heard: Vec<heard::Station>,
}

impl Node {
    /// A node starting at `now`, with what it learned before from `store`.
    pub fn new(id: NodeIdentity, settings: Settings, store: Arc<Store>, notify: Notify, now: u64) -> Node {
        let mut graph = ContactGraph::new(GraphConfig::default()).expect("default contact graph");
        for schedule in &id.schedules {
            if let Err(error) = graph.add_schedule(*schedule) {
                log(format!("ignored contact schedule: {error}"));
            }
        }
        let mut beliefs = Beliefs::new();
        match store.beliefs() {
            Ok(saved) => {
                for (key, value) in saved {
                    if let Err(error) = beliefs.restore(&key, &value) {
                        log(format!("ignored a saved belief: {error}"));
                    }
                }
            }
            Err(error) => log(format!("could not restore beliefs: {error}")),
        }
        let live_window = live_window_secs(settings.beacon_secs);
        graph.set_live_contact_secs(live_window);
        let mut node = Node {
            rng: DetRng::from_seed(id.seed),
            plans: 0,
            control: ControlPlane::for_station(id.me),
            advertised: advertised_flags(id.has_internet, &settings.relay),
            schedule_base: now as u32,
            radio_up: false,
            last_down: None,
            radio_via: id.radio_via.clone(),
            beacon_interval: settings.beacon_secs,
            id,
            settings,
            store,
            notify,
            graph,
            beliefs,
            handed: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            radio_ids: BTreeMap::new(),
            links: BTreeSet::new(),
            modem: None,
            last_pairwise_sync: BTreeMap::new(),
            last_sync_ignore: None,
            advertised_live: BTreeMap::new(),
            live_window,
            holding_sent: None,
            next_holding_check: 0,
            beacons_seen: BTreeMap::new(),
            heard: Vec::new(),
        };
        node.advertise_schedules(now);
        node
    }

    /// Take `input`, happening at `now` (Unix seconds); what to do goes to `out`.
    pub fn handle(&mut self, now: u64, input: Input, out: &mut Vec<Command>) {
        match input {
            Input::Tick { links, modem } => self.tick(now, links, modem, out),
            Input::Radio(event) => self.radio_event(now, event, out),
            Input::NetDone { id, peer, result } => self.net_done(now, id, peer, result),
            Input::ModemDone { id, peer, result } => self.modem_done(now, id, peer, result),
            Input::NetSync {
                from,
                payload,
                peer_key,
            } => self.receive_sync(now, Bearer::Internet, from, &payload, peer_key, out),
            Input::Settings(settings) => self.apply_settings(now, *settings),
        }
    }

    /// What the node shows of itself now.
    pub fn status(&self, now: u64) -> NodeStatus {
        NodeStatus {
            radio: self.radio_via.as_ref().map(|_| self.radio_up),
            radio_via: self.radio_via.clone(),
            internet_peers: self.links.iter().copied().collect(),
            modem: self.modem.map(|(up, _)| up),
            modem_peer: self.modem.and_then(|(_, peer)| peer),
            estimates: self
                .beliefs
                .links()
                .filter(|(key, _)| key.from == self.id.me)
                .map(|(key, _)| {
                    (
                        key.to,
                        key.bearer.name(),
                        self.beliefs.link_success(*key, now, now),
                    )
                })
                .collect(),
            heard: self.heard.clone(),
        }
    }

    /// Whether an internet link to `peer` (or to its base callsign) is up.
    fn linked(&self, peer: Callsign) -> bool {
        self.links.contains(&peer) || self.links.contains(&peer.base())
    }

    fn modem_up(&self) -> bool {
        self.modem.is_some_and(|(up, _)| up)
    }

    fn apply_settings(&mut self, now: u64, settings: Settings) {
        self.settings = settings;
        let flags = advertised_flags(self.id.has_internet, &self.settings.relay);
        if flags != self.advertised {
            self.advertised = flags;
            // Above every sequence number used so far, even within one second.
            self.schedule_base = (now as u32).max(
                self.schedule_base
                    .wrapping_add(self.id.schedules.len().max(1) as u32),
            );
            self.advertise_schedules(now);
            // Live contacts are claimed again at once, with the new flags.
            for claim in self.advertised_live.values_mut() {
                claim.expire(now);
            }
        }
    }

    /// Once a second: save what was learned, take receipts in, look after
    /// links and adverts, reclaim suspect custody, send what is due.
    fn tick(
        &mut self,
        now: u64,
        links: Vec<Callsign>,
        modem: Option<(bool, Option<Callsign>)>,
        out: &mut Vec<Command>,
    ) {
        self.modem = modem;
        self.beliefs.prune(now);
        if let Err(error) = self.store.save_beliefs(&self.beliefs.take_changed()) {
            log(format!("store: {error}"));
        }
        self.take_custody_outcomes();
        self.internet_links(now, links.into_iter().collect(), out);
        self.graph.prune(now);
        self.due_contacts(now, out);
        self.holding_check(now, out);
        self.reclaim_suspects(now);
        self.deliver_due(now, out);
    }
}
