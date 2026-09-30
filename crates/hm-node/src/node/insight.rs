//! [`Node::insight`]: what the node knows, gathered into the views of
//! [`crate::insight`]. Read-only: nothing here changes what the node
//! believes or decides.

use std::collections::BTreeMap;

use hm_model::{
    Bearer, Beliefs, Beta, CustodianModel, CustodianObservation, LinkKey, LinkModel, LinkObservation,
    Observed, Subject, Update,
};
use hm_route::Route;
use hm_wire::{Callsign, ObjectId};

use super::decisions::{Choice, Decision};
use super::Node;
use crate::heard;
use crate::insight::{
    BandView, BeaconView, CalibrationView, ChannelView, CustodianView, DecisionView, HopView, Insight,
    Interval, LinkView, Place, RouteView, StationView, UpdateView, Verdict,
};

const HOUR: u64 = 3_600;
const DAY: u64 = 24 * HOUR;
/// The credible intervals shown: the middle 90 %.
const CREDIBLE: (f64, f64) = (0.05, 0.95);

impl Node {
    /// What the node knows at `now`: every station it has heard of, what it
    /// believes of every path and custodian, the channel, how its chances
    /// have come true, the latest evidence, and why each message goes or
    /// waits.
    pub fn insight(&self, now: u64) -> Insight {
        let mut decisions: Vec<DecisionView> = self
            .decisions
            .iter()
            .map(|(id, decision)| decision_view(id, decision))
            .collect();
        decisions.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
        Insight {
            at: now,
            me: self.id.me.to_string(),
            stations: self.station_views(now),
            links: self
                .beliefs
                .links()
                .map(|(key, link)| self.link_view(*key, link, now))
                .collect(),
            custodians: self
                .beliefs
                .custodians()
                .map(|(call, custodian)| self.custodian_view(*call, custodian, now))
                .collect(),
            channel: ChannelView {
                radio_up: self.radio_up,
                busy: self.channel.busy,
                contenders: self.channel.contenders,
                airtime_price_per_min: self.settings.costs.airtime(self.channel.busy)[Bearer::Radio] * 60.0,
                beacon_interval_secs: self.beacon_interval,
            },
            calibration: calibration_views(&self.beliefs),
            journal: self.beliefs.journal().rev().map(update_view).collect(),
            decisions,
        }
    }

    /// Every station heard of: this one, those heard on the air and those
    /// their beacons name, the trusted, both ends of every path believed in
    /// or advertised, every custodian, every destination.
    fn station_views(&self, now: u64) -> Vec<StationView> {
        let me = self.id.me;
        let mut stations: BTreeMap<Callsign, StationView> = BTreeMap::new();
        let mine = station(&mut stations, me);
        mine.me = true;
        mine.place = self.settings.locator.map(Place::from);
        mine.offers = heard::offers(self.advertised);
        for heard in &self.heard {
            for named in heard.beacon.iter().flat_map(|beacon| &beacon.heard) {
                station(&mut stations, named.call);
            }
            let view = station(&mut stations, heard.call);
            view.heard_at = Some(heard.last);
            if let Some(beacon) = &heard.beacon {
                view.place = beacon.locator.map(Place::from);
                view.offers = beacon.offers();
                view.beacon = Some(BeaconView {
                    at: beacon.at,
                    key: beacon.key.as_str(),
                    clock_offset: beacon.clock_offset,
                    hears: beacon
                        .heard
                        .iter()
                        .map(|named| (named.call.to_string(), named.minutes))
                        .collect(),
                });
            }
        }
        let ends = self
            .beliefs
            .links()
            .flat_map(|(key, _)| [key.from, key.to])
            .chain(self.graph.links().flat_map(|(from, to, _, _)| [from, to]));
        let others = self.settings.trust.iter().map(|(call, _)| call);
        let custodians = self.beliefs.custodians().map(|(call, _)| *call);
        let destinations = self.decisions.iter().map(|(_, decision)| decision.to);
        for call in ends.chain(others).chain(custodians).chain(destinations) {
            station(&mut stations, call);
        }
        for (call, view) in stations.iter_mut() {
            let call = *call;
            view.trusted = self.settings.trust.key_for(call).is_some();
            if view.offers.is_empty() {
                view.offers = self.graph.flags(call, now).map(heard::offers).unwrap_or_default();
            }
            if call == me {
                continue;
            }
            let on_air = view
                .heard_at
                .is_some_and(|at| self.radio_up && at + self.live_window >= now);
            view.linked = [
                (on_air, Bearer::Radio),
                (self.linked(call), Bearer::Internet),
                (self.modem.and_then(|(_, peer)| peer) == Some(call), Bearer::Modem),
            ]
            .into_iter()
            .filter(|(linked, _)| *linked)
            .map(|(_, bearer)| bearer.name())
            .collect();
        }
        stations.into_values().collect()
    }

    fn link_view(&self, key: LinkKey, link: &LinkModel, now: u64) -> LinkView {
        let prior = self.beliefs.link_prior(key.bearer);
        let forecast: Vec<f64> = (0..24).map(|h| link.p_open(now + h * HOUR, now)).collect();
        let midnight = now - now % DAY;
        let erasure = link.erasure(prior, now);
        LinkView {
            a: key.from.to_string(),
            b: key.to.to_string(),
            bearer: key.bearer.name(),
            mine: key.touches(self.id.me),
            seen: link.last_open().is_some(),
            reach: link.reachable(),
            open_now: forecast[0],
            next_opening: forecast
                .iter()
                .position(|&p| p >= 0.5)
                .map(|h| now + h as u64 * HOUR),
            forecast,
            daily: (0..24)
                .map(|h| link.availability().p_open(midnight + h * HOUR, now))
                .collect(),
            frame_loss: interval(Beta {
                a: erasure.lost,
                b: erasure.got,
            }),
            handoff_if_open: interval(link.handoff(prior, now)),
            success_model: self.beliefs.model_success(key, now, now),
            success: self.beliefs.link_success(key, now, now),
            persistence_mins: link.persistence_secs() / 60.0,
            last_open: link.last_open(),
            beacon_interval: link.beacon_interval(),
            frames: link.frame_counts(now),
            handoffs: link.handoff_counts(now),
        }
    }

    fn custodian_view(&self, call: Callsign, custodian: &CustodianModel, now: u64) -> CustodianView {
        let prior = self.beliefs.custodian_prior();
        CustodianView {
            call: call.to_string(),
            accepts: interval(custodian.accepts(prior, now)),
            delivers: interval(custodian.delivers(prior, now)),
            lateness: custodian.lateness(prior, now),
            busy_until: Some(custodian.busy_until()).filter(|&until| until > now),
            accept_counts: custodian.accept_counts(now),
            deliver_counts: custodian.deliver_counts(now),
        }
    }
}

/// The view of `call`, added blank when it is not there yet.
fn station(stations: &mut BTreeMap<Callsign, StationView>, call: Callsign) -> &mut StationView {
    stations.entry(call).or_insert_with(|| StationView {
        call: call.to_string(),
        me: false,
        place: None,
        heard_at: None,
        beacon: None,
        trusted: false,
        offers: Vec::new(),
        linked: Vec::new(),
    })
}

fn interval(beta: Beta) -> Interval {
    Interval {
        mean: beta.mean(),
        low: beta.quantile(CREDIBLE.0),
        high: beta.quantile(CREDIBLE.1),
    }
}

/// Every bearer's record, for paths seen open and paths only inferred.
fn calibration_views(beliefs: &Beliefs) -> Vec<CalibrationView> {
    Bearer::ALL
        .into_iter()
        .flat_map(|bearer| [true, false].map(|seen| (bearer, seen)))
        .map(|(bearer, seen)| {
            let calibration = beliefs.calibration(bearer, seen);
            let bands: Vec<BandView> = calibration
                .bands()
                .map(|band| BandView {
                    given: band.given,
                    carried: band.carried,
                    outcomes: band.outcomes,
                })
                .collect();
            CalibrationView {
                bearer: bearer.name(),
                seen,
                outcomes: bands.iter().map(|band| band.outcomes).sum(),
                bands,
                curve: calibration.curve().collect(),
            }
        })
        .collect()
}

fn update_view(update: &Update) -> UpdateView {
    let (subject, stations, bearer) = match update.subject {
        Subject::Link(key) => (
            "link",
            vec![key.from.to_string(), key.to.to_string()],
            Some(key.bearer),
        ),
        Subject::Custodian(call) => ("custodian", vec![call.to_string()], None),
        Subject::Calibration(bearer, _) => ("calibration", Vec::new(), Some(bearer)),
    };
    UpdateView {
        at: update.at,
        subject,
        stations,
        bearer: bearer.map(Bearer::name),
        observed: describe(update),
        before: update.before,
        after: update.after,
    }
}

/// What was observed, in words.
fn describe(update: &Update) -> String {
    match update.observed {
        Observed::Link(observation) => match observation {
            LinkObservation::Beacon => "beacon heard".into(),
            LinkObservation::Heard => "frame heard".into(),
            LinkObservation::Reported => "a neighbour reported hearing it".into(),
            LinkObservation::Missed => "beacon due, not heard".into(),
            LinkObservation::Up => "session up".into(),
            LinkObservation::Down => "session down".into(),
            LinkObservation::Over { sent, got } => format!("{got} of {sent} frames arrived"),
            LinkObservation::Handoff { ok: true } => "handoff completed".into(),
            LinkObservation::Handoff { ok: false } => "handoff failed".into(),
        },
        Observed::Custodian(observation) => match observation {
            CustodianObservation::Accepted => "took custody".into(),
            CustodianObservation::Refused => "refused custody".into(),
            CustodianObservation::Busy { retry_after } => format!("busy for {}", duration(retry_after)),
            CustodianObservation::Delivered { late_secs } => {
                format!("receipt came back, {} past due", duration(late_secs))
            }
            CustodianObservation::Silent { late_secs, .. } => {
                format!("no receipt {} past due; custody reclaimed", duration(late_secs))
            }
        },
        Observed::Outcome { chance, carried } => {
            let paths = match update.subject {
                Subject::Calibration(_, false) => ", path never seen open",
                _ => "",
            };
            format!(
                "a handoff given {:.0} %{paths} was {}",
                chance * 100.0,
                if carried { "carried" } else { "not carried" }
            )
        }
    }
}

fn duration(secs: u64) -> String {
    match secs {
        0..90 => format!("{secs} s"),
        90..5_400 => format!("{} min", (secs + 30) / 60),
        _ => format!("{} h", (secs + 1_800) / 3_600),
    }
}

fn decision_view(id: &ObjectId, decision: &Decision) -> DecisionView {
    DecisionView {
        id: id.to_string(),
        to: decision.to.to_string(),
        at: decision.at,
        verdict: match &decision.choice {
            Choice::Send(route) => Verdict::Send {
                route: route_view(route),
            },
            Choice::Wait(route) => Verdict::Wait {
                route: route_view(route),
            },
            Choice::Hear(route) => Verdict::Hear {
                route: route_view(route),
            },
            Choice::Hold(error) => Verdict::Hold {
                reason: error.to_string(),
            },
        },
    }
}

fn route_view(route: &Route) -> RouteView {
    RouteView {
        hops: route
            .hops
            .iter()
            .map(|hop| HopView {
                from: hop.contact.from.to_string(),
                to: hop.contact.to.to_string(),
                bearer: hop.contact.bearer.name(),
                depart: hop.depart,
                chance: f64::from(hop.probability_permillion) / 1_000_000.0,
            })
            .collect(),
        chance: route.success_probability,
        arrival: route.arrival,
        utility: route.utility,
    }
}
