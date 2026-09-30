//! Stations on simulated HF for days.
//!
//! Each station is a real [`hm_node::Station`] (the node's decisions and the
//! radio's transfer engine, beacons and control plane, as the daemon runs
//! them) with a store of its own in memory. They share one 300 bd HF channel
//! in hm-sim, with p-persistent CSMA. Each path between two stations opens
//! and closes through the day: in each UTC hour it is open with that hour's
//! probability, and it tends to stay as it was (band openings last). While
//! open it fades as a Watterson HF channel does. Messages are queued at
//! random times between random pairs; many pairs have no direct path, and
//! need a relay.
//!
//! An oracle knows the opening schedule and finds each message's earliest
//! possible arrival over it (paths that open later, relays that hold): what a
//! network that knew the future, and had no airtime limits, could do.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use hm_bundle::Precedence;
use hm_core::{DetRng, Millis};
use hm_ident::Identity;
use hm_node::{
    Costs, LinkTiming, RelaySettings, Settings, Station, StationCmd, StationEvent, StationSpec, Trust,
};
use hm_sim::{Csma, Loss, RadioParams, Sim, Stats};
use hm_store::{Direction, RetryPolicy, Store};
use hm_wire::{Callsign, ObjectId, FEATURE_COMPACT};

/// 2026-01-01 00:00:00 UTC: simulation time 0 is midnight UTC.
pub const EPOCH: u64 = 1_767_225_600;
const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 24 * HOUR_MS;

/// One HF path between two stations.
#[derive(Clone, Debug)]
pub struct Path {
    pub a: usize,
    pub b: usize,
    /// Chance the path is open in each UTC hour.
    pub open: [f64; 24],
    /// Chance an hour keeps the previous hour's state rather than drawing
    /// anew: how long openings last.
    pub persistence: f64,
    pub snr_db: f64,
    /// Doppler spread of the fading while open: 0.1 Hz quiet, 0.5 Hz
    /// moderate, 1 Hz poor (CCIR 520).
    pub doppler_hz: f64,
}

/// Open by day (06–18 UTC) with chance `day`, at night with chance `night`.
pub fn diurnal(day: f64, night: f64) -> [f64; 24] {
    std::array::from_fn(|h| if (6..18).contains(&h) { day } else { night })
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub stations: Vec<String>,
    pub paths: Vec<Path>,
    pub days: u64,
    /// Messages are queued over the first this-many hours; the rest of the
    /// run is for delivering them.
    pub traffic_hours: u64,
    pub messages_per_day: usize,
    pub seed: u64,
    /// How often each node takes stock (the daemon: every second).
    pub tick: Millis,
    /// Keep every frame sent, for [`Outcome::airtime`].
    pub log: bool,
}

/// A station's belief that a handoff to a station over a bearer would
/// complete now.
pub type Estimate = (Callsign, &'static str, f64);

/// What happened to one message.
#[derive(Clone, Debug)]
pub struct Sent {
    pub id: ObjectId,
    pub from: usize,
    pub to: usize,
    /// Seconds after the start.
    pub at: u64,
    /// Seconds from queued to stored at the destination.
    pub delivered_after: Option<u64>,
    /// The oracle's earliest possible arrival, seconds after queueing.
    pub possible_after: Option<u64>,
}

pub struct Outcome {
    pub sent: Vec<Sent>,
    pub channel: Stats,
    pub seconds: u64,
    /// Share of path-hours open.
    pub open_share: f64,
    pub trace: u64,
    /// Wall-clock seconds the run took.
    pub wall_secs: f64,
    /// With [`Scenario::log`]: airtime by frame type, sender and addressee:
    /// (frames, milliseconds).
    pub airtime: BTreeMap<(String, String, String), (u64, u64)>,
    /// Each station's belief, at the end, that a handoff to each station it
    /// knows of would complete now.
    pub estimates: Vec<(Callsign, Vec<Estimate>)>,
    /// With [`Scenario::log`]: unicast frames by kind and by what became of
    /// them at the station they were for (`Delivered`, `Corrupted`,
    /// `LostCollision`, ..., or `Unheard`: no path to it then): (frames,
    /// milliseconds).
    pub fates: BTreeMap<(String, String), (u64, u64)>,
    /// With [`Scenario::log`]: how long each key-up lasted (the transmitter
    /// on from its first frame's start to its last frame's end), ms.
    pub keyups: Vec<u64>,
}

impl Outcome {
    pub fn delivered(&self) -> usize {
        self.sent.iter().filter(|s| s.delivered_after.is_some()).count()
    }

    /// Key-up lengths: median, 90th percentile and longest, seconds.
    pub fn keyup_secs(&self) -> (f64, f64, f64) {
        let at = |p: f64| {
            self.keyups
                .get(((self.keyups.len().max(1) - 1) as f64 * p).round() as usize)
                .map_or(0.0, |ms| *ms as f64 / 1_000.0)
        };
        (at(0.5), at(0.9), at(1.0))
    }

    pub fn possible(&self) -> usize {
        self.sent.iter().filter(|s| s.possible_after.is_some()).count()
    }

    /// Share of the channel's time on air.
    pub fn channel_busy(&self) -> f64 {
        self.channel.airtime_ms as f64 / (self.seconds as f64 * 1_000.0)
    }

    /// Delivery latencies in seconds, sorted.
    pub fn latencies(&self) -> Vec<u64> {
        let mut l: Vec<u64> = self.sent.iter().filter_map(|s| s.delivered_after).collect();
        l.sort_unstable();
        l
    }

    /// For messages delivered, the time lost against the oracle's earliest
    /// arrival, in seconds, sorted.
    pub fn excess(&self) -> Vec<u64> {
        let mut e: Vec<u64> = self
            .sent
            .iter()
            .filter_map(|s| Some(s.delivered_after?.saturating_sub(s.possible_after?)))
            .collect();
        e.sort_unstable();
        e
    }

    pub fn summary(&self) -> String {
        let q = |v: &[u64], p: f64| {
            if v.is_empty() {
                "-".to_string()
            } else {
                hours(v[((v.len() - 1) as f64 * p).round() as usize])
            }
        };
        let (lat, excess) = (self.latencies(), self.excess());
        format!(
            "delivered {}/{} ({} possible) | latency p50 {} p90 {} | behind oracle p50 {} p90 {} | channel busy {:.1}% | paths open {:.0}% | {:.1} s wall",
            self.delivered(),
            self.sent.len(),
            self.possible(),
            q(&lat, 0.5),
            q(&lat, 0.9),
            q(&excess, 0.5),
            q(&excess, 0.9),
            100.0 * self.channel_busy(),
            100.0 * self.open_share,
            self.wall_secs,
        )
    }
}

fn hours(secs: u64) -> String {
    if secs < 3600 {
        format!("{:.0} min", secs as f64 / 60.0)
    } else {
        format!("{:.1} h", secs as f64 / 3600.0)
    }
}

fn call(s: &str) -> Callsign {
    Callsign::parse(s).expect("valid callsign")
}

fn identity(seed: u64, i: usize) -> Identity {
    let mut rng = DetRng::from_seed(seed ^ 0x4b45_5953).fork(i as u64);
    Identity::from_secret(std::array::from_fn(|_| rng.below(256) as u8))
}

/// The daemon's settings, for a station that relays.
fn settings(trust: Trust) -> Settings {
    Settings {
        trust,
        costs: Costs::default(),
        retry: RetryPolicy {
            first_delay_secs: 60,
            max_delay_secs: 3600,
            max_attempts: 12,
        },
        receipt_retry: RetryPolicy {
            first_delay_secs: 60,
            max_delay_secs: 3600,
            max_attempts: 24,
        },
        custody_grace_secs: 6 * 3600,
        custody_suspect_secs: 24 * 3600,
        relay: RelaySettings {
            enabled: true,
            ..RelaySettings::default()
        },
        beacon_secs: 600,
        radio_bitrate: 300,
        locator: None,
    }
}

/// When each path is open: `(start, end)` in simulation milliseconds.
fn openings(scenario: &Scenario, rng: &mut DetRng) -> Vec<Vec<(u64, u64)>> {
    let hours = scenario.days * 24;
    scenario
        .paths
        .iter()
        .map(|path| {
            let mut spans = Vec::new();
            let mut open = rng.chance(path.open[0]);
            let mut since = 0;
            for h in 1..hours {
                let next = if rng.chance(path.persistence) {
                    open
                } else {
                    rng.chance(path.open[(h % 24) as usize])
                };
                if next != open {
                    // Openings and closings fall anywhere in the hour.
                    let at = h * HOUR_MS + rng.below(HOUR_MS);
                    if open {
                        spans.push((since, at));
                    }
                    since = at;
                    open = next;
                }
            }
            if open {
                spans.push((since, hours * HOUR_MS));
            }
            spans
        })
        .collect()
}

/// The earliest a message queued at `from` at `at` can reach `to` over
/// `spans`, taking no time per hop (ms), within `ttl`.
fn earliest(
    scenario: &Scenario,
    spans: &[Vec<(u64, u64)>],
    from: usize,
    to: usize,
    at: u64,
    ttl: u64,
) -> Option<u64> {
    let n = scenario.stations.len();
    let mut best = vec![u64::MAX; n];
    let mut done = vec![false; n];
    best[from] = at;
    loop {
        let u = (0..n)
            .filter(|&i| !done[i] && best[i] < u64::MAX)
            .min_by_key(|&i| best[i])?;
        if u == to {
            return (best[u] <= at + ttl).then_some(best[u]);
        }
        done[u] = true;
        for (p, path) in scenario.paths.iter().enumerate() {
            let v = match (path.a == u, path.b == u) {
                (true, _) => path.b,
                (_, true) => path.a,
                _ => continue,
            };
            if let Some(&(start, _)) = spans[p].iter().find(|(_, end)| *end > best[u]) {
                let arrive = start.max(best[u]);
                if arrive < best[v] {
                    best[v] = arrive;
                }
            }
        }
    }
}

pub fn run(scenario: &Scenario) -> Outcome {
    let wall = std::time::Instant::now();
    // HM_LOG=<text>: show the log lines that contain it.
    let filter = std::env::var("HM_LOG").ok();
    hm_node::set_log(move |line| {
        if filter.as_deref().is_some_and(|f| line.contains(f)) {
            eprintln!("{line}");
        }
    });
    let rng = DetRng::from_seed(scenario.seed);
    let spans = openings(scenario, &mut rng.fork(1));
    let mut sim: Sim<Station, StationCmd, StationEvent> = Sim::new(scenario.seed, RadioParams::HF_300);
    let calls: Vec<Callsign> = scenario.stations.iter().map(|s| call(s)).collect();
    let ids: Vec<Identity> = (0..calls.len()).map(|i| identity(scenario.seed, i)).collect();
    let mut trust = Trust::default();
    for (c, id) in calls.iter().zip(&ids) {
        trust.insert(*c, id.public());
    }
    let radio = RadioParams::HF_300;
    for (i, (&me, id)) in calls.iter().zip(&ids).enumerate() {
        let station = Station::new(
            StationSpec {
                me,
                identity: Identity::from_secret(id.secret()),
                timing: LinkTiming {
                    bitrate_bps: radio.bitrate_bps,
                    txdelay_ms: radio.txdelay.0,
                    guard_ms: 1_500,
                    max_rounds: 12,
                    max_keyup_ms: 20_000,
                },
                link_features: FEATURE_COMPACT,
                settings: settings(trust.clone()),
                schedules: Vec::new(),
                epoch: EPOCH,
                seed: scenario.seed.wrapping_mul(1_000).wrapping_add(i as u64),
                tick: scenario.tick,
            },
            Arc::new(Store::in_memory().expect("store in memory")),
        )
        .expect("station");
        let node = sim.add_node(station);
        sim.set_csma(node, 0, Some(Csma::DEFAULT));
    }
    for (p, path) in scenario.paths.iter().enumerate() {
        sim.link(
            path.a,
            path.b,
            Loss::Fading {
                curve: &hm_sim::afsk_1200::CURVE,
                mean_snr_db: path.snr_db,
                doppler_spread_hz: path.doppler_hz,
                rician_k: 0.0,
            },
        );
        let mut open_at = 0;
        let ch = hm_sim::ChannelId(0);
        sim.set_link_at(Millis(0), ch, path.a, path.b, false);
        for &(start, end) in &spans[p] {
            sim.set_link_at(Millis(start.max(open_at)), ch, path.a, path.b, true);
            sim.set_link_at(Millis(end), ch, path.a, path.b, false);
            open_at = end + 1;
        }
    }
    let mut traffic = rng.fork(2);
    let total = scenario.messages_per_day * scenario.traffic_hours as usize / 24;
    let n = calls.len();
    let mut planned = Vec::new();
    for k in 0..total {
        let at = traffic.below(scenario.traffic_hours * HOUR_MS);
        let from = traffic.below(n as u64) as usize;
        let to = (from + 1 + traffic.below(n as u64 - 1) as usize) % n;
        let len = 100 + traffic.below(500) as usize;
        let text: String = std::iter::repeat_n('x', len).collect();
        sim.command_at(
            Millis(at),
            from,
            StationCmd::Send {
                to: calls[to],
                text,
                subject: Some(format!("message {k}")),
                precedence: Precedence::Routine,
            },
        );
        planned.push((at, from, to));
    }
    if scenario.log {
        sim.enable_log();
    }
    let end = scenario.days * DAY_MS;
    sim.run_until(Millis(end));
    let mut airtime = BTreeMap::new();
    // Unicast frames by id: their kind, addressee and airtime, until their
    // fate at the addressee is known.
    let mut unicast: BTreeMap<u64, (String, usize, u64)> = BTreeMap::new();
    let mut fates: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
    // Each station's key-up under way: (first frame's start, last frame's end).
    let mut keyed: BTreeMap<usize, (u64, u64)> = BTreeMap::new();
    let mut keyups = Vec::new();
    for entry in sim.log() {
        match entry {
            hm_sim::LogEntry::Tx {
                id,
                from,
                start,
                end,
                keyup,
                data,
                ..
            } => {
                match keyed.get_mut(from) {
                    Some(on) if !*keyup => on.1 = end.0,
                    _ => {
                        if let Some((first, last)) = keyed.insert(*from, (start.0, end.0)) {
                            keyups.push(last - first);
                        }
                    }
                }
                let (kind, to, dst) = match hm_wire::FrameHeader::decode(data) {
                    Ok((h, _)) => (
                        format!("{:?}", h.ftype),
                        format!("{:?}", h.dst),
                        match h.dst {
                            hm_wire::Dest::Station(c) => calls.iter().position(|call| *call == c),
                            hm_wire::Dest::Broadcast => None,
                        },
                    ),
                    Err(_) => ("?".into(), "?".into(), None),
                };
                // HM_TRACE=A>B: every frame A sends B (seconds into the run).
                let addressee = dst.map_or(to.as_str(), |d| scenario.stations[d].as_str());
                if std::env::var("HM_TRACE")
                    .is_ok_and(|t| t == format!("{}>{addressee}", scenario.stations[*from]))
                {
                    eprintln!("{} {kind} {} bytes", start.0 / 1000, data.len());
                }
                let ms = end.0 - start.0;
                if let Some(dst) = dst {
                    // Unheard unless the addressee's reception says otherwise.
                    unicast.insert(*id, (kind.clone(), dst, ms));
                }
                let e = airtime
                    .entry((kind, scenario.stations[*from].clone(), to))
                    .or_insert((0, 0));
                e.0 += 1;
                e.1 += ms;
            }
            hm_sim::LogEntry::Rx { tx, to, outcome, .. } => {
                if unicast.get(tx).is_some_and(|(_, dst, _)| dst == to) {
                    let (kind, _, ms) = unicast.remove(tx).expect("just found");
                    let e = fates.entry((kind, format!("{outcome:?}"))).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += ms;
                }
            }
            _ => {}
        }
    }
    keyups.extend(keyed.into_values().map(|(first, last)| last - first));
    keyups.sort_unstable();
    for (kind, _, ms) in unicast.into_values() {
        let e = fates.entry((kind, "Unheard".into())).or_insert((0, 0));
        e.0 += 1;
        e.1 += ms;
    }

    let mut queued: BTreeMap<(u64, usize), ObjectId> = BTreeMap::new();
    for (at, node, event) in sim.events() {
        if let StationEvent::Queued { id, .. } = event {
            queued.insert((at.0, *node), *id);
        }
    }
    let ttl = u64::from(hm_node::message::MAIL_TTL) * 1_000;
    let sent = planned
        .iter()
        .filter_map(|&(at, from, to)| {
            let id = *queued.get(&(at, from))?;
            let delivered_after = sim
                .node(to)
                .store()
                .record(id)
                .ok()
                .flatten()
                .filter(|r| r.direction == Direction::In)
                .map(|r| r.at.saturating_sub(EPOCH + at / 1_000));
            let possible_after = earliest(scenario, &spans, from, to, at, ttl).map(|t| (t - at) / 1_000);
            Some(Sent {
                id,
                from,
                to,
                at: at / 1_000,
                delivered_after,
                possible_after,
            })
        })
        .collect();
    let path_hours = scenario.paths.len() as f64 * end as f64;
    let open: u64 = spans.iter().flatten().map(|(s, e)| e - s).sum();
    let report = sim.report();
    Outcome {
        sent,
        channel: report.channels[0].clone(),
        seconds: end / 1_000,
        open_share: open as f64 / path_hours,
        trace: report.trace,
        wall_secs: wall.elapsed().as_secs_f64(),
        airtime,
        fates,
        keyups,
        estimates: sim
            .machines()
            .map(|station| (station.me(), station.node().status(EPOCH + end / 1_000).estimates))
            .collect(),
    }
}

/// Five stations around the Baltic on one HF channel. Near paths open by
/// day most days (NVIS); the long ones mostly at night; two pairs never hear
/// each other and depend on relays.
pub fn baltic(days: u64, seed: u64) -> Scenario {
    let near = |a, b, snr_db| Path {
        a,
        b,
        open: diurnal(0.85, 0.15),
        persistence: 0.7,
        snr_db,
        doppler_hz: 0.5,
    };
    let far = |a, b, snr_db| Path {
        a,
        b,
        open: diurnal(0.2, 0.6),
        persistence: 0.7,
        snr_db,
        doppler_hz: 1.0,
    };
    Scenario {
        // 0 Stockholm, 1 Helsinki, 2 Oslo, 3 Copenhagen, 4 Tallinn.
        stations: ["SM0AAA", "OH2BBB", "LA1CCC", "OZ1DDD", "ES1EEE"]
            .map(String::from)
            .to_vec(),
        paths: vec![
            near(0, 1, 18.0),
            near(0, 2, 16.0),
            near(0, 3, 17.0),
            near(1, 4, 20.0),
            far(2, 3, 14.0),
            far(0, 4, 13.0),
            far(1, 3, 12.0),
        ],
        days,
        traffic_hours: 24 * days.saturating_sub(2).max(1),
        messages_per_day: 12,
        seed,
        tick: Millis(1_000),
        log: false,
    }
}

/// Three stations in a line on paths that stay open: the ends never hear
/// each other, and reach each other only through the middle.
pub fn line(hours: u64, seed: u64) -> Scenario {
    let open = |a, b| Path {
        a,
        b,
        open: [1.0; 24],
        persistence: 1.0,
        snr_db: 20.0,
        doppler_hz: 0.1,
    };
    Scenario {
        stations: ["SM0AAA", "OH2BBB", "LA1CCC"].map(String::from).to_vec(),
        paths: vec![open(0, 1), open(1, 2)],
        days: hours.div_ceil(24),
        traffic_hours: hours / 2,
        messages_per_day: 24,
        seed,
        tick: Millis(10_000),
        log: false,
    }
}

/// `n` stations scattered at random over a square `side_km` wide, sharing
/// one HF channel. Whether two hear each other, and when, follows their
/// distance: near ones by day (NVIS), middling ones some of the time, far
/// ones mostly at night, and none beyond about 1,000 km. Each station sends
/// `per_station_per_day` messages a day to stations picked at random, so the
/// load each station offers stays the same as the network grows.
pub fn scattered(n: usize, side_km: f64, per_station_per_day: usize, days: u64, seed: u64) -> Scenario {
    let mut rng = DetRng::from_seed(seed.wrapping_mul(7_919));
    let unit = |rng: &mut DetRng| rng.below(1_000_000) as f64 / 1_000_000.0;
    let places: Vec<(f64, f64)> = (0..n)
        .map(|_| (unit(&mut rng) * side_km, unit(&mut rng) * side_km))
        .collect();
    let mut paths = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            let ((xa, ya), (xb, yb)) = (places[a], places[b]);
            let km = (xa - xb).hypot(ya - yb);
            let (open, doppler_hz, snr_db) = match km {
                d if d < 250.0 => (diurnal(0.85, 0.15), 0.5, 18.0),
                d if d < 600.0 => (diurnal(0.5, 0.4), 0.7, 15.0),
                d if d < 1_000.0 => (diurnal(0.2, 0.6), 1.0, 12.0),
                _ => continue,
            };
            paths.push(Path {
                a,
                b,
                open,
                persistence: 0.7,
                snr_db: snr_db + unit(&mut rng) * 6.0 - 3.0,
                doppler_hz,
            });
        }
    }
    Scenario {
        stations: (0..n).map(|i| format!("SM{}A{}", i % 10, letters(i))).collect(),
        paths,
        days,
        traffic_hours: 24 * days.saturating_sub(2).max(1),
        messages_per_day: per_station_per_day * n,
        seed,
        tick: Millis(10_000),
        log: false,
    }
}

/// Two letters for station `i`: AA, AB, ...
fn letters(i: usize) -> String {
    let letter = |k: usize| char::from(b'A' + (k % 26) as u8);
    [letter(i / 26 / 10), letter(i / 10)].iter().collect()
}

/// Each station's share of time the channel is busy where it is: its own
/// frames and those of the stations it has a path to (the others reuse the
/// channel without contending). Needs [`Scenario::log`].
pub fn local_busy(scenario: &Scenario, outcome: &Outcome) -> Vec<f64> {
    let mut sent_ms = BTreeMap::new();
    for ((_, from, _), (_, ms)) in &outcome.airtime {
        *sent_ms.entry(from.as_str()).or_insert(0) += ms;
    }
    let seconds = outcome.seconds.max(1) as f64;
    (0..scenario.stations.len())
        .map(|me| {
            let near = scenario
                .paths
                .iter()
                .filter_map(|p| match (p.a == me, p.b == me) {
                    (true, _) => Some(p.b),
                    (_, true) => Some(p.a),
                    _ => None,
                });
            let ms: u64 = std::iter::once(me)
                .chain(near)
                .map(|i| sent_ms.get(scenario.stations[i].as_str()).copied().unwrap_or(0))
                .sum();
            ms as f64 / 1_000.0 / seconds
        })
        .collect()
}
