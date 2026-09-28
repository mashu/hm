//! Radio bearer thread: KISS / built-in modem, transfer engine, beacons.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use hm_core::{DetRng, Input, Machine, Millis, Output};
use hm_wire::{
    Callsign, Dest, FrameHeader, FrameType, ObjectId, FEATURE_MAILBOX, FEATURE_RELAY, FLAG_INTERNET,
    FLAG_MAILBOX, FLAG_RELAY,
};
use hm_xfer::beacon::{beacon_frame, read_beacon};
use hm_xfer::{Command, Event, Failure, Receipt};

use super::control::{ControlBudget, CONTROL_BUDGET_WINDOW_MS};
use super::heard;
use super::live::LiveConfig;
use super::types::{log, NodeConfig, RadioConfig, RadioLink};
use crate::config::RadioSettings;
use crate::driver::Link;
use crate::files::Trust;
use crate::kiss_link::KissLink;
use crate::sound_link::SoundLink;
use crate::station::{unix_now, Station};

pub(crate) enum RadioCmd {
    Send {
        object: Vec<u8>,
        to: Callsign,
        precedence: u8,
    },
    /// RF bulletin: `Dest::Broadcast`, no ACK wait.
    Broadcast {
        object: Vec<u8>,
        precedence: u8,
    },
    Accept {
        from: Callsign,
        xfer_id: ObjectId,
        accepted: bool,
        retry_after: u16,
    },
    Sync {
        to: Dest,
        payload: Vec<u8>,
    },
}

pub(crate) enum RadioEvt {
    /// The radio link now in use (`None`: the radio is off).
    Using(Option<String>),
    Up,
    Down(String),
    Received {
        from: Callsign,
        xfer_id: ObjectId,
        object: Vec<u8>,
    },
    Delivered {
        xfer_id: ObjectId,
        to: Callsign,
        receipt: Receipt,
    },
    Failed {
        xfer_id: ObjectId,
        to: Callsign,
        reason: Failure,
    },
    Sync {
        from: Callsign,
        payload: Vec<u8>,
    },
    Heard(Vec<heard::Station>),
}

const RECONNECT: Duration = Duration::from_secs(5);
/// The first beacon goes out at a random moment in this window after the radio
/// comes up (or between half and one beacon interval, if that is shorter), so
/// stations started together do not all beacon at once.
const FIRST_BEACON_MS: (u64, u64) = (5_000, 30_000);
/// Heard-table updates reach the status API at least this often.
const HEARD_REPORT: Duration = Duration::from_secs(30);
const MAX_WAIT: Duration = Duration::from_millis(200);

/// How a radio session ended.
pub(crate) enum Ended {
    Stopped,
    /// `[radio]` changed: open the link it describes now.
    Reconfigure,
}

/// Keep the radio link up and run the transfer engine on it; open a new link
/// when `[radio]` changes, if the node has a [`RadioBuilder`].
pub(crate) fn radio_thread(
    cfg: &NodeConfig,
    live: &LiveConfig,
    cmds: mpsc::Receiver<RadioCmd>,
    events: tokio::sync::mpsc::UnboundedSender<RadioEvt>,
    stop: &AtomicBool,
) {
    // The link in use: the one the node started with until [radio] changes.
    let mut rebuilt: Option<Option<RadioConfig>> = None;
    let mut settings = live.get().radio.link_settings();
    let changed = |settings: &RadioSettings| {
        cfg.radio_builder.is_some() && live.get().radio.link_settings() != *settings
    };
    while !stop.load(Ordering::Relaxed) {
        let rc = match &rebuilt {
            Some(r) => r.as_ref(),
            None => cfg.radio.as_ref(),
        };
        let _ = events.send(RadioEvt::Using(rc.map(|r| r.describe())));
        // Radio off: idle until [radio] is enabled. Do not emit Down or reconnect.
        let Some(rc) = rc else {
            while !stop.load(Ordering::Relaxed) && !changed(&settings) {
                while cmds.try_recv().is_ok() {}
                thread::sleep(Duration::from_millis(200));
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if changed(&settings) {
                let now = live.get().radio;
                settings = now.link_settings();
                match cfg.radio_builder.as_ref().expect("checked by changed")(&now) {
                    Ok(r) => rebuilt = Some(r),
                    Err(e) => log(format!(
                        "radio settings not applied, keeping the link in use: {e}"
                    )),
                }
            }
            continue;
        };
        let session = |rc: &RadioConfig, link: &mut dyn Link| {
            let _ = events.send(RadioEvt::Up);
            radio_session(cfg, rc, live, &settings, link, &cmds, &events, stop)
        };
        let result = match &rc.link {
            RadioLink::Kiss {
                target,
                tnc_port,
                params,
            } => KissLink::open(target, cfg.me, *tnc_port, *params)
                .map(|mut l| session(rc, &mut l))
                .map_err(|e| format!("cannot open {}: {e}", rc.describe())),
            RadioLink::Modem { audio, ptt, csma, .. } => {
                SoundLink::start(cfg.me, audio.clone(), ptt.clone(), *csma)
                    .map(|mut l| session(rc, &mut l))
                    .map_err(|e| format!("cannot open {}: {e}", rc.describe()))
            }
        };
        let wait = match result {
            Ok(Ok(Ended::Stopped)) => return,
            Ok(Ok(Ended::Reconfigure)) => {
                let _ = events.send(RadioEvt::Down("the radio settings changed".into()));
                false
            }
            Ok(Err(e)) => {
                let _ = events.send(RadioEvt::Down(e.to_string()));
                true
            }
            Err(e) => {
                let _ = events.send(RadioEvt::Down(e));
                true
            }
        };
        // Wait to reconnect, unless [radio] changes.
        let until = Instant::now() + RECONNECT;
        while wait && Instant::now() < until && !changed(&settings) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            // Commands sent while the radio is down are answered by RadioEvt::Down.
            while cmds.try_recv().is_ok() {}
            thread::sleep(Duration::from_millis(100));
        }
        if changed(&settings) {
            let now = live.get().radio;
            settings = now.link_settings();
            match cfg.radio_builder.as_ref().expect("checked by changed")(&now) {
                Ok(r) => rebuilt = Some(r),
                Err(e) => log(format!(
                    "radio settings not applied, keeping the link in use: {e}"
                )),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn radio_session(
    cfg: &NodeConfig,
    rc: &RadioConfig,
    live: &LiveConfig,
    settings: &RadioSettings,
    link: &mut dyn Link,
    cmds: &mpsc::Receiver<RadioCmd>,
    events: &tokio::sync::mpsc::UnboundedSender<RadioEvt>,
    stop: &AtomicBool,
) -> io::Result<Ended> {
    let mut live_version = live.version();
    let station = Station {
        key: &cfg.key,
        trust: &live.get().trust,
        me: cfg.me,
        timing: rc.timing,
    };
    let mut x = station.engine()?;
    let mut features = link.features();
    if cfg.relay.enabled {
        features |= FEATURE_RELAY;
    }
    if cfg.relay.mailbox {
        features |= FEATURE_MAILBOX;
    }
    x.set_features(features);
    x.set_application_ack(true);
    let port = x.config().port;
    let start = Instant::now();
    let now = || Millis(start.elapsed().as_millis() as u64);
    let mut out: Vec<Output<Event>> = Vec::new();
    let mut rng = DetRng::from_seed(getrandom::u64().unwrap_or(0xBEAC));
    let mut flags = if cfg.internet.is_some() { FLAG_INTERNET } else { 0 };
    if cfg.relay.enabled {
        flags |= FLAG_RELAY;
    }
    if cfg.relay.mailbox {
        flags |= FLAG_MAILBOX;
    }
    let mut heard = heard::HeardTable::default();
    let beacon_every = |secs: u64| (secs > 0).then(|| Duration::from_secs(secs));
    let mut beacon_secs = live.get().beacon_secs;
    let mut next_beacon = beacon_every(beacon_secs).map(|every| {
        let every = every.as_millis() as u64;
        let (lo, hi) = (FIRST_BEACON_MS.0.min(every / 2), FIRST_BEACON_MS.1.min(every));
        start + Duration::from_millis(lo + rng.below(hi - lo + 1))
    });
    let mut heard_changed = false;
    let mut last_report = Instant::now();
    let control_permyriad = (cfg.relay.control_airtime_fraction * 10_000.0)
        .round()
        .clamp(0.0, 10_000.0) as u16;
    let mut control_budget =
        ControlBudget::new(CONTROL_BUDGET_WINDOW_MS, control_permyriad).map_err(io::Error::other)?;
    let mut sync_queue = VecDeque::<(Dest, Vec<u8>)>::new();
    loop {
        if live.version() != live_version {
            live_version = live.version();
            let live = live.get();
            if cfg.radio_builder.is_some() && live.radio.link_settings() != *settings {
                return Ok(Ended::Reconfigure);
            }
            x.set_trust(live.trust.iter());
            if live.beacon_secs != beacon_secs {
                // A new interval: the next beacon one (new) interval from now, or none.
                beacon_secs = live.beacon_secs;
                next_beacon = beacon_every(beacon_secs).map(|e| Instant::now() + e);
            }
        }
        if let (Some(at), Some(every)) = (next_beacon, beacon_every(beacon_secs)) {
            if Instant::now() >= at {
                let t = unix_now();
                let locator = live.get().locator;
                let frame = beacon_frame(
                    &cfg.key.identity,
                    cfg.me,
                    flags,
                    t as u32,
                    locator,
                    heard.for_beacon(t),
                )
                .map_err(|e| io::Error::other(format!("beacon: {e}")))?;
                link.send(&frame)?;
                let jitter = every.as_millis() as u64 / 10;
                let ms = every.as_millis() as u64 - jitter + rng.below(2 * jitter + 1);
                next_beacon = Some(Instant::now() + Duration::from_millis(ms));
            }
        }
        if heard_changed || last_report.elapsed() >= HEARD_REPORT {
            heard.expire(unix_now());
            let _ = events.send(RadioEvt::Heard(heard.list()));
            heard_changed = false;
            last_report = Instant::now();
        }
        while let Ok(command) = cmds.try_recv() {
            match command {
                RadioCmd::Send {
                    object,
                    to,
                    precedence,
                } => x.handle(
                    now(),
                    Input::Command(Command::Send {
                        to,
                        object,
                        precedence,
                    }),
                    &mut out,
                ),
                RadioCmd::Broadcast { object, precedence } => x.handle(
                    now(),
                    Input::Command(Command::Broadcast { object, precedence }),
                    &mut out,
                ),
                RadioCmd::Accept {
                    from,
                    xfer_id,
                    accepted,
                    retry_after,
                } => x.handle(
                    now(),
                    Input::Command(Command::Accept {
                        from,
                        id: xfer_id,
                        accepted,
                        retry_after,
                    }),
                    &mut out,
                ),
                RadioCmd::Sync { to, payload } if sync_queue.len() < 64 => {
                    sync_queue.push_back((to, payload));
                }
                RadioCmd::Sync { .. } => {}
            }
        }
        if let Some((to, payload)) = sync_queue.front() {
            let frame = FrameHeader {
                ftype: FrameType::Sync,
                src: cfg.me,
                dst: *to,
                session: 0,
                index: 0,
            }
            .frame(payload)
            .map_err(|error| io::Error::other(error.to_string()))?;
            let airtime_ms = rc.timing.txdelay_ms.saturating_add(
                ((frame.len() as u64 + 24) * 8 * 1_000).div_ceil(u64::from(rc.timing.bitrate_bps)),
            );
            if control_budget.admit(now().0, airtime_ms) {
                link.send(&frame)?;
                sync_queue.pop_front();
            }
        }
        for o in out.drain(..) {
            match o {
                Output::Transmit { port: p, data } if p == port => {
                    // Tell the link what the destination decodes, so it can frame to suit.
                    if let Ok((
                        FrameHeader {
                            dst: Dest::Station(to),
                            ..
                        },
                        _,
                    )) = FrameHeader::decode(&data)
                    {
                        if let Some(open) = x.peer(to) {
                            link.peer_features(to, open.features);
                        }
                    }
                    link.send(&data)?
                }
                Output::Transmit { .. } => {}
                Output::Event(Event::Received { from, id, object }) => {
                    let _ = events.send(RadioEvt::Received {
                        from,
                        xfer_id: id,
                        object,
                    });
                }
                Output::Event(Event::Delivered { to, id, receipt, .. }) => {
                    let _ = events.send(RadioEvt::Delivered {
                        xfer_id: id,
                        to,
                        receipt,
                    });
                }
                Output::Event(Event::Failed { to, id, reason }) => {
                    let _ = events.send(RadioEvt::Failed {
                        xfer_id: id,
                        to,
                        reason,
                    });
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(Ended::Stopped);
        }
        let t = now();
        match x.next_deadline() {
            Some(d) if d <= t => x.on_deadline(t, &mut out),
            next => {
                let wait = next
                    .map_or(MAX_WAIT, |d| Duration::from_millis(d.0 - t.0))
                    .min(MAX_WAIT);
                let wait = match next_beacon {
                    Some(at) => wait.min(at.saturating_duration_since(Instant::now())),
                    None => wait,
                };
                if let Some(frame) = link.recv_timeout(wait)? {
                    heard_changed |= hear(&mut heard, &live.get().trust, cfg.me, &frame);
                    if let Ok((header, payload)) = FrameHeader::decode(&frame) {
                        if header.ftype == FrameType::Sync
                            && (matches!(header.dst, Dest::Broadcast) || header.dst == Dest::Station(cfg.me))
                            && header.src != cfg.me
                        {
                            let _ = events.send(RadioEvt::Sync {
                                from: header.src,
                                payload: payload.to_vec(),
                            });
                        }
                    }
                    x.handle(now(), Input::Frame { port, data: frame }, &mut out);
                }
            }
        }
    }
}

/// Note the station a frame came from, and check its beacon if it is one;
/// true when the table changed in a way worth reporting.
pub(crate) fn hear(table: &mut heard::HeardTable, trust: &Trust, me: Callsign, frame: &[u8]) -> bool {
    let Ok((h, _)) = FrameHeader::decode(frame) else {
        return false;
    };
    if h.src == me {
        return false;
    }
    let t = unix_now();
    let new = table.frame(t, h.src);
    match read_beacon(frame) {
        Some(b) => {
            if table.beacon(t, &b, trust) == heard::KeyCheck::Mismatch {
                log(format!(
                    "beacon from {} carries a different key than station.toml trusts for {}",
                    b.from, b.from
                ));
            }
            true
        }
        None => new,
    }
}
