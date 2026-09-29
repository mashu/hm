//! Control-plane airtime on a shared radio channel.
//!
//! N stations that all hear each other run the daemon's radio control policy,
//! built from the same pieces the daemon uses (`hm_cli::node::control`):
//! signed beacons at an interval that grows with the stations sharing the
//! channel, live contact claims (internet only), Trickle dissemination of
//! scheduled contacts on air, holdings filters sent only to stations whose
//! beacon says they hold something, and one control budget shared by the
//! stations in reach. The simulator puts the frames on one channel with
//! p-persistent CSMA, so collisions and deferral are real. The question is
//! the one that sinks flooding meshes: does control traffic stay small as
//! the network grows?
//!
//! Before this policy (commit 5ff3fef, the same harness on the old policy),
//! control traffic took 19% of a VHF channel at 10 stations, 40% at 20 and
//! 75% at 40, and more than an HF 300 bd channel could carry at 40.

use std::collections::{BTreeMap, BTreeSet};

use hm_cli::node::control::{
    beacon_interval_ms, control_share_permyriad, holdings_filter, live_window_secs, radio_pull_due,
    AdvertSource, ControlBudget, SyncQueue, Trickle, CONTROL_BUDGET_PERMYRIAD, CONTROL_BUDGET_WINDOW_MS,
    LIVE_ADVERT_REFRESH_SECS, LIVE_ADVERT_VALIDITY_SECS, SYNC_QUEUE_FRAMES,
};
use hm_core::{DetRng, Input, Machine, Millis, Output};
use hm_sim::{Csma, Loss, RadioParams, Sim};
use hm_wire::{
    Beacon, Callsign, ContactAdvert, ContactBearer, Dest, FrameHeader, FrameType, Heard, ObjectId,
    SyncMessage, FLAG_HOLDING, FLAG_RELAY, MAX_HEARD,
};

const BEACON_EVERY_MS: u64 = 10 * 60 * 1000;
const FIRST_BEACON_MS: (u64, u64) = (5_000, 30_000);
/// Stations heard within this long share the channel, at least (as `radio.rs`).
const SHARING_WINDOW_SECS: u64 = 3600;
/// Objects in each station's store: what its holdings filters describe.
const STORED_OBJECTS: usize = 50;
const TICK_MS: u64 = 1_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    Beacon,
    Contact,
    Filter,
}

#[derive(Copy, Clone)]
struct Scenario {
    radio: RadioParams,
    /// Every this-many-th station holds something for others (a mailbox or
    /// relay with mail waiting); 1: all of them.
    holding_every: usize,
    /// Scheduled contacts each station claims (advertised on air).
    schedules: usize,
    /// Adverts station 0 learned over the internet (a gateway).
    internet_adverts: usize,
}

/// One station's control plane, as the daemon runs it.
struct Station {
    me: Callsign,
    radio: RadioParams,
    rng: DetRng,
    next_tick: Millis,
    next_beacon: Millis,
    beacon_interval_secs: u64,
    holding: bool,
    /// Last time each station was heard, and the time in its last beacon.
    heard: BTreeMap<Callsign, (u64, u32)>,
    /// Radio links this station has seen evidence of, from beacons (the
    /// sender reaches us; each listed station reaches the sender): when.
    links: BTreeMap<(Callsign, Callsign), u64>,
    /// Live claims: `(since, claimed_at)` by peer.
    claims: BTreeMap<Callsign, (u64, u64)>,
    last_pull: BTreeMap<Callsign, u64>,
    trickle: Trickle,
    budget: ControlBudget,
    sharing: usize,
    queue: SyncQueue,
    schedules: Vec<Callsign>,
    /// Adverts learned over the internet, refreshed every half hour.
    internet: Vec<(Callsign, u64)>,
    sent: Vec<(u64, Kind, u64)>,
}

fn call(i: usize) -> Callsign {
    Callsign::parse(&format!("T{i}X")).unwrap()
}

fn airtime_estimate(radio: &RadioParams, frame_len: usize) -> u64 {
    radio.txdelay.0 + ((frame_len as u64 + 24) * 8 * 1000).div_ceil(radio.bitrate_bps as u64)
}

impl Station {
    fn new(me: Callsign, scenario: Scenario, holding: bool, mut rng: DetRng) -> Station {
        let first = FIRST_BEACON_MS.0 + rng.below(FIRST_BEACON_MS.1 - FIRST_BEACON_MS.0 + 1);
        Station {
            me,
            radio: scenario.radio,
            rng,
            next_tick: Millis(TICK_MS),
            next_beacon: Millis(first),
            beacon_interval_secs: BEACON_EVERY_MS / 1000,
            holding,
            heard: BTreeMap::new(),
            links: BTreeMap::new(),
            claims: BTreeMap::new(),
            last_pull: BTreeMap::new(),
            trickle: Trickle::default().with_salt(me.packed()),
            budget: ControlBudget::new(CONTROL_BUDGET_WINDOW_MS, CONTROL_BUDGET_PERMYRIAD).unwrap(),
            sharing: 1,
            queue: SyncQueue::new(SYNC_QUEUE_FRAMES),
            schedules: (0..scenario.schedules).map(|j| call(30_000 + j)).collect(),
            internet: Vec::new(),
            sent: Vec::new(),
        }
    }

    fn advert(
        &self,
        peer: Callsign,
        bearer: ContactBearer,
        since: u64,
        now_s: u64,
        valid: u64,
    ) -> ContactAdvert {
        ContactAdvert {
            origin: self.me,
            sequence: now_s as u32,
            start: since as u32,
            end: (now_s + valid) as u32,
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

    fn live_window(&self) -> u64 {
        live_window_secs(self.beacon_interval_secs)
    }

    fn beacon(&mut self, now: Millis, out: &mut Vec<Output<()>>) {
        let now_s = now.0 / 1000;
        let window = self.live_window().max(SHARING_WINDOW_SECS);
        let mut heard: Vec<(Callsign, u64)> = self
            .heard
            .iter()
            .filter(|(_, (at, _))| now_s.saturating_sub(*at) < window)
            .map(|(c, (at, _))| (*c, *at))
            .collect();
        heard.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
        let beacon = Beacon {
            flags: FLAG_RELAY | if self.holding { FLAG_HOLDING } else { 0 },
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
        let every = beacon_interval_ms(
            BEACON_EVERY_MS,
            self.sharing,
            airtime_estimate(&self.radio, frame.len()),
        );
        self.beacon_interval_secs = every / 1000;
        let jitter = every / 10;
        self.next_beacon = Millis(now.0 + every - jitter + self.rng.below(2 * jitter + 1));
        self.transmit(now, Kind::Beacon, frame, out);
    }

    fn tick(&mut self, now: Millis, out: &mut Vec<Output<()>>) {
        let now_s = now.0 / 1000;
        let window = self.live_window().max(SHARING_WINDOW_SECS);
        let sharing = self
            .heard
            .values()
            .filter(|(at, _)| now_s.saturating_sub(*at) < window)
            .count()
            + 1;
        if sharing != self.sharing {
            self.sharing = sharing;
            self.budget
                .set_permyriad(control_share_permyriad(CONTROL_BUDGET_PERMYRIAD, sharing));
        }
        if now >= self.next_beacon {
            self.beacon(now, out);
        }
        // Scheduled contacts: stable claims, on air.
        for i in 0..self.schedules.len() {
            if now_s % 3600 == 1 || now_s == 1 {
                let advert = self.advert(self.schedules[i], ContactBearer::Radio, 0, 1, 7 * 24 * 3600);
                self.trickle
                    .observe(advert, now_s * 1000, AdvertSource::Own { on_air: true })
                    .unwrap();
            }
        }
        for i in 0..self.internet.len() {
            let (peer, at) = self.internet[i];
            if at == 0 || now_s.saturating_sub(at) >= LIVE_ADVERT_REFRESH_SECS {
                let mut advert =
                    self.advert(peer, ContactBearer::Internet, 1, now_s, LIVE_ADVERT_VALIDITY_SECS);
                advert.origin = call(10_000 + i);
                self.trickle
                    .observe(advert, now_s * 1000, AdvertSource::Internet)
                    .unwrap();
                self.internet[i].1 = now_s;
            }
        }
        self.trickle.expire(now_s);
        for due in self.trickle.poll(now_s * 1000) {
            if due.on_air {
                let payload = SyncMessage::Contact(due.advert).encode().unwrap();
                self.queue.push(Dest::Broadcast, payload, now_s);
            }
        }
        while let Some(item) = self.queue.front(now_s) {
            let frame = FrameHeader {
                ftype: FrameType::Sync,
                src: self.me,
                dst: item.to,
                session: 0,
                index: 0,
            }
            .frame(&item.payload)
            .unwrap();
            if !self
                .budget
                .admit(now.0, airtime_estimate(&self.radio, frame.len()))
            {
                break;
            }
            let kind = match SyncMessage::decode(&item.payload) {
                Ok(SyncMessage::Contact(_)) => Kind::Contact,
                _ => Kind::Filter,
            };
            self.queue.pop();
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
                    || now_s.saturating_sub(u64::from(beacon.time)) >= self.live_window()
                {
                    return;
                }
                // A live claim, as `claim_live`: internet only.
                let claim = self.claims.get(&header.src).copied();
                if claim.is_none_or(|(_, at)| now_s.saturating_sub(at) >= LIVE_ADVERT_REFRESH_SECS) {
                    let since = match claim {
                        Some((since, at)) if now_s.saturating_sub(at) < LIVE_ADVERT_VALIDITY_SECS => since,
                        _ => now_s,
                    };
                    let advert = self.advert(
                        header.src,
                        ContactBearer::Radio,
                        since,
                        now_s,
                        LIVE_ADVERT_VALIDITY_SECS,
                    );
                    self.trickle
                        .observe(advert, now_s * 1000, AdvertSource::Own { on_air: false })
                        .unwrap();
                    self.claims.insert(header.src, (since, now_s));
                }
                let last = self.last_pull.get(&header.src).copied();
                if radio_pull_due(beacon.flags, last, now_s) {
                    let ids: Vec<ObjectId> = (0..STORED_OBJECTS)
                        .map(|i| {
                            ObjectId(
                                *blake3::hash(
                                    &[self.me.to_bytes().as_slice(), &(i as u64).to_be_bytes()].concat(),
                                )
                                .as_bytes(),
                            )
                        })
                        .collect();
                    for scope in [0, 1] {
                        let filter = holdings_filter(scope, now_s as u32, &ids).unwrap();
                        let payload = SyncMessage::Filter(filter).encode().unwrap();
                        self.queue.push(Dest::Station(header.src), payload, now_s);
                    }
                    self.last_pull.insert(header.src, now_s);
                }
            }
            FrameType::Sync => {
                self.queue.heard(payload);
                if let Ok(SyncMessage::Contact(advert)) = SyncMessage::decode(payload) {
                    let _ = self.trickle.observe(advert, now_s * 1000, AdvertSource::Radio);
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
    /// Scheduled contacts of the other stations each station holds, as a
    /// share of all of them.
    schedules_known: f64,
    /// Radio links between stations each station knows of lately, from
    /// beacons, as a share of all of them.
    links_known: f64,
    beacon_interval_min: f64,
}

fn run(n: usize, scenario: Scenario, seed: u64) -> Load {
    let warmup = Millis(2 * 60 * 60 * 1000);
    let end = Millis(6 * 60 * 60 * 1000);
    let mut sim: Sim<Station, (), ()> = Sim::new(seed, scenario.radio);
    for i in 0..n {
        let rng = sim.machine_rng(i as u64);
        let mut station = Station::new(call(i), scenario, i % scenario.holding_every == 0, rng);
        if i == 0 {
            station.internet = (0..scenario.internet_adverts)
                .map(|j| (call(20_000 + j), 0))
                .collect();
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
    let now_s = end.0 / 1000;
    let (mut schedules_known, mut links_known, mut interval) = (0usize, 0usize, 0u64);
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
        dropped += station.queue.dropped();
        schedules_known += station
            .trickle
            .adverts()
            .filter(|a| {
                a.origin != station.me && a.bearer == ContactBearer::Radio && u64::from(a.end) > now_s
            })
            .map(|a| (a.origin, a.peer))
            .collect::<BTreeSet<_>>()
            .len();
        links_known += station
            .links
            .iter()
            .filter(|(_, at)| now_s.saturating_sub(**at) < station.live_window())
            .count();
        interval += station.beacon_interval_secs;
    }
    let schedules_possible = (n * (n - 1) * scenario.schedules).max(1);
    Load {
        beacon_share: beacon as f64 / span,
        contact_share: contact as f64 / span,
        filter_share: filter as f64 / span,
        channel_share: (after.airtime_ms - before.airtime_ms) as f64 / span,
        collisions: after.lost_collision - before.lost_collision,
        dropped,
        schedules_known: schedules_known as f64 / schedules_possible as f64,
        links_known: links_known as f64 / (n * n * (n - 1)) as f64,
        beacon_interval_min: interval as f64 / n as f64 / 60.0,
    }
}

fn table(name: &str, scenario: Scenario) {
    println!(
        "{name}: 1 in {} stations holding, {} schedules each, {} internet adverts at station 0",
        scenario.holding_every, scenario.schedules, scenario.internet_adverts
    );
    println!(
        "   N  beacon every  beacons  contacts  filters  channel  collisions  dropped  schedules  links"
    );
    for n in [5, 10, 20, 40] {
        let l = run(n, scenario, 7);
        println!(
            "  {n:>2}  {:>8.0} min  {:>6.1}%  {:>7.1}%  {:>6.1}%  {:>6.1}%  {:>10}  {:>7}  {:>8.0}%  {:>4.0}%",
            l.beacon_interval_min,
            100.0 * l.beacon_share,
            100.0 * l.contact_share,
            100.0 * l.filter_share,
            100.0 * l.channel_share,
            l.collisions,
            l.dropped,
            100.0 * l.schedules_known,
            100.0 * l.links_known
        );
    }
}

const VHF: Scenario = Scenario {
    radio: RadioParams::VHF_1200,
    holding_every: 4,
    schedules: 1,
    internet_adverts: 0,
};

/// The table behind the control-plane numbers in the protocol assessment.
#[test]
#[ignore = "measurement: control-plane airtime by network size"]
fn control_plane_load_by_network_size() {
    table("VHF 1200 bd", VHF);
    table(
        "HF 300 bd",
        Scenario {
            radio: RadioParams::HF_300,
            ..VHF
        },
    );
    table(
        "VHF 1200 bd, every station holding",
        Scenario {
            holding_every: 1,
            ..VHF
        },
    );
    table(
        "VHF 1200 bd gateway",
        Scenario {
            internet_adverts: 50,
            ..VHF
        },
    );
}

/// However many stations share the channel, their control traffic together
/// stays within the beacon and control budgets (2% each), and never goes out
/// stale.
#[test]
fn control_traffic_stays_within_its_share_of_the_channel() {
    for n in [10, 30] {
        let l = run(
            n,
            Scenario {
                holding_every: 1,
                ..VHF
            },
            11,
        );
        assert!(
            l.beacon_share < 0.025,
            "{n} stations: beacons take {:.1}% of the channel",
            100.0 * l.beacon_share
        );
        assert!(
            l.contact_share + l.filter_share < 0.025,
            "{n} stations: SYNC takes {:.1}% of the channel",
            100.0 * (l.contact_share + l.filter_share)
        );
        assert!(
            l.links_known > 0.4,
            "{n} stations: links known {:.0}%",
            100.0 * l.links_known
        );
        // The old policy spent up to 75% of the channel and taught nothing.
        assert!(
            l.schedules_known > 0.6,
            "{n} stations: schedules known {:.0}%",
            100.0 * l.schedules_known
        );
    }
}
