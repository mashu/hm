use super::*;
use alloc::vec;
use hm_wire::{Ack, CloseReason, DataPreamble, DATA_PREAMBLE_LEN, HEADER_LEN, NEED_OFFER};

mod broadcast;
mod delivery;
mod pacing;
mod receipts;
mod sessions;
mod sharing;
mod symbols;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

/// A fixed key per callsign, so tests can trust each other's keys.
fn identity(me: &str) -> Identity {
    let mut secret = [0x5Au8; 32];
    secret[..8].copy_from_slice(&call(me).packed().to_be_bytes());
    Identity::from_secret(secret)
}

fn engine(me: &str) -> Xfer {
    let mut cfg = Config::vhf_1200(call(me));
    cfg.duty_cycle_permille = 1000;
    // These tests look at OFFER, DATA and ACK frames one by one; sessions
    // (OPEN before the first over) have tests of their own.
    cfg.sessions = false;
    Xfer::new(cfg, identity(me), DetRng::from_seed(1)).unwrap()
}

fn frames(out: &[Output<Event>]) -> Vec<Vec<u8>> {
    out.iter()
        .filter_map(|o| match o {
            Output::Transmit { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect()
}

/// What happened to transfers, without the loss observations reported along
/// the way (see [`overs`]).
fn events(out: &[Output<Event>]) -> Vec<Event> {
    out.iter()
        .filter_map(|o| match o {
            Output::Event(Event::Over { .. }) => None,
            Output::Event(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

/// The loss observations reported: (to, sent, got).
fn overs(out: &[Output<Event>]) -> Vec<(Callsign, u32, u32)> {
    out.iter()
        .filter_map(|o| match o {
            Output::Event(Event::Over { to, sent, got }) => Some((*to, *sent, *got)),
            _ => None,
        })
        .collect()
}

/// Deliver `fs` to `rx` in order, then run its deadlines until quiet; returns its outputs.
fn deliver(rx: &mut Xfer, now: Millis, fs: &[Vec<u8>]) -> Vec<Output<Event>> {
    let mut out = Vec::new();
    for f in fs {
        rx.handle(
            now,
            Input::Frame {
                port: 0,
                data: f.clone(),
            },
            &mut out,
        );
    }
    let t = rx.next_deadline().unwrap();
    rx.on_deadline(t, &mut out);
    out
}

#[test]
fn rejects_bad_config() {
    let mut cfg = Config::vhf_1200(call("SA0KAM"));
    cfg.symbol_size = 100;
    assert!(Xfer::new(cfg, identity("SA0KAM"), DetRng::from_seed(0)).is_err());
}

fn session_engine(me: &str, tweak: impl FnOnce(&mut Config)) -> Xfer {
    let mut cfg = Config::vhf_1200(call(me));
    cfg.duty_cycle_permille = 1000;
    tweak(&mut cfg);
    Xfer::new(cfg, identity(me), DetRng::from_seed(1)).unwrap()
}

fn send(x: &mut Xfer, now: Millis, to: &str, object: Vec<u8>) -> Vec<Output<Event>> {
    let mut out = Vec::new();
    x.handle(
        now,
        Input::Command(Command::Send {
            to: call(to),
            object,
            precedence: 0,
        }),
        &mut out,
    );
    out
}

fn ctrl_type(f: &[u8]) -> Option<u8> {
    let (h, p) = FrameHeader::decode(f).unwrap();
    (h.ftype == FrameType::Ctrl).then(|| p[0])
}
