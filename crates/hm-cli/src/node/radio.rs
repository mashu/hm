//! The radio bearer thread: a shell around [`hm_node::Radio`]. It keeps a
//! KISS TNC or the built-in modem open, moves frames between it and the
//! machine, passes on the node's commands and the machine's events, and opens
//! a new link when `[radio]` changes.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use hm_core::{Input, Machine, Millis, Output};
use hm_ident::Identity;
use hm_node::{Radio, RadioCmd, RadioEvt, RadioSpec};
use hm_wire::{Dest, FrameHeader};

use super::live::LiveConfig;
use super::types::{log, NodeConfig, RadioConfig, RadioLink};
use crate::config::RadioSettings;
use crate::driver::Link;
use crate::kiss_link::KissLink;
use crate::sound_link::SoundLink;
use crate::station::unix_now;

const RECONNECT: Duration = Duration::from_secs(5);
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
    let start = Instant::now();
    let now = || Millis(start.elapsed().as_millis() as u64);
    let spec = RadioSpec {
        me: cfg.me,
        identity: Identity::from_secret(cfg.key.identity.secret()),
        timing: rc.timing,
        link_features: link.features(),
        has_internet: cfg.internet.is_some(),
        epoch: unix_now(),
        seed: getrandom::u64().unwrap_or_else(|_| unix_now()),
    };
    let mut radio = Radio::new(spec, &live.get().node_settings(), now()).map_err(io::Error::other)?;
    let port = radio.port();
    let mut out: Vec<Output<RadioEvt>> = Vec::new();
    loop {
        if live.version() != live_version {
            live_version = live.version();
            let live = live.get();
            if cfg.radio_builder.is_some() && live.radio.link_settings() != *settings {
                return Ok(Ended::Reconfigure);
            }
            radio.apply(now(), &live.node_settings());
        }
        while let Ok(command) = cmds.try_recv() {
            radio.handle(now(), Input::Command(command), &mut out);
        }
        if let Some(at) = link.drained() {
            radio.transmitted(
                Millis(at.saturating_duration_since(start).as_millis() as u64),
                port,
            );
        }
        let t = now();
        if radio.next_deadline().is_some_and(|d| d <= t) {
            radio.on_deadline(t, &mut out);
        }
        for o in out.drain(..) {
            match o {
                Output::Transmit { data, .. } => {
                    // Tell the link what the destination decodes, so it can frame to suit.
                    if let Ok((
                        FrameHeader {
                            dst: Dest::Station(to),
                            ..
                        },
                        _,
                    )) = FrameHeader::decode(&data)
                    {
                        if let Some(features) = radio.peer_features(to) {
                            link.peer_features(to, features);
                        }
                    }
                    link.send(&data)?
                }
                Output::Event(event) => {
                    let _ = events.send(event);
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(Ended::Stopped);
        }
        let t = now();
        let wait = radio
            .next_deadline()
            .map_or(MAX_WAIT, |d| Duration::from_millis(d.0.saturating_sub(t.0)))
            .min(MAX_WAIT);
        if let Some(frame) = link.recv_timeout(wait)? {
            radio.handle(now(), Input::Frame { port, data: frame }, &mut out);
        }
    }
}
