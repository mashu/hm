//! The radio side of a station, without I/O: the transfer engine, our
//! beacons, the table of stations heard, SYNC frames under the control
//! airtime budget, and what the channel is believed to be.
//!
//! [`Radio`] is an [`hm_core::Machine`]: frames heard and the node's
//! [`RadioCmd`]s go in, frames to put on air and [`RadioEvt`]s for the node
//! come out. The daemon's radio thread is a shell that moves frames between
//! it and a KISS TNC or the built-in modem; a simulator runs it on simulated
//! channels.
//!
//! Time is the machine's own clock ([`Millis`]); `epoch` says what Unix time
//! its zero is, for beacons, the stations heard and SYNC expiry.

use hm_core::{DetRng, Input, Machine, Millis, Output, Port};
use hm_ident::Identity;
use hm_model::{ChannelModel, ChannelObservation};
use hm_wire::{
    Callsign, Dest, FrameHeader, FrameType, Locator, FEATURE_MAILBOX, FEATURE_RELAY, FLAG_HOLDING,
};
use hm_xfer::beacon::{beacon_frame, read_beacon};
use hm_xfer::{broadcast_peer, Command, Config, Event, PeerBelief, Xfer};

use crate::adverts::advertised_flags;
use crate::control::{
    beacon_interval_ms, control_share_permyriad, live_window_secs, ControlBudget, SyncQueue,
    CONTROL_BUDGET_WINDOW_MS, SYNC_QUEUE_FRAMES,
};
use crate::{heard, log, RadioCmd, RadioEvt, RelaySettings, Settings, Trust};

/// The first beacon goes out at a random moment in this window after the
/// radio comes up (or between half and one beacon interval, if that is
/// shorter), so stations started together do not all beacon at once.
const FIRST_BEACON_MS: (u64, u64) = (5_000, 30_000);
/// The stations heard reach the node at least this often.
const HEARD_REPORT_MS: u64 = 30_000;
/// Stations heard within this long share the channel with us, at least.
const SHARING_WINDOW_SECS: u64 = 3600;
/// How often the stations heard are taken into the channel belief.
const CHANNEL_LOOK_SECS: u64 = 60;
/// A SYNC frame the control budget holds back is offered again this soon.
const SYNC_RETRY_MS: u64 = 1_000;
/// Bytes a link adds to each frame, for the airtime of our own frames.
const LINK_OVERHEAD_BYTES: u64 = 24;

/// Link parameters the transfer engine uses to time overs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LinkTiming {
    pub bitrate_bps: u32,
    pub txdelay_ms: u64,
    pub guard_ms: u64,
    pub max_rounds: u8,
}

impl LinkTiming {
    /// Airtime of one frame of `len` bytes on this link, key-up included.
    pub fn airtime_ms(&self, len: usize) -> u64 {
        self.txdelay_ms.saturating_add(self.frame_ms(len))
    }

    /// Airtime of a frame of `len` bytes sent while the transmitter is keyed.
    pub fn frame_ms(&self, len: usize) -> u64 {
        ((len as u64 + LINK_OVERHEAD_BYTES) * 8 * 1_000).div_ceil(u64::from(self.bitrate_bps.max(1)))
    }
}

/// What a radio is, fixed while its link stays up.
pub struct RadioSpec {
    pub me: Callsign,
    /// Our key: it signs beacons and custody receipts.
    pub identity: Identity,
    pub timing: LinkTiming,
    /// Feature bits the link adds to our OPEN (compact header, IL2P).
    pub link_features: u32,
    /// Whether the station has an internet endpoint: beacons say so.
    pub has_internet: bool,
    /// Unix seconds at the machine's `Millis(0)`.
    pub epoch: u64,
    /// Seed of the machine's random choices (beacon timing, symbols).
    pub seed: u64,
}

/// The radio side of a station; see the crate docs.
pub struct Radio {
    me: Callsign,
    identity: Identity,
    timing: LinkTiming,
    link_features: u32,
    has_internet: bool,
    epoch: u64,
    x: Xfer,
    port: Port,
    rng: DetRng,
    trust: Trust,
    relay: RelaySettings,
    locator: Option<Locator>,
    heard: heard::HeardTable,
    heard_changed: bool,
    next_report: Millis,
    /// Seconds between beacons as set; 0 sends none.
    beacon_secs: u64,
    next_beacon: Option<Millis>,
    /// Seconds between beacons now: longer as more stations share the channel.
    beacon_interval_secs: u64,
    interval_told: bool,
    control_permyriad: u16,
    control_budget: ControlBudget,
    sync_queue: SyncQueue,
    next_sync: Option<Millis>,
    holding: bool,
    channel: ChannelModel,
    channel_seen: u64,
    /// Airtime of others' frames heard since `channel_seen`.
    busy_ms: u64,
    sharing: usize,
}

impl Radio {
    /// A radio whose link came up at `now`.
    pub fn new(spec: RadioSpec, settings: &Settings, now: Millis) -> Result<Radio, String> {
        let RadioSpec {
            me,
            identity,
            timing,
            link_features,
            has_internet,
            epoch,
            seed,
        } = spec;
        let mut cfg = Config::for_link(me, timing.bitrate_bps, Millis(timing.txdelay_ms));
        cfg.ack_guard = Millis(timing.guard_ms);
        cfg.max_rounds = timing.max_rounds;
        let rng = DetRng::from_seed(seed);
        let mut x = Xfer::new(cfg, Identity::from_secret(identity.secret()), rng.fork(1))?;
        x.set_trust(settings.trust.iter());
        x.set_application_ack(true);
        let port = x.config().port;
        let control_permyriad = permyriad(settings.relay.control_airtime_fraction);
        let unix = epoch + now.0 / 1_000;
        let channel = ChannelModel::default();
        let sharing = channel.sharing(unix);
        let control_budget = ControlBudget::new(
            CONTROL_BUDGET_WINDOW_MS,
            control_share_permyriad(control_permyriad, sharing),
        )?;
        let mut radio = Radio {
            me,
            identity,
            timing,
            link_features,
            has_internet,
            epoch,
            x,
            port,
            trust: settings.trust.clone(),
            relay: settings.relay.clone(),
            locator: settings.locator,
            heard: heard::HeardTable::default(),
            heard_changed: false,
            next_report: now + Millis(HEARD_REPORT_MS),
            beacon_secs: settings.beacon_secs,
            next_beacon: None,
            beacon_interval_secs: settings.beacon_secs,
            interval_told: false,
            control_permyriad,
            control_budget,
            sync_queue: SyncQueue::new(SYNC_QUEUE_FRAMES),
            next_sync: None,
            holding: false,
            channel,
            channel_seen: unix,
            busy_ms: 0,
            sharing,
            rng: rng.fork(2),
        };
        radio.set_features();
        radio.next_beacon = radio.beacon_every().map(|every| {
            let (lo, hi) = (FIRST_BEACON_MS.0.min(every / 2), FIRST_BEACON_MS.1.min(every));
            now + Millis(lo + radio.rng.below(hi - lo + 1))
        });
        Ok(radio)
    }

    /// Take changed settings in, at `now`.
    pub fn apply(&mut self, now: Millis, settings: &Settings) {
        if settings.relay != self.relay {
            self.relay = settings.relay.clone();
            self.set_features();
            self.control_permyriad = permyriad(self.relay.control_airtime_fraction);
            self.control_budget
                .set_permyriad(control_share_permyriad(self.control_permyriad, self.sharing));
        }
        if settings.trust != self.trust {
            self.trust = settings.trust.clone();
            self.x.set_trust(self.trust.iter());
        }
        self.locator = settings.locator;
        if settings.beacon_secs != self.beacon_secs {
            // A new interval: the next beacon one (new) interval from now, or none.
            self.beacon_secs = settings.beacon_secs;
            self.next_beacon = self.beacon_every().map(|every| now + Millis(every));
        }
    }

    /// Feature bits `peer` said it decodes in its latest OPEN: a link may
    /// frame to suit.
    pub fn peer_features(&self, peer: Callsign) -> Option<u32> {
        self.x.peer(peer).map(|open| open.features)
    }

    /// The port the transfer engine transmits on.
    pub fn port(&self) -> Port {
        self.port
    }

    fn unix(&self, now: Millis) -> u64 {
        self.epoch + now.0 / 1_000
    }

    fn beacon_every(&self) -> Option<u64> {
        (self.beacon_secs > 0).then(|| self.beacon_secs * 1_000)
    }

    fn set_features(&mut self) {
        let mut features = self.link_features;
        if self.relay.enabled {
            features |= FEATURE_RELAY;
        }
        if self.relay.mailbox {
            features |= FEATURE_MAILBOX;
        }
        self.x.set_features(features);
    }

    fn command(&mut self, now: Millis, command: RadioCmd, xo: &mut Vec<Output<Event>>) {
        match command {
            RadioCmd::Send {
                object,
                to,
                precedence,
                belief,
            } => {
                self.x
                    .handle(now, Input::Command(Command::Belief { peer: to, belief }), xo);
                self.x.handle(
                    now,
                    Input::Command(Command::Send {
                        to,
                        object,
                        precedence,
                    }),
                    xo,
                );
            }
            RadioCmd::Broadcast {
                object,
                precedence,
                erasure,
            } => {
                self.x.handle(
                    now,
                    Input::Command(Command::Belief {
                        peer: broadcast_peer(),
                        belief: PeerBelief::open(erasure),
                    }),
                    xo,
                );
                self.x
                    .handle(now, Input::Command(Command::Broadcast { object, precedence }), xo);
            }
            RadioCmd::Accept {
                from,
                xfer_id,
                accepted,
                retry_after,
            } => self.x.handle(
                now,
                Input::Command(Command::Accept {
                    from,
                    id: xfer_id,
                    accepted,
                    retry_after,
                }),
                xo,
            ),
            RadioCmd::Sync { to, payload } => {
                let unix = self.unix(now);
                self.sync_queue.push(to, payload, unix);
                self.next_sync = Some(now);
            }
            RadioCmd::Holding(holding) => self.holding = holding,
        }
    }

    fn frame(
        &mut self,
        now: Millis,
        data: Vec<u8>,
        xo: &mut Vec<Output<Event>>,
        out: &mut Vec<Output<RadioEvt>>,
    ) {
        let unix = self.unix(now);
        self.busy_ms += self.timing.frame_ms(data.len());
        self.heard_changed |= self.hear(unix, &data);
        if let Ok((header, payload)) = FrameHeader::decode(&data) {
            if header.ftype == FrameType::Sync && header.src != self.me {
                self.sync_queue.heard(payload);
                if matches!(header.dst, Dest::Broadcast) || header.dst == Dest::Station(self.me) {
                    out.push(Output::Event(RadioEvt::Sync {
                        from: header.src,
                        payload: payload.to_vec(),
                    }));
                }
            }
        }
        self.x.handle(
            now,
            Input::Frame {
                port: self.port,
                data,
            },
            xo,
        );
    }

    /// Note the station a frame came from, and check its beacon if it is
    /// one; true when the table changed in a way worth reporting.
    fn hear(&mut self, unix: u64, frame: &[u8]) -> bool {
        let Ok((header, _)) = FrameHeader::decode(frame) else {
            return false;
        };
        if header.src == self.me {
            return false;
        }
        let new = self.heard.frame(unix, header.src);
        match read_beacon(frame) {
            Some(beacon) => {
                if self.heard.beacon(unix, &beacon, &self.trust) == heard::KeyCheck::Mismatch {
                    log(format!(
                        "beacon from {} carries a different key than the one trusted for it",
                        beacon.from
                    ));
                }
                true
            }
            None => new,
        }
    }

    /// Everything that is due at `now`, besides the transfer engine's own
    /// deadlines: the channel belief, a beacon, the table of stations heard,
    /// a SYNC frame the control budget admits.
    fn service(&mut self, now: Millis, out: &mut Vec<Output<RadioEvt>>) {
        let unix = self.unix(now);
        if !self.interval_told {
            self.interval_told = true;
            out.push(Output::Event(RadioEvt::BeaconInterval(self.beacon_interval_secs)));
        }
        let window = live_window_secs(self.beacon_interval_secs).max(SHARING_WINDOW_SECS);
        if unix >= self.channel_seen + CHANNEL_LOOK_SECS {
            let exposure = (unix - self.channel_seen) as f64 / window as f64;
            self.channel
                .observe_active(unix, self.heard.active(unix, window) as u32, exposure);
            self.channel.observe(
                unix,
                ChannelObservation::Occupancy {
                    busy_ms: self.busy_ms,
                    total_ms: (unix - self.channel_seen) * 1_000,
                },
            );
            self.busy_ms = 0;
            self.channel_seen = unix;
            let busy = self.channel.busy(unix);
            self.x
                .set_channel(busy, self.channel.contenders(unix).ceil() as u32);
            out.push(Output::Event(RadioEvt::Channel { busy }));
        }
        let sharing = self.channel.sharing(unix);
        if sharing != self.sharing {
            self.sharing = sharing;
            self.control_budget
                .set_permyriad(control_share_permyriad(self.control_permyriad, sharing));
        }
        if let (Some(at), Some(every)) = (self.next_beacon, self.beacon_every()) {
            if now >= at {
                self.beacon(now, unix, every, out);
            }
        }
        if self.heard_changed || now >= self.next_report {
            self.heard.expire(unix);
            out.push(Output::Event(RadioEvt::Heard(self.heard.list())));
            self.heard_changed = false;
            self.next_report = now + Millis(HEARD_REPORT_MS);
        }
        if self.next_sync.is_some_and(|at| now >= at) {
            self.next_sync = None;
            let sending = self
                .sync_queue
                .front(unix)
                .map(|item| (item.to, item.payload.clone()));
            if let Some((to, payload)) = sending {
                let header = FrameHeader {
                    ftype: FrameType::Sync,
                    src: self.me,
                    dst: to,
                    session: 0,
                    index: 0,
                };
                match header.frame(&payload) {
                    Ok(frame) => {
                        if self
                            .control_budget
                            .admit(now.0, self.timing.airtime_ms(frame.len()))
                        {
                            out.push(Output::Transmit {
                                port: self.port,
                                data: frame,
                            });
                            self.sync_queue.pop();
                            // The next one, if any, goes when the budget has room.
                            self.next_sync = (!self.sync_queue.is_empty()).then_some(now);
                        } else {
                            self.next_sync = Some(now + Millis(SYNC_RETRY_MS));
                        }
                    }
                    Err(error) => {
                        log(format!("SYNC frame not sent: {error}"));
                        self.sync_queue.pop();
                        self.next_sync = (!self.sync_queue.is_empty()).then_some(now);
                    }
                }
            }
        }
    }

    fn beacon(&mut self, now: Millis, unix: u64, every: u64, out: &mut Vec<Output<RadioEvt>>) {
        let flags = advertised_flags(self.has_internet, &self.relay);
        let flags = if self.holding { flags | FLAG_HOLDING } else { flags };
        let heard = self
            .heard
            .for_beacon(unix, live_window_secs(self.beacon_interval_secs));
        let frame = match beacon_frame(&self.identity, self.me, flags, unix as u32, self.locator, heard) {
            Ok(frame) => frame,
            Err(error) => {
                log(format!("beacon not sent: {error}"));
                self.next_beacon = Some(now + Millis(every));
                return;
            }
        };
        // Every station in reach beacons about this often: together they keep
        // to the beacon share of the channel.
        let interval = beacon_interval_ms(every, self.sharing, self.timing.airtime_ms(frame.len()));
        out.push(Output::Transmit {
            port: self.port,
            data: frame,
        });
        if interval / 1_000 != self.beacon_interval_secs {
            self.beacon_interval_secs = interval / 1_000;
            out.push(Output::Event(RadioEvt::BeaconInterval(self.beacon_interval_secs)));
        }
        let jitter = interval / 10;
        self.next_beacon = Some(now + Millis(interval - jitter + self.rng.below(2 * jitter + 1)));
    }

    /// What the transfer engine said, for the node.
    fn engine_output(&self, xo: Vec<Output<Event>>, out: &mut Vec<Output<RadioEvt>>) {
        for o in xo {
            out.push(match o {
                Output::Transmit { port, data } => Output::Transmit { port, data },
                Output::Event(Event::Received { from, id, object }) => Output::Event(RadioEvt::Received {
                    from,
                    xfer_id: id,
                    object,
                }),
                Output::Event(Event::Delivered { to, id, receipt, .. }) => {
                    Output::Event(RadioEvt::Delivered {
                        xfer_id: id,
                        to,
                        receipt,
                    })
                }
                Output::Event(Event::Failed { to, id, reason }) => Output::Event(RadioEvt::Failed {
                    xfer_id: id,
                    to,
                    reason,
                }),
                Output::Event(Event::Over { to, sent, got }) => {
                    Output::Event(RadioEvt::Over { to, sent, got })
                }
            });
        }
    }

    fn channel_look_at(&self) -> Millis {
        Millis((self.channel_seen + CHANNEL_LOOK_SECS).saturating_sub(self.epoch) * 1_000)
    }
}

impl Machine for Radio {
    type Input = Input<RadioCmd>;
    type Output = Output<RadioEvt>;

    fn handle(&mut self, now: Millis, input: Input<RadioCmd>, out: &mut Vec<Output<RadioEvt>>) {
        let mut xo = Vec::new();
        match input {
            Input::Command(command) => self.command(now, command, &mut xo),
            Input::Frame { data, .. } => self.frame(now, data, &mut xo, out),
        }
        self.engine_output(xo, out);
        self.service(now, out);
    }

    fn on_deadline(&mut self, now: Millis, out: &mut Vec<Output<RadioEvt>>) {
        if self.x.next_deadline().is_some_and(|at| at <= now) {
            let mut xo = Vec::new();
            self.x.on_deadline(now, &mut xo);
            self.engine_output(xo, out);
        }
        self.service(now, out);
    }

    fn next_deadline(&self) -> Option<Millis> {
        if !self.interval_told {
            return Some(Millis::ZERO);
        }
        [
            self.x.next_deadline(),
            self.next_beacon,
            Some(self.next_report),
            Some(self.channel_look_at()),
            self.next_sync,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn transmitted(&mut self, now: Millis, port: Port) {
        self.x.transmitted(now, port);
    }
}

fn permyriad(fraction: f64) -> u16 {
    (fraction * 10_000.0).round().clamp(0.0, 10_000.0) as u16
}
