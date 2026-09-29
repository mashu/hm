//! A whole station, node and radio, as one [`Machine`], for simulators.
//!
//! The daemon runs [`Node`] and [`Radio`] in shells of their own (the
//! coordinator and the radio thread). A simulation runs them together, one
//! machine per station on simulated channels, and gives the node its tick:
//! the same decisions and the same frames on air, with no clock, threads or
//! sockets, so weeks of HF go by in seconds.
//!
//! A simulated station has a radio only: the internet and modem transfers the
//! node asks for fail at once, as for a station without those bearers.

use std::sync::Arc;

use hm_bundle::Precedence;
use hm_core::{Input, Machine, Millis, Output, Port};
use hm_ident::Identity;
use hm_route::ScheduledContact;
use hm_store::{EnqueueOpts, Store};
use hm_wire::{Callsign, ObjectId};

use crate::message::Draft;
use crate::{
    as_station, Command, Input as NodeInput, LinkTiming, Node, NodeIdentity, Notify, Radio, RadioEvt,
    RadioSpec, Settings, Transfer,
};

/// Everything a station is made of.
pub struct StationSpec {
    pub me: Callsign,
    pub identity: Identity,
    pub timing: LinkTiming,
    /// Feature bits the radio link adds to our OPEN.
    pub link_features: u32,
    pub settings: Settings,
    pub schedules: Vec<ScheduledContact>,
    /// Unix seconds at the machine's `Millis(0)`.
    pub epoch: u64,
    pub seed: u64,
    /// How often the node takes stock (the daemon: every second).
    pub tick: Millis,
}

/// What the operator does.
#[derive(Clone, Debug)]
pub enum StationCmd {
    /// Queue a text message to `to`: mail with a subject, chat without.
    Send {
        to: Callsign,
        text: String,
        subject: Option<String>,
        precedence: Precedence,
    },
    /// New settings.
    Settings(Box<Settings>),
}

/// What the operator sees.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StationEvent {
    /// A message was queued, as `id`.
    Queued { id: ObjectId, to: Callsign },
    /// A message could not be queued.
    NotQueued { to: Callsign, reason: String },
}

/// A station's node and radio as one machine; see the [module docs](self).
pub struct Station {
    me: Callsign,
    identity: Identity,
    epoch: u64,
    node: Node,
    radio: Radio,
    store: Arc<Store>,
    tick: Millis,
    next_tick: Millis,
    started: bool,
}

impl Station {
    /// A station keeping its memory in `store`, its radio up from `Millis(0)`.
    pub fn new(spec: StationSpec, store: Arc<Store>) -> Result<Station, String> {
        let StationSpec {
            me,
            identity,
            timing,
            link_features,
            settings,
            schedules,
            epoch,
            seed,
            tick,
        } = spec;
        let radio = Radio::new(
            RadioSpec {
                me,
                identity: Identity::from_secret(identity.secret()),
                timing,
                link_features,
                has_internet: false,
                epoch,
                seed: seed ^ 0x5241_4449_4f00_0000,
            },
            &settings,
            Millis::ZERO,
        )?;
        let node = Node::new(
            NodeIdentity {
                me,
                key_call: me,
                identity: Identity::from_secret(identity.secret()),
                schedules,
                has_internet: false,
                modem: None,
                radio_via: Some("simulated radio".into()),
                seed,
            },
            settings,
            store.clone(),
            Notify::none(),
            epoch,
        );
        Ok(Station {
            me,
            identity,
            epoch,
            node,
            radio,
            store,
            tick: Millis(tick.0.max(1)),
            next_tick: tick,
            started: false,
        })
    }

    pub fn me(&self) -> Callsign {
        self.me
    }

    /// The station's memory: its messages, holdings and beliefs.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn node(&self) -> &Node {
        &self.node
    }

    fn unix(&self, now: Millis) -> u64 {
        self.epoch + now.0 / 1_000
    }

    fn queue(
        &mut self,
        now: Millis,
        to: Callsign,
        text: &str,
        subject: Option<&str>,
        precedence: Precedence,
    ) -> StationEvent {
        let unix = self.unix(now);
        let seq = match subject {
            None => match self.store.next_peer_seq(to) {
                Ok(seq) => Some(seq),
                Err(error) => {
                    return StationEvent::NotQueued {
                        to,
                        reason: error.to_string(),
                    }
                }
            },
            Some(_) => None,
        };
        let draft = Draft {
            to,
            text,
            subject,
            precedence,
            seq,
        };
        let bundle = match draft.seal(&self.identity, self.me, unix) {
            Ok(bundle) => bundle,
            Err(error) => {
                return StationEvent::NotQueued {
                    to,
                    reason: error.to_string(),
                }
            }
        };
        let id = bundle.id();
        let queued = self.store.enqueue_with(
            id,
            &bundle.to_vec(),
            EnqueueOpts {
                to,
                precedence: precedence.to_u8(),
                now: unix,
                wire_seq: bundle.bundle().seq,
                expires_at: Some(bundle.bundle().expires_at()),
            },
        );
        match queued {
            Ok(_) => StationEvent::Queued { id, to },
            Err(error) => StationEvent::NotQueued {
                to,
                reason: error.to_string(),
            },
        }
    }

    /// Pass radio events to the node and the node's commands to the radio
    /// until neither has anything more to say.
    fn pump(
        &mut self,
        now: Millis,
        mut radio: Vec<Output<RadioEvt>>,
        mut commands: Vec<Command>,
        out: &mut Vec<Output<StationEvent>>,
    ) {
        let unix = self.unix(now);
        while !radio.is_empty() || !commands.is_empty() {
            for output in std::mem::take(&mut radio) {
                match output {
                    Output::Transmit { port, data } => out.push(Output::Transmit { port, data }),
                    Output::Event(event) => self.node.handle(unix, NodeInput::Radio(event), &mut commands),
                }
            }
            for command in std::mem::take(&mut commands) {
                match command {
                    Command::Radio(command) => self.radio.handle(now, Input::Command(command), &mut radio),
                    Command::NetDeliver { id, peer, .. } => self.node.handle(
                        unix,
                        NodeInput::NetDone {
                            id,
                            peer,
                            result: Transfer::Failed("no internet".into()),
                        },
                        &mut commands,
                    ),
                    Command::ModemDeliver { id, peer, .. } => self.node.handle(
                        unix,
                        NodeInput::ModemDone {
                            id,
                            peer,
                            result: Transfer::Failed("no modem".into()),
                        },
                        &mut commands,
                    ),
                    Command::NetSync { .. } => {}
                }
            }
        }
    }
}

impl Machine for Station {
    type Input = Input<StationCmd>;
    type Output = Output<StationEvent>;

    fn handle(&mut self, now: Millis, input: Input<StationCmd>, out: &mut Vec<Output<StationEvent>>) {
        as_station(self.me, self.unix(now), || self.take(now, input, out));
    }

    fn on_deadline(&mut self, now: Millis, out: &mut Vec<Output<StationEvent>>) {
        as_station(self.me, self.unix(now), || self.due(now, out));
    }

    fn next_deadline(&self) -> Option<Millis> {
        if !self.started {
            return Some(Millis::ZERO);
        }
        Some(
            self.radio
                .next_deadline()
                .map_or(self.next_tick, |at| at.min(self.next_tick)),
        )
    }

    fn transmitted(&mut self, now: Millis, port: Port) {
        self.radio.transmitted(now, port);
    }
}

impl Station {
    fn take(&mut self, now: Millis, input: Input<StationCmd>, out: &mut Vec<Output<StationEvent>>) {
        let (mut radio, mut commands) = (Vec::new(), Vec::new());
        match input {
            Input::Frame { port, data } => self.radio.handle(now, Input::Frame { port, data }, &mut radio),
            Input::Command(StationCmd::Send {
                to,
                text,
                subject,
                precedence,
            }) => {
                let event = self.queue(now, to, &text, subject.as_deref(), precedence);
                out.push(Output::Event(event));
            }
            Input::Command(StationCmd::Settings(settings)) => {
                self.radio.apply(now, &settings);
                self.node
                    .handle(self.unix(now), NodeInput::Settings(settings), &mut commands);
            }
        }
        self.pump(now, radio, commands, out);
    }

    fn due(&mut self, now: Millis, out: &mut Vec<Output<StationEvent>>) {
        let unix = self.unix(now);
        let (mut radio, mut commands) = (Vec::new(), Vec::new());
        if !self.started {
            self.started = true;
            self.node.handle(
                unix,
                NodeInput::Radio(RadioEvt::Using(Some("simulated radio".into()))),
                &mut commands,
            );
            self.node
                .handle(unix, NodeInput::Radio(RadioEvt::Up), &mut commands);
        }
        if self.radio.next_deadline().is_some_and(|at| at <= now) {
            self.radio.on_deadline(now, &mut radio);
        }
        if now >= self.next_tick {
            self.next_tick = now + self.tick;
            self.node.handle(
                unix,
                NodeInput::Tick {
                    links: Vec::new(),
                    modem: None,
                },
                &mut commands,
            );
        }
        self.pump(now, radio, commands, out);
    }
}
