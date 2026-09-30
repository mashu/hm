//! hm-xfer in the simulator, measured against the Phase 1 exit criteria.
//!
//! `HM_XFER_TRIALS=5000 cargo test -p hm-xfer --release --test sim -- --nocapture`

use hm_core::{Millis, Output};
use hm_ident::Identity;
use hm_sim::metrics::Percentiles;
use hm_sim::{ChannelId, Csma, Loss, RadioParams, Report, Sim};
use hm_wire::Callsign;
use hm_xfer::{object_id, Command, Config, Event, Receipt, Xfer};

mod bulletins;
mod hf;
mod one_link;
mod shared_channel;

/// A fixed key per callsign; every station trusts every other one.
fn identity(me: &str) -> Identity {
    let mut secret = [0x5Au8; 32];
    secret[..8].copy_from_slice(&Callsign::parse(me).unwrap().packed().to_be_bytes());
    Identity::from_secret(secret)
}

fn station(me: &str, rng: hm_core::DetRng, peers: &[&str]) -> Xfer {
    let mut x = Xfer::new(Config::vhf_1200(call(me)), identity(me), rng).unwrap();
    for p in peers {
        x.trust(call(p), identity(p).public());
    }
    x
}

type XferSim = Sim<Xfer, Command, Event>;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn trials() -> u64 {
    std::env::var("HM_XFER_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300)
}

struct Outcome {
    received: usize,
    delivered: bool,
    failed: bool,
    latency: Option<Millis>,
    report: Report,
}

fn object(len: usize, seed: u64) -> Vec<u8> {
    let mut g = hm_core::DetRng::from_seed(seed);
    (0..len).map(|_| g.next_u64() as u8).collect()
}

/// One transfer SA0KAM -> SO5KM-1 on a single 1200 bd channel.
fn run(seed: u64, len: usize, forward: Loss, back: Loss, corrupt: f64, limit: Millis) -> Outcome {
    let mut sim = XferSim::new(seed, RadioParams::VHF_1200);
    let a_rng = sim.machine_rng(0);
    let b_rng = sim.machine_rng(1);
    let a = sim.add_node(station("SA0KAM", a_rng, &["SO5KM-1"]));
    let b = sim.add_node(station("SO5KM-1", b_rng, &["SA0KAM"]));
    sim.link_one_way(a, b, forward);
    sim.link_one_way(b, a, back);
    if corrupt > 0.0 {
        sim.set_corruption(ChannelId(0), a, b, corrupt);
    }
    let obj = object(len, seed);
    let id = object_id(&obj);
    sim.command_at(
        Millis(0),
        a,
        Command::Send {
            to: call("SO5KM-1"),
            object: obj.clone(),
            precedence: 0,
        },
    );
    sim.run_until(limit);
    let mut o = Outcome {
        received: 0,
        delivered: false,
        failed: false,
        latency: None,
        report: sim.report(),
    };
    for (t, n, e) in sim.events() {
        match e {
            Event::Received { object, id: got, .. } if *n == b => {
                assert_eq!((object, got), (&obj, &id), "seed {seed}: wrong bytes delivered");
                o.received += 1;
                o.latency = Some(*t);
            }
            Event::Delivered { receipt, .. } if *n == a => {
                assert_eq!(*receipt, Receipt::Verified, "seed {seed}: receipt not verified");
                o.delivered = true
            }
            Event::Failed { .. } if *n == a => o.failed = true,
            _ => {}
        }
    }
    let _ = Output::<Event>::Event;
    o
}

/// One 2 kB transfer on a 300 bd HF path fading at `spread` Hz around
/// `snr_db`, with `cfg` at both ends: (delivered, latency, airtime of both).
fn hf_run(seed: u64, cfg: &dyn Fn(&str) -> Config, snr_db: f64, spread: f64) -> (bool, Millis, Millis) {
    hf_run_len(seed, 2000, cfg, snr_db, spread)
}

fn hf_run_len(
    seed: u64,
    len: usize,
    cfg: &dyn Fn(&str) -> Config,
    snr_db: f64,
    spread: f64,
) -> (bool, Millis, Millis) {
    let mut sim = XferSim::new(seed, RadioParams::HF_300);
    let [ra, rb] = [sim.machine_rng(0), sim.machine_rng(1)];
    let make = |me: &str, rng, peer: &str| {
        let mut x = Xfer::new(cfg(me), identity(me), rng).unwrap();
        x.trust(call(peer), identity(peer).public());
        x
    };
    let a = sim.add_node(make("SA0KAM", ra, "SO5KM-1"));
    let b = sim.add_node(make("SO5KM-1", rb, "SA0KAM"));
    sim.link(
        a,
        b,
        Loss::Fading {
            curve: &hm_sim::afsk_1200::CURVE,
            mean_snr_db: snr_db,
            doppler_spread_hz: spread,
            rician_k: 0.0,
        },
    );
    sim.command_at(
        Millis(0),
        a,
        Command::Send {
            to: call("SO5KM-1"),
            object: object(len, seed),
            precedence: 0,
        },
    );
    sim.run_until(Millis::from_secs(3 * 3600));
    let done = sim.events().iter().find_map(|(t, n, e)| match e {
        Event::Delivered { .. } if *n == a => Some(*t),
        _ => None,
    });
    let air = Millis(sim.report().channels[0].airtime_ms);
    (done.is_some(), done.unwrap_or(Millis::ZERO), air)
}
