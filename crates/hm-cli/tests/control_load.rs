//! Control-plane airtime on a shared radio channel.
//!
//! N stations that all hear each other run the daemon's radio control policy:
//! signed beacons, live CONTACT adverts for the stations they hear, Trickle
//! dissemination of every advert they learn, and the rolling control budget.
//! The simulator puts the frames on one channel with p-persistent CSMA, so
//! collisions and deferral are real. The question is the one that sinks
//! flooding meshes: does control traffic stay small as the network grows?

use std::collections::{BTreeMap, VecDeque};

use hm_cli::node::control::{
    holdings_filter, ControlBudget, Trickle, CONTROL_BUDGET_PERMYRIAD, CONTROL_BUDGET_WINDOW_MS,
};
use hm_core::{DetRng, Input, Machine, Millis, Output};
use hm_sim::{Csma, Loss, RadioParams, Sim};
use hm_wire::{
    Beacon, Callsign, ContactAdvert, ContactBearer, Dest, FrameHeader, FrameType, Heard, ObjectId,
    SyncMessage, FLAG_RELAY, MAX_HEARD,
};

const BEACON_EVERY_MS: u64 = 10 * 60 * 1000;
const FIRST_BEACON_MS: (u64, u64) = (5_000, 30_000);
const LIVE_BEACON_SECS: u64 = 20 * 60;
const LIVE_READVERTISE_SECS: u64 = 10 * 60;
const SYNC_QUEUE: usize = 64;
/// Pairwise holdings SYNC with a station heard, at most this often.
const PAIRWISE_SYNC_SECS: u64 = 5 * 60;
/// Objects in each station's store: what its holdings filters describe.
const STORED_OBJECTS: usize = 50;
const TICK_MS: u64 = 1_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    Beacon,
    Contact,
    Filter,
}

/// One station's control plane, as the daemon runs it.
struct Station {
    me: Callsign,
    radio: RadioParams,
    rng: DetRng,
    next_tick: Millis,
    next_beacon: Millis,
    /// Last time each station was heard, and the time in its last beacon.
    heard: BTreeMap<Callsign, (u64, u32)>,
    /// Radio links this station has seen evidence of, from beacons (the
    /// sender reaches us; each listed station reaches the sender): when.
    links: BTreeMap<(Callsign, Callsign), u64>,
    advertised_live: BTreeMap<Callsign, u64>,
    last_pairwise: BTreeMap<Callsign, u64>,
    trickle: Trickle,
    budget: ControlBudget,
    queue: VecDeque<(Kind, Vec<u8>)>,
    /// Adverts this station learned over the internet and refreshes every
    /// ten minutes (a gateway): `(peer, last refresh)`.
    internet: Vec<(Callsign, u64)>,
    sent: Vec<(u64, Kind, u64)>,
    dropped: u64,
}

fn call(i: usize) -> Callsign {
    Callsign::parse(&format!("T{i}X")).unwrap()
}

impl Station {
    fn new(me: Callsign, radio: RadioParams, mut rng: DetRng) -> Station {
        let first = FIRST_BEACON_MS.0 + rng.below(FIRST_BEACON_MS.1 - FIRST_BEACON_MS.0 + 1);
        Station {
            me,
            radio,
            rng,
            next_tick: Millis(TICK_MS),
            next_beacon: Millis(first),
            heard: BTreeMap::new(),
            links: BTreeMap::new(),
            advertised_live: BTreeMap::new(),
            last_pairwise: BTreeMap::new(),
            trickle: Trickle::default(),
            budget: ControlBudget::new(CONTROL_BUDGET_WINDOW_MS, CONTROL_BUDGET_PERMYRIAD).unwrap(),
            queue: VecDeque::new(),
            internet: Vec::new(),
            sent: Vec::new(),
            dropped: 0,
        }
    }

    fn advert(&self, peer: Callsign, bearer: ContactBearer, now_s: u64) -> ContactAdvert {
        ContactAdvert {
            origin: self.me,
            sequence: now_s as u32,
            start: now_s as u32,
            end: (now_s + 20 * 60) as u32,
            peer,
            bearer,
            success_permyriad: 7_000,
            rate_bps: self.radio.bitrate_bps,
            capacity_bytes: self.radio.bitrate_bps * 600 / 8,
            flags: FLAG_RELAY,
            signature: [0x5a; 64],
        }
    }

    fn transmit(&mut self, now: Millis, kind: Kind, data: Vec<u8>, out: &mut Vec<Output<()>>) {
        let airtime = self.radio.airtime_of(&data, true).0;
        self.sent.push((now.0, kind, airtime));
        out.push(Output::Transmit { port: 0, data });
    }

    fn beacon(&mut self, now: Millis, out: &mut Vec<Output<()>>) {
        let now_s = now.0 / 1000;
        let mut heard: Vec<(Callsign, u64)> = self
            .heard
            .iter()
            .filter(|(_, (at, _))| now_s.saturating_sub(*at) < 3600)
            .map(|(c, (at, _))| (*c, *at))
            .collect();
        heard.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
        let beacon = Beacon {
            flags: FLAG_RELAY,
            key: [7; 32],
            time: now_s as u32,
            locator: None,
            heard: heard
                .into_iter()
                .take(MAX_HEARD)
                .map(|(call, at)| Heard {
                    call,
                    minutes: (now_s.saturating_sub(at) / 60).min(255) as u8,
                })
                .collect(),
            signature: [0x33; 64],
        };
        let frame = FrameHeader {
            ftype: FrameType::Beacon,
            src: self.me,
            dst: Dest::Broadcast,
            session: 0,
            index: 0,
        }
        .frame(&beacon.to_vec().unwrap())
        .unwrap();
        self.transmit(now, Kind::Beacon, frame, out);
    }

    fn sync_frame(&self, dst: Dest, payload: &[u8]) -> Vec<u8> {
        FrameHeader {
            ftype: FrameType::Sync,
            src: self.me,
            dst,
            session: 0,
            index: 0,
        }
        .frame(payload)
        .unwrap()
    }

    fn enqueue(&mut self, kind: Kind, frame: Vec<u8>) {
        if self.queue.len() < SYNC_QUEUE {
            self.queue.push_back((kind, frame));
        } else {
            self.dropped += 1;
        }
    }

    /// The holdings filters the daemon sends a station it heard: mail held
    /// for us, and relayable holdings.
    fn filters(&mut self, to: Callsign, now_s: u64) {
        let ids: Vec<ObjectId> = (0..STORED_OBJECTS)
            .map(|i| {
                ObjectId(
                    *blake3::hash(&[self.me.to_bytes().as_slice(), &(i as u64).to_be_bytes()].concat())
                        .as_bytes(),
                )
            })
            .collect();
        for scope in [0, 1] {
            let filter = holdings_filter(scope, now_s as u32, &ids).unwrap();
            let payload = SyncMessage::Filter(filter).encode().unwrap();
            let frame = self.sync_frame(Dest::Station(to), &payload);
            self.enqueue(Kind::Filter, frame);
        }
    }

    fn tick(&mut self, now: Millis, out: &mut Vec<Output<()>>) {
        let now_s = now.0 / 1000;
        if now >= self.next_beacon {
            self.beacon(now, out);
            let jitter = BEACON_EVERY_MS / 10;
            self.next_beacon = Millis(now.0 + BEACON_EVERY_MS - jitter + self.rng.below(2 * jitter + 1));
        }
        for i in 0..self.internet.len() {
            let (peer, at) = self.internet[i];
            if now_s.saturating_sub(at) >= LIVE_READVERTISE_SECS {
                let mut advert = self.advert(peer, ContactBearer::Internet, now_s);
                advert.origin = call(10_000 + i);
                self.trickle.observe(advert, now_s * 1000).unwrap();
                self.internet[i].1 = now_s;
            }
        }
        self.trickle.expire(now_s);
        for advert in self.trickle.poll(now_s * 1000) {
            let payload = SyncMessage::Contact(advert).encode().unwrap();
            let frame = self.sync_frame(Dest::Broadcast, &payload);
            self.enqueue(Kind::Contact, frame);
        }
        while let Some((_, frame)) = self.queue.front() {
            let estimate = self.radio.txdelay.0
                + ((frame.len() as u64 + 24) * 8 * 1000).div_ceil(self.radio.bitrate_bps as u64);
            if !self.budget.admit(now.0, estimate) {
                break;
            }
            let (kind, frame) = self.queue.pop_front().unwrap();
            self.transmit(now, kind, frame, out);
        }
    }

    fn receive(&mut self, now: Millis, data: &[u8]) {
        let now_s = now.0 / 1000;
        let Ok((header, payload)) = FrameHeader::decode(data) else {
            return;
        };
        match header.ftype {
            FrameType::Beacon => {
                let Ok(beacon) = Beacon::decode(payload) else {
                    return;
                };
                let seen = self.heard.get(&header.src).map(|(_, t)| *t);
                self.heard.insert(header.src, (now_s, beacon.time));
                self.links.insert((header.src, self.me), now_s);
                for h in &beacon.heard {
                    let at = now_s.saturating_sub(u64::from(h.minutes) * 60);
                    let entry = self.links.entry((h.call, header.src)).or_insert(at);
                    *entry = (*entry).max(at);
                }
                if seen == Some(beacon.time)
                    || now_s.saturating_sub(u64::from(beacon.time)) >= LIVE_BEACON_SECS
                {
                    return;
                }
                let advertised = self.advertised_live.get(&header.src).copied().unwrap_or(0);
                if now_s.saturating_sub(advertised) >= LIVE_READVERTISE_SECS || advertised == 0 {
                    let advert = self.advert(header.src, ContactBearer::Radio, now_s);
                    self.trickle.observe(advert, now_s * 1000).unwrap();
                    self.advertised_live.insert(header.src, now_s);
                }
                let last = self.last_pairwise.get(&header.src).copied();
                if last.is_none_or(|at| now_s.saturating_sub(at) >= PAIRWISE_SYNC_SECS) {
                    self.filters(header.src, now_s);
                    self.last_pairwise.insert(header.src, now_s);
                }
            }
            FrameType::Sync => {
                if let Ok(SyncMessage::Contact(advert)) = SyncMessage::decode(payload) {
                    let _ = self.trickle.observe(advert, now_s * 1000);
                }
            }
            _ => {}
        }
    }
}

impl Machine for Station {
    type Input = Input<()>;
    type Output = Output<()>;

    fn handle(&mut self, now: Millis, input: Input<()>, _out: &mut Vec<Output<()>>) {
        if let Input::Frame { data, .. } = input {
            self.receive(now, &data);
        }
    }

    fn on_deadline(&mut self, now: Millis, out: &mut Vec<Output<()>>) {
        self.tick(now, out);
        self.next_tick = Millis(now.0 + TICK_MS);
    }

    fn next_deadline(&self) -> Option<Millis> {
        Some(self.next_tick)
    }
}

struct Load {
    beacon_share: f64,
    contact_share: f64,
    filter_share: f64,
    channel_share: f64,
    collisions: u64,
    dropped: u64,
    /// Live radio contacts of the other stations that each station holds
    /// from CONTACT adverts, as a share of all of them.
    known: f64,
    /// Radio links between stations each station knows of lately, from
    /// beacons or adverts, as a share of all of them.
    known_any: f64,
}

fn run(n: usize, radio: RadioParams, internet_adverts: usize, seed: u64) -> Load {
    let warmup = Millis(60 * 60 * 1000);
    let end = Millis(3 * 60 * 60 * 1000);
    let mut sim: Sim<Station, (), ()> = Sim::new(seed, radio);
    for i in 0..n {
        let rng = sim.machine_rng(i as u64);
        let mut station = Station::new(call(i), radio, rng);
        if i == 0 {
            station.internet = (0..internet_adverts).map(|j| (call(20_000 + j), 0)).collect();
        }
        let id = sim.add_node(station);
        sim.set_csma(id, 0, Some(Csma::DEFAULT));
    }
    for a in 0..n {
        for b in a + 1..n {
            sim.link(a, b, Loss::None);
        }
    }
    sim.run_until(warmup);
    let before = sim.report().total();
    sim.run_until(end);
    let after = sim.report().total();
    let span = (end.0 - warmup.0) as f64;
    let (mut beacon, mut contact, mut filter, mut dropped) = (0u64, 0u64, 0u64, 0u64);
    for station in sim.machines() {
        for (at, kind, airtime) in &station.sent {
            if *at >= warmup.0 {
                match kind {
                    Kind::Beacon => beacon += airtime,
                    Kind::Contact => contact += airtime,
                    Kind::Filter => filter += airtime,
                }
            }
        }
        dropped += station.dropped;
    }
    let now_s = end.0 / 1000;
    let (mut known, mut known_any) = (0usize, 0usize);
    for station in sim.machines() {
        let from_adverts: std::collections::BTreeSet<_> = station
            .trickle
            .adverts()
            .filter(|a| {
                a.bearer == ContactBearer::Radio && u64::from(a.end) > now_s && a.origin != station.me
            })
            .map(|a| (a.origin, a.peer))
            .collect();
        known += from_adverts.len();
        let mut any: std::collections::BTreeSet<_> = station
            .links
            .iter()
            .filter(|(_, at)| now_s.saturating_sub(**at) < LIVE_BEACON_SECS)
            .map(|(link, _)| *link)
            .collect();
        any.extend(from_adverts);
        known_any += any.len();
    }
    // Each station can learn the contacts of the other n - 1 stations, to
    // any of their n - 1 neighbours.
    let possible = n * (n - 1) * (n - 1);
    Load {
        beacon_share: beacon as f64 / span,
        contact_share: contact as f64 / span,
        filter_share: filter as f64 / span,
        channel_share: (after.airtime_ms - before.airtime_ms) as f64 / span,
        collisions: after.lost_collision - before.lost_collision,
        dropped,
        known: known as f64 / possible as f64,
        known_any: known_any as f64 / (n * n * (n - 1)) as f64,
    }
}

fn table(radio: RadioParams, name: &str, internet_adverts: usize) {
    println!("{name}, {internet_adverts} internet adverts at station 0:");
    println!("   N  beacons  contacts  filters  channel  collisions  dropped  adverts  links");
    for n in [5, 10, 20, 40] {
        let l = run(n, radio, internet_adverts, 7);
        println!(
            "  {n:>2}  {:>6.1}%  {:>7.1}%  {:>6.1}%  {:>6.1}%  {:>10}  {:>7}  {:>6.0}%  {:>4.0}%",
            100.0 * l.beacon_share,
            100.0 * l.contact_share,
            100.0 * l.filter_share,
            100.0 * l.channel_share,
            l.collisions,
            l.dropped,
            100.0 * l.known,
            100.0 * l.known_any
        );
    }
}

/// The table behind the control-plane numbers in the protocol assessment.
#[test]
#[ignore = "measurement: control-plane airtime by network size"]
fn control_plane_load_by_network_size() {
    table(RadioParams::VHF_1200, "VHF 1200 bd", 0);
    table(RadioParams::HF_300, "HF 300 bd", 0);
    table(RadioParams::VHF_1200, "VHF 1200 bd gateway", 50);
}
