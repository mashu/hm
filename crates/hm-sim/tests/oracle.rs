//! Randomised scenarios checked against an independent oracle.
//!
//! Each seed builds a random two-channel network with loss, corruption, clock
//! drift, outages and partitions, runs it with logging on, and replays the log
//! through rules written here from scratch. The oracle shares no state with
//! the simulator: it knows only the scenario it built and the log.
//!
//! `HM_SEEDS=10000 cargo test -p hm-sim --release --test oracle` for the nightly run.

use std::collections::{BTreeMap, BTreeSet};

use hm_core::{DetRng, Millis, Port};
use hm_sim::toy::{Beacon, BeaconCmd, BeaconEvent};
use hm_sim::{ChannelId, Clock, Csma, LogEntry, Loss, NodeId, Outcome, RadioParams, Report, Sim, Stats};

type BeaconSim = Sim<Beacon, BeaconCmd, BeaconEvent>;

const RUN: Millis = Millis(2 * 3600 * 1000);

/// Everything the oracle may know: what the scenario builder decided.
struct Scenario {
    channels: Vec<RadioParams>,
    /// (node, channel) -> port
    ports: BTreeMap<(NodeId, ChannelId), Port>,
    /// (channel, from, to) -> (loss is None, corruption probability)
    links: BTreeMap<(ChannelId, NodeId, NodeId), (bool, f64)>,
    /// (node, channel) -> channel access, for radios that have it
    csma: BTreeMap<(NodeId, ChannelId), Csma>,
    nodes: usize,
}

fn random_loss(g: &mut DetRng) -> Loss {
    match g.below(7) {
        0 => Loss::None,
        1 => Loss::Bernoulli(g.next_f64() * 0.3),
        2 => Loss::GilbertElliott {
            p_good_to_bad: 0.02 + g.next_f64() * 0.2,
            p_bad_to_good: 0.05 + g.next_f64() * 0.5,
            loss_good: g.next_f64() * 0.05,
            loss_bad: 0.5 + g.next_f64() * 0.5,
        },
        3 => {
            let mut t = [0.0; 24];
            for h in t.iter_mut() {
                *h = if g.chance(0.4) { 1.0 } else { g.next_f64() * 0.2 };
            }
            Loss::Hourly(t)
        }
        4 => Loss::afsk_1200(4.0 + g.next_f64() * 8.0),
        5 => Loss::Fading {
            curve: &hm_sim::afsk_1200::CURVE,
            mean_snr_db: 8.0 + g.next_f64() * 16.0,
            doppler_spread_hz: 0.05 + g.next_f64() * 2.0,
            rician_k: if g.chance(0.5) { 0.0 } else { g.next_f64() * 10.0 },
        },
        _ => Loss::Bernoulli(0.0),
    }
}

/// Rule: key-up (TXDELAY and TXTAIL) when keying up, then the frame, the
/// per-frame overhead and, on HDLC channels, a 0 after every five 1s sent
/// (bytes least significant bit first), rounded up to a whole millisecond.
fn airtime(p: &RadioParams, data: &[u8], keyup: bool) -> Millis {
    let mut bits = 8 * (data.len() as u64 + p.phy_overhead_bytes as u64);
    if p.hdlc {
        let mut run = 0;
        for bit in data.iter().flat_map(|b| (0..8).map(move |i| b >> i & 1)) {
            run = if bit == 1 { run + 1 } else { 0 };
            if run == 5 {
                bits += 1;
                run = 0;
            }
        }
    }
    let body = (bits * 1000).div_ceil(p.bitrate_bps as u64);
    let key = if keyup { p.txdelay.0 + p.txtail.0 } else { 0 };
    Millis(key + body)
}

fn build_and_run(seed: u64) -> (Scenario, BeaconSim) {
    let mut g = DetRng::from_seed(seed ^ 0x5EED_0F0F);
    let mut sim = BeaconSim::new(seed, RadioParams::VHF_1200);
    let hf_params = RadioParams {
        bitrate_bps: [300, 600, 1200, 2400][g.below(4) as usize],
        txdelay: Millis(50 + g.below(200)),
        txtail: Millis(g.below(30)),
        phy_overhead_bytes: g.below(20) as u32,
        hdlc: g.chance(0.5),
    };
    let hf = sim.add_channel(hf_params);
    let mut sc = Scenario {
        channels: vec![RadioParams::VHF_1200, hf_params],
        ports: BTreeMap::new(),
        links: BTreeMap::new(),
        csma: BTreeMap::new(),
        nodes: 3 + g.below(8) as usize,
    };
    for i in 0..sc.nodes {
        let on_hf = g.chance(0.5);
        let mut m = if g.chance(0.8) {
            Beacon::periodic(
                i as u8,
                5 + g.below(80) as usize,
                Millis(5_000 + g.below(55_000)),
                g.below(5_000),
                sim.machine_rng(i as u64),
            )
        } else {
            Beacon::silent(i as u8, 5 + g.below(80) as usize)
        };
        let mut radios = vec![(0u8, ChannelId(0))];
        if on_hf {
            m = m.on_ports(&[0, 1]);
            radios.push((1, hf));
        }
        let id = sim.add_node_on(m, &radios);
        for (p, ch) in radios {
            sc.ports.insert((id, ch), p);
            if g.chance(0.4) {
                let c = Csma {
                    persist: g.below(256) as u8,
                    slot: Millis(20 + g.below(200)),
                    dcd_delay: Millis(g.below(200)),
                };
                sim.set_csma(id, p, Some(c));
                sc.csma.insert((id, ch), c);
            }
        }
        if g.chance(0.3) {
            sim.set_clock(
                id,
                Clock {
                    offset: Millis(g.below(10_000)),
                    ppm: g.below(2001) as i32 - 1000,
                },
            );
        }
    }
    for ch in [ChannelId(0), hf] {
        let members: Vec<NodeId> = (0..sc.nodes)
            .filter(|&n| sc.ports.contains_key(&(n, ch)))
            .collect();
        for (i, &a) in members.iter().enumerate() {
            for &b in &members[i + 1..] {
                if !g.chance(0.6) {
                    continue;
                }
                let loss = random_loss(&mut g);
                let lossless = loss == Loss::None;
                let one_way = g.chance(0.2);
                if one_way {
                    sim.link_one_way_on(ch, a, b, loss);
                    sc.links.insert((ch, a, b), (lossless, 0.0));
                } else {
                    sim.link_on(ch, a, b, loss);
                    sc.links.insert((ch, a, b), (lossless, 0.0));
                    sc.links.insert((ch, b, a), (lossless, 0.0));
                }
                if g.chance(0.2) {
                    sim.set_corruption(ch, a, b, 0.05);
                    sc.links.get_mut(&(ch, a, b)).unwrap().1 = 0.05;
                }
            }
        }
        for _ in 0..g.below(3) {
            let (side_a, side_b): (Vec<NodeId>, Vec<NodeId>) = members.iter().partition(|_| g.chance(0.5));
            let t = Millis(g.below(RUN.0));
            sim.partition_at(t, ch, &side_a, &side_b);
            sim.heal_at(t + Millis(g.below(1_800_000)), ch, &side_a, &side_b);
        }
    }
    for _ in 0..g.below(6) {
        let n = g.below(sc.nodes as u64) as usize;
        let t = Millis(g.below(RUN.0));
        sim.set_up_at(t, n, false);
        sim.set_up_at(t + Millis(g.below(600_000)), n, true);
    }
    for _ in 0..g.below(60) {
        let n = g.below(sc.nodes as u64) as usize;
        sim.command_at(Millis(g.below(RUN.0)), n, BeaconCmd::SendNow);
    }
    sim.enable_log();
    sim.run_until(RUN);
    (sc, sim)
}

struct TxRec {
    channel: ChannelId,
    from: NodeId,
    start: Millis,
    end: Millis,
    /// Start of the key-up the frame went out in.
    keyed_at: Millis,
    digest: u64,
}

/// Replays the log against the rules; returns a description of the first violation.
fn check(
    sc: &Scenario,
    log: &[LogEntry],
    report: &Report,
    heard: &BTreeMap<NodeId, u64>,
) -> Result<(), String> {
    let mut up = vec![true; sc.nodes];
    let mut enabled: BTreeMap<(ChannelId, NodeId, NodeId), bool> =
        sc.links.keys().map(|k| (*k, true)).collect();
    let mut txs: BTreeMap<u64, TxRec> = BTreeMap::new();
    let mut radio_busy_until: BTreeMap<(NodeId, ChannelId), Millis> = BTreeMap::new();
    let mut radio_keyed_at: BTreeMap<(NodeId, ChannelId), Millis> = BTreeMap::new();
    let mut radio_last_queued: BTreeMap<(NodeId, ChannelId), Millis> = BTreeMap::new();
    let mut time = Millis::ZERO;
    let mut current_eval: Option<(u64, BTreeSet<NodeId>)> = None;
    let mut evaluated: BTreeSet<u64> = BTreeSet::new();
    let mut unevaluated: BTreeMap<u64, Millis> = BTreeMap::new();
    let mut max_air = Millis::ZERO;
    let mut got = vec![Stats::default(); sc.channels.len()];
    let mut delivered_to: BTreeMap<NodeId, u64> = BTreeMap::new();

    let overlaps =
        |t: &TxRec, ch: ChannelId, s: Millis, e: Millis| t.channel == ch && t.start < e && s < t.end;
    let close_eval = |cur: &mut Option<(u64, BTreeSet<NodeId>)>| -> Result<(), String> {
        if let Some((tx, remaining)) = cur.take() {
            if !remaining.is_empty() {
                return Err(format!("tx {tx}: no outcome for listeners {remaining:?}"));
            }
        }
        Ok(())
    };

    for (i, e) in log.iter().enumerate() {
        let at = match e {
            LogEntry::Tx { queued_at, .. } => *queued_at,
            LogEntry::Eval { at, .. }
            | LogEntry::Rx { at, .. }
            | LogEntry::Up { at, .. }
            | LogEntry::Link { at, .. } => *at,
        };
        if !matches!(e, LogEntry::Rx { .. }) {
            close_eval(&mut current_eval)?;
        }
        if at < time {
            return Err(format!("entry {i}: time went backwards {at:?} < {time:?}"));
        }
        time = at;
        match *e {
            LogEntry::Up { node, up: u, .. } => up[node] = u,
            LogEntry::Link {
                channel,
                from,
                to,
                enabled: en,
                ..
            } => {
                *enabled
                    .get_mut(&(channel, from, to))
                    .ok_or(format!("entry {i}: toggled unknown link"))? = en;
            }
            LogEntry::Tx {
                id,
                channel,
                from,
                port,
                asked_at,
                queued_at,
                start,
                end,
                keyup,
                len,
                digest,
                ref data,
            } => {
                if sc.ports.get(&(from, channel)) != Some(&port) {
                    return Err(format!(
                        "tx {id}: station {from} has no port {port} on {channel:?}"
                    ));
                }
                let busy = radio_busy_until.entry((from, channel)).or_default();
                // Rule: a frame queued while the radio still transmits follows the previous
                // one directly, without TXDELAY; otherwise it keys up at once.
                if keyup != (*busy <= queued_at) {
                    return Err(format!(
                        "tx {id}: keyup {keyup}, radio busy until {busy:?}, queued at {queued_at:?}"
                    ));
                }
                let params = sc.channels[channel.0];
                if data.len() != len {
                    return Err(format!(
                        "tx {id}: logged {} bytes for a {len}-byte frame",
                        data.len()
                    ));
                }
                let expect_start = if keyup { queued_at } else { *busy };
                let expect_air = airtime(&params, data, keyup);
                if start != expect_start || end.0 - start.0 != expect_air.0 {
                    return Err(format!(
                        "tx {id}: {start:?}..{end:?}, expected start {expect_start:?}, airtime {} ms",
                        expect_air.0
                    ));
                }
                *busy = end;
                let keyed_at = *radio_keyed_at
                    .entry((from, channel))
                    .and_modify(|k| {
                        if keyup {
                            *k = start
                        }
                    })
                    .or_insert(start);
                let last_queued = radio_last_queued.insert((from, channel), queued_at);
                match sc.csma.get(&(from, channel)) {
                    // Rule: without channel access the radio takes a frame when asked.
                    None if asked_at != queued_at => {
                        return Err(format!(
                            "tx {id}: asked at {asked_at:?}, queued at {queued_at:?} without CSMA"
                        ));
                    }
                    None => {}
                    Some(c) => {
                        if asked_at > queued_at {
                            return Err(format!(
                                "tx {id}: queued at {queued_at:?} before asked at {asked_at:?}"
                            ));
                        }
                        // Rule: key up only when no station this one hears has
                        // been keyed up for the carrier-detect delay.
                        if keyup {
                            let heard = txs.values().find(|t| {
                                t.channel == channel
                                    && t.from != from
                                    && t.start <= queued_at
                                    && queued_at < t.end
                                    && t.keyed_at + c.dcd_delay <= queued_at
                                    && enabled.get(&(channel, t.from, from)) == Some(&true)
                            });
                            if let Some(t) = heard {
                                return Err(format!(
                                    "tx {id}: station {from} keyed up at {queued_at:?} over station {}'s carrier since {:?}",
                                    t.from, t.keyed_at
                                ));
                            }
                        } else if asked_at < queued_at && last_queued != Some(queued_at) {
                            // Rule: frames that waited for the channel go out together.
                            return Err(format!(
                                "tx {id}: waited for the channel but left in another key-up"
                            ));
                        }
                    }
                }
                let s = &mut got[channel.0];
                s.frames_sent += 1;
                s.bytes_sent += len as u64;
                s.airtime_ms += end.0 - start.0;
                max_air = max_air.max(Millis(end.0 - start.0));
                unevaluated.insert(id, end);
                txs.insert(
                    id,
                    TxRec {
                        channel,
                        from,
                        start,
                        end,
                        keyed_at,
                        digest,
                    },
                );
            }
            LogEntry::Eval { tx, at } => {
                let t = txs.get(&tx).ok_or(format!("eval of unknown tx {tx}"))?;
                if at != t.end {
                    return Err(format!("tx {tx}: evaluated at {at:?}, ends at {:?}", t.end));
                }
                let (t_channel, t_from) = (t.channel, t.from);
                if !evaluated.insert(tx) {
                    return Err(format!("tx {tx} evaluated twice"));
                }
                unevaluated.remove(&tx);
                // A transmission that ended more than one maximum airtime ago cannot
                // overlap anything evaluated from now on.
                txs.retain(|id, o| *id == tx || o.end + max_air > at);
                let listeners: BTreeSet<NodeId> = enabled
                    .iter()
                    .filter(|(&(ch, from, _), &on)| on && ch == t_channel && from == t_from)
                    .map(|(&(_, _, to), _)| to)
                    .collect();
                current_eval = Some((tx, listeners));
            }
            LogEntry::Rx {
                tx,
                channel,
                to,
                port,
                at,
                outcome,
                digest,
            } => {
                let t = txs.get(&tx).ok_or(format!("rx of unknown tx {tx}"))?;
                let (cur_tx, remaining) = current_eval.as_mut().ok_or("rx outside an evaluation")?;
                if *cur_tx != tx || !remaining.remove(&to) {
                    return Err(format!("tx {tx}: unexpected outcome at station {to}"));
                }
                if channel != t.channel || sc.ports.get(&(to, channel)) != Some(&port) || at != t.end {
                    return Err(format!("tx {tx}: wrong channel, port or time at {to}"));
                }
                let own = txs
                    .values()
                    .any(|o| o.from == to && overlaps(o, channel, t.start, t.end));
                let collision = txs.iter().any(|(&oid, o)| {
                    oid != tx
                        && o.from != t.from
                        && o.from != to
                        && overlaps(o, channel, t.start, t.end)
                        && enabled.get(&(channel, o.from, to)) == Some(&true)
                });
                let expected_class = if !up[to] {
                    Some(Outcome::LostDown)
                } else if own {
                    Some(Outcome::LostHalfDuplex)
                } else if collision {
                    Some(Outcome::LostCollision)
                } else {
                    None
                };
                match (expected_class, outcome) {
                    (Some(x), o) if x != o => {
                        return Err(format!("tx {tx} at {to}: {o:?}, oracle says {x:?}"))
                    }
                    (None, Outcome::LostDown | Outcome::LostHalfDuplex | Outcome::LostCollision) => {
                        return Err(format!("tx {tx} at {to}: {outcome:?} without cause"))
                    }
                    _ => {}
                }
                let (lossless, corrupt) = sc.links[&(channel, t.from, to)];
                if expected_class.is_none() && lossless && corrupt == 0.0 && outcome != Outcome::Delivered {
                    return Err(format!("tx {tx} at {to}: {outcome:?} on a perfect link"));
                }
                let same_bytes = digest == t.digest;
                match outcome {
                    Outcome::Corrupted if same_bytes => {
                        return Err(format!("tx {tx}: corruption left bytes unchanged"))
                    }
                    Outcome::Corrupted if corrupt == 0.0 => {
                        return Err(format!("tx {tx}: corruption on a clean link"))
                    }
                    Outcome::Delivered | Outcome::LostChannel if !same_bytes => {
                        return Err(format!("tx {tx}: bytes changed without corruption"))
                    }
                    _ => {}
                }
                let s = &mut got[channel.0];
                match outcome {
                    Outcome::Delivered => s.delivered += 1,
                    Outcome::Corrupted => s.corrupted += 1,
                    Outcome::LostChannel => s.lost_channel += 1,
                    Outcome::LostCollision => s.lost_collision += 1,
                    Outcome::LostHalfDuplex => s.lost_half_duplex += 1,
                    Outcome::LostDown => s.lost_down += 1,
                }
                if matches!(outcome, Outcome::Delivered | Outcome::Corrupted) {
                    *delivered_to.entry(to).or_default() += 1;
                }
            }
        }
    }
    close_eval(&mut current_eval)?;
    for (id, end) in &unevaluated {
        if *end <= report.now {
            return Err(format!("tx {id} ended at {end:?} but was never evaluated"));
        }
    }
    for (ch, (mine, theirs)) in got.iter().zip(&report.channels).enumerate() {
        let mut theirs = theirs.clone();
        theirs.airtime = Default::default();
        if *mine != theirs {
            return Err(format!("channel {ch}: log totals {mine:?} != report {theirs:?}"));
        }
    }
    if &delivered_to != heard {
        return Err(format!(
            "frames handed to stations {delivered_to:?} != events {heard:?}"
        ));
    }
    Ok(())
}

fn seeds() -> u64 {
    std::env::var("HM_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
}

#[test]
fn random_scenarios_obey_the_channel_rules() {
    let mut totals = Stats::default();
    for seed in 0..seeds() {
        let (sc, sim) = build_and_run(seed);
        let report = sim.report();
        let mut heard: BTreeMap<NodeId, u64> = BTreeMap::new();
        for (_, n, _) in sim.events() {
            *heard.entry(*n).or_default() += 1;
        }
        if let Err(msg) = check(&sc, sim.log(), &report, &heard) {
            panic!("seed {seed}: {msg}");
        }
        let t = report.total();
        totals.frames_sent += t.frames_sent;
        totals.delivered += t.delivered;
        totals.corrupted += t.corrupted;
        totals.lost_channel += t.lost_channel;
        totals.lost_collision += t.lost_collision;
        totals.lost_half_duplex += t.lost_half_duplex;
        totals.lost_down += t.lost_down;
        if seed % 8 == 0 {
            let (_, again) = build_and_run(seed);
            assert_eq!(again.report(), report, "seed {seed}: second run differs");
            assert_eq!(again.log(), sim.log(), "seed {seed}: second log differs");
        }
    }
    // Every rule was actually exercised, not just vacuously satisfied.
    assert!(
        totals.delivered > 0
            && totals.corrupted > 0
            && totals.lost_channel > 0
            && totals.lost_collision > 0
            && totals.lost_half_duplex > 0
            && totals.lost_down > 0,
        "{totals:?}"
    );
    eprintln!("{} seeds, totals: {totals:?}", seeds());
}
