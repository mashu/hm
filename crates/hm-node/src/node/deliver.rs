//! What is due goes out: bulletins are published, everything else is routed
//! by expected utility over the contact plan and handed to its next hop.

use hm_bundle::{Bundle, Kind, Opened};
use hm_model::{Bearer, Erasure, LinkKey, LinkObservation, PerBearer};
use hm_route::{plan_routes, LiveContact, Route, RouteError, RouteRequest, RoutingPolicy};
use hm_store::{Direction, Record};
use hm_wire::{wrap_routed, Callsign, ObjectId, FLAG_INTERNET, FLAG_MAILBOX, FLAG_RELAY};
use hm_xfer::PeerBelief;

use super::custody::enqueue_custody_fail;
use super::handoff::InFlight;
use super::{Command, Node};
use crate::{log, rf_policy, short, RadioCmd};

/// What an internet link is taken to carry.
const INTERNET_RATE_BPS: u32 = 10_000_000;
const INTERNET_CAPACITY: u64 = 64 * 1024 * 1024;
/// A radio contact is taken to last this long, for its capacity.
const RADIO_CONTACT_SECS: u64 = 600;
/// A message nothing known leads to is planned again this much later.
const REPLAN_SECS: u64 = 15 * 60;

/// What a handoff carries besides its route.
struct Handoff<'a> {
    visited: &'a [Callsign],
    expires_at: u64,
    object: &'a [u8],
    routed_len: u64,
    /// The best other route's chance, should this one fail.
    alternative: f64,
    /// What getting past the first hop is worth, in a delivered message's value.
    worth: f64,
}

impl Node {
    pub(super) fn deliver_due(&mut self, now: u64, out: &mut Vec<Command>) {
        let due = match self.store.due(now) {
            Ok(due) => due,
            Err(e) => {
                log(format!("store: {e}"));
                return;
            }
        };
        for r in due {
            if self.in_flight.keys().any(|(id, _)| *id == r.id) {
                continue;
            }
            let Ok(Some(object)) = self.store.object(r.id) else {
                continue;
            };
            let Ok(opened) = Opened::decode(&object) else {
                self.give_up(r.id, "stored object is not a bundle", now);
                continue;
            };
            let bundle = opened.bundle;
            if bundle.is_expired(now) {
                self.give_up(r.id, "bundle expired", now);
                continue;
            }
            if bundle.kind == Kind::Bulletin {
                self.publish_bulletin(now, &r, object, out);
            } else {
                self.route_and_send(now, &r, &bundle, object, out);
            }
        }
    }

    /// Give up on `id` for good, and tell the custodian before us.
    fn give_up(&self, id: ObjectId, reason: &str, now: u64) {
        if let Ok(Some(prior)) = self.store.abandon(id, reason) {
            enqueue_custody_fail(&self.store, self.id.me, &self.id.identity, id, prior, reason, now);
        }
    }

    /// A bulletin: to a peer that asked for it over SYNC, on the radio for
    /// everyone in reach, and without a radio to every internet peer.
    fn publish_bulletin(&mut self, now: u64, r: &Record, object: Vec<u8>, out: &mut Vec<Command>) {
        let bulletin_peer = hm_xfer::broadcast_peer();
        // A peer asked for this bulletin over SYNC: send it on the internet.
        // RF publish and fan-out go on as well.
        if let Some(peer) = self.control.target_for(r.id, now) {
            if self.linked(peer) && !self.in_flight.contains_key(&(r.id, peer)) {
                self.in_flight.insert(
                    (r.id, peer),
                    InFlight::direct(Bearer::Internet, peer, object.len() as u64, now),
                );
                out.push(Command::NetDeliver {
                    id: r.id,
                    peer,
                    object: object.clone(),
                });
                log(format!(
                    "sending bulletin {} to {peer} over the internet (requested)",
                    short(&r.id)
                ));
            }
        }
        if self.radio_up && !self.in_flight.contains_key(&(r.id, bulletin_peer)) {
            if let Err(error) = self.store.set_next_hop(r.id, bulletin_peer) {
                log(format!("store: {error}"));
                return;
            }
            self.radio_ids
                .insert((hm_xfer::object_id(&object), bulletin_peer), r.id);
            self.in_flight.insert(
                (r.id, bulletin_peer),
                InFlight::direct(Bearer::Radio, bulletin_peer, object.len() as u64, now),
            );
            // Listeners' links are like the radio links this station knows,
            // as a population.
            let prior = self.beliefs.link_prior(Bearer::Radio);
            out.push(Command::Radio(RadioCmd::Broadcast {
                object,
                precedence: r.precedence,
                erasure: Erasure::from_prior(prior.erasure, prior.dispersion),
            }));
            log(format!(
                "publishing bulletin {} on radio (attempt {})",
                short(&r.id),
                r.attempts + 1
            ));
            return;
        }
        if self.radio_up {
            return;
        }
        // No radio: push once to each connected internet peer; otherwise
        // wait for the radio or an internet link.
        let peers: Vec<Callsign> = self.links.iter().copied().collect();
        for peer in peers {
            if self.in_flight.contains_key(&(r.id, peer)) {
                continue;
            }
            self.in_flight.insert(
                (r.id, peer),
                InFlight::direct(Bearer::Internet, peer, object.len() as u64, now),
            );
            out.push(Command::NetDeliver {
                id: r.id,
                peer,
                object: object.clone(),
            });
            log(format!(
                "publishing bulletin {} to {peer} over the internet",
                short(&r.id)
            ));
        }
    }

    /// Links that could be tried now for a message to `destination`, never
    /// seen open: how likely each is comes from the beliefs about it, which
    /// start from its bearer's population. An internet link may be to the
    /// destination under its base callsign (then it is up, seen); an ARQ
    /// modem can call any station; the destination may be in radio range,
    /// unheard, of this station or, when nothing known leads there, of a
    /// relay it reaches; a relaying internet gateway may reach it through the
    /// internet core (a default route).
    fn add_potential_contacts(
        &mut self,
        now: u64,
        destination: Callsign,
        visited: &[Callsign],
        rf_ok: bool,
        routed_len: u64,
    ) {
        let me = self.id.me;
        let potential = |from, to, bearer, rate_bps, capacity_bytes| LiveContact {
            from,
            to,
            bearer,
            rate_bps,
            capacity_bytes,
            flags: 0,
            observed_at: now,
        };
        if self.linked(destination) {
            let link = LinkKey {
                from: me,
                to: destination,
                bearer: Bearer::Internet,
            };
            let seen_lately = self
                .beliefs
                .link(link)
                .and_then(|l| l.last_open())
                .is_some_and(|t| t + 60 >= now);
            if !seen_lately {
                self.beliefs.observe_link(link, now, LinkObservation::Up);
            }
            let _ = self.graph.observe_live_link(LiveContact {
                flags: FLAG_INTERNET,
                ..potential(
                    me,
                    destination,
                    Bearer::Internet,
                    INTERNET_RATE_BPS,
                    INTERNET_CAPACITY,
                )
            });
        }
        if let Some(modem) = self.id.modem.filter(|_| self.modem_up() && rf_ok) {
            let _ = self.graph.add_potential(potential(
                me,
                destination,
                Bearer::Modem,
                modem.rate_bps,
                modem.max_object,
            ));
        }
        if self.radio_up && rf_ok {
            let rate = self.settings.radio_bitrate;
            let _ = self.graph.add_potential(potential(
                me,
                destination,
                Bearer::Radio,
                rate,
                (u64::from(rate) * RADIO_CONTACT_SECS / 8).max(routed_len),
            ));
        }
        // Nothing known leads to the destination: beacons tell of two hops
        // around, and it is further. A relay this station reaches may hear
        // it, unheard here, as this station itself may; the chance is the
        // beliefs' about that path, which start from the population's. The
        // relay, a hop nearer, plans with what it knows.
        if rf_ok && self.graph.links_from(destination).next().is_none() {
            let relays: Vec<(Callsign, u32)> = self
                .graph
                .links_from(me)
                .filter(|&(to, bearer, _)| {
                    bearer.on_air()
                        && to != destination
                        && !visited.contains(&to)
                        && self
                            .graph
                            .flags(to, now)
                            .is_some_and(|flags| flags & FLAG_RELAY != 0)
                })
                .map(|(to, _, known)| (to, known.rate_bps))
                .collect();
            for (relay, rate) in relays {
                let _ = self.graph.add_potential(potential(
                    relay,
                    destination,
                    Bearer::Radio,
                    rate,
                    (u64::from(rate) * RADIO_CONTACT_SECS / 8).max(routed_len),
                ));
            }
        }
        let gateways: Vec<Callsign> = self
            .graph
            .stations_flagged(FLAG_INTERNET, now)
            .filter(|gateway| {
                *gateway != me
                    && *gateway != destination
                    && !visited.contains(gateway)
                    && self
                        .graph
                        .flags(*gateway, now)
                        .is_some_and(|flags| flags & (FLAG_RELAY | FLAG_MAILBOX) != 0)
            })
            .collect();
        for gateway in gateways {
            let _ = self.graph.add_potential(potential(
                gateway,
                destination,
                Bearer::Internet,
                INTERNET_RATE_BPS,
                INTERNET_CAPACITY,
            ));
        }
    }

    /// Route `r` by expected utility and hand it to the first hop of the
    /// chosen route (and, for urgent traffic, of a second one).
    fn route_and_send(
        &mut self,
        now: u64,
        r: &Record,
        bundle: &Bundle,
        object: Vec<u8>,
        out: &mut Vec<Command>,
    ) {
        let me = self.id.me;
        let destination = r.final_destination();
        let origin = bundle.from;
        if r.direction == Direction::Relay && self.settings.trust.key_for(origin).is_none() {
            let reason = format!("relay origin {origin} is no longer trusted");
            self.give_up(r.id, &reason, now);
            log(format!("stopped relaying {}: {reason}", short(&r.id)));
            return;
        }
        let rf_ok = rf_policy::may_transmit_rf(origin, me, self.id.key_call, &self.settings.trust);
        let requested_peer = self.control.target_for(r.id, now);
        let route_destination = requested_peer.unwrap_or(destination);
        let visited = r.visited.clone().unwrap_or_default();
        // The stations a route may not pass: those the message came through,
        // and its origin, which the routing wrapper does not list.
        let mut avoid = visited.clone();
        if origin != me && !avoid.contains(&origin) {
            avoid.push(origin);
        }
        let mut max_hops = r.max_hops.unwrap_or_else(|| bundle.max_hops());
        max_hops = max_hops.min(bundle.max_hops()).min(self.settings.relay.max_hops);
        if r.direction == Direction::Relay && !self.settings.relay.enabled {
            max_hops = max_hops.min((avoid.len() + 1) as u8);
        }
        let routed_len = (object.len()
            + usize::from(r.direction == Direction::Relay)
                * (10 + 6 * (usize::from(r.hop_count.unwrap_or(0)) + 1))) as u64;
        self.add_potential_contacts(now, route_destination, &avoid, rf_ok, routed_len);
        let closed_now = self.closed_now(now);
        let request = RouteRequest {
            source: me,
            destination: route_destination,
            now,
            expires_at: bundle.expires_at(),
            object_bytes: routed_len,
            // A station that asked for the message gets it straight.
            max_hops: if requested_peer.is_some() {
                (avoid.len() + 1) as u8
            } else {
                max_hops
            },
            airtime_budget_millis: self.settings.relay.airtime_budget_secs.saturating_mul(1_000),
            visited: &avoid,
            forbidden: PerBearer::from_fn(|bearer| bearer.on_air() && !rf_ok),
            closed_now: &closed_now,
            urgent: r.precedence >= 2,
        };
        let policy = RoutingPolicy {
            attempt_cost: self.settings.costs.attempt(),
            airtime_price: self.settings.costs.airtime(self.channel_busy),
            ..RoutingPolicy::default()
        };
        // Plan with one draw from the beliefs: links little is known about
        // get tried in proportion to the chance that they are the best. A
        // draw in which nothing is worth trying holds the message only when
        // the beliefs on average agree: a link as likely out of reach as
        // not is still worth one try when its airtime is cheap.
        self.plans += 1;
        let mut draw = self.beliefs.thompson(self.rng.fork(self.plans), now);
        let planned = match plan_routes(&self.graph, &mut draw, &request, policy) {
            Err(RouteError::NotWorthIt) => {
                plan_routes(&self.graph, &mut self.beliefs.mean(now), &request, policy)
            }
            planned => planned,
        };
        let plan = match planned {
            Ok(plan) => plan,
            Err(error) => {
                // Nothing known leads there yet: look again after a while,
                // or when the destination is heard.
                let again = now + REPLAN_SECS;
                if self.store.defer(r.id, again, None).unwrap_or(false) {
                    log(format!(
                        "{} to {destination}: {error}; looking again in {} min",
                        short(&r.id),
                        REPLAN_SECS / 60
                    ));
                }
                return;
            }
        };
        let alternative = plan
            .alternatives
            .iter()
            .map(|route| route.success_probability)
            .fold(0.0_f64, f64::max);
        for route in plan.active {
            // What getting past the first hop is worth: the rest of the
            // route's chance, and the value of arriving when it would.
            let worth = route.downstream_probability() * request.value_at(route.arrival);
            let hop = Handoff {
                visited: &visited,
                expires_at: bundle.expires_at(),
                object: &object,
                routed_len,
                alternative,
                worth,
            };
            self.send_on(now, r, route, hop, out);
        }
    }

    /// The links from this station that cannot carry anything now: the
    /// radio or the modem is down, or the internet peer is not linked.
    fn closed_now(&self, now: u64) -> Vec<LinkKey> {
        let me = self.id.me;
        let contacts = self
            .graph
            .outgoing(me, now)
            .map(|contact| (contact.key.to, contact.key.bearer));
        let known = self.graph.links_from(me).map(|(to, bearer, _)| (to, bearer));
        contacts
            .chain(known)
            .filter(|&(to, bearer)| match bearer {
                Bearer::Radio => !self.radio_up,
                Bearer::Internet => !self.linked(to),
                Bearer::Modem => !self.modem_up(),
            })
            .map(|(to, bearer)| LinkKey { from: me, to, bearer })
            .collect()
    }

    /// Hand `r` to the first hop of `route` if it leaves now; if it leaves
    /// later, hold `r` until then, or until the first hop is heard. A plan
    /// for later is a forecast: it is made again at least every
    /// [`REPLAN_SECS`] with what has been learned meanwhile.
    fn send_on(&mut self, now: u64, r: &Record, route: Route, hop: Handoff<'_>, out: &mut Vec<Command>) {
        let Some(first) = route.hops.first() else {
            return;
        };
        let (bearer, peer) = (first.contact.bearer, first.contact.to);
        if first.depart > now {
            let again = first.depart.min(now + REPLAN_SECS);
            if self.store.defer(r.id, again, Some(peer)).unwrap_or(false) {
                log(format!(
                    "{} to {} waits {} min for {peer} by {} ({:.0}% then)",
                    short(&r.id),
                    r.final_destination(),
                    (first.depart - now).div_ceil(60),
                    bearer.name(),
                    route.first_hop_probability * 100.0
                ));
            }
            return;
        }
        let available = match bearer {
            Bearer::Radio => self.radio_up,
            Bearer::Internet => self.linked(peer),
            Bearer::Modem => self.modem_up(),
        };
        if !available || self.in_flight.contains_key(&(r.id, peer)) {
            return;
        }
        let Handoff {
            visited,
            expires_at,
            object,
            routed_len,
            alternative,
            worth,
        } = hop;
        let held = route.contacts();
        if self.graph.reserve_many(&held, routed_len).is_err() {
            return;
        }
        if !self.store.set_next_hop(r.id, peer).unwrap_or(false) {
            self.graph.release_many(&held, routed_len);
            return;
        }
        let wire_object = if r.direction == Direction::Relay {
            let mut path = visited.to_vec();
            path.push(self.id.me);
            match wrap_routed(r.hop_count.unwrap_or(0) + 1, &path, object) {
                Ok(wrapped) => wrapped,
                Err(error) => {
                    self.graph.release_many(&held, routed_len);
                    let _ = self.store.clear_next_hop(r.id, peer);
                    log(format!("cannot route {}: {error}", short(&r.id)));
                    return;
                }
            }
        } else {
            object.to_vec()
        };
        log(format!(
            "sending {} toward {} via {peer} by {} (attempt {})",
            short(&r.id),
            r.final_destination(),
            bearer.name(),
            r.attempts + 1
        ));
        let downstream = route.downstream_probability();
        self.in_flight.insert(
            (r.id, peer),
            InFlight {
                bearer,
                peer,
                route,
                object_bytes: routed_len,
                downstream,
                alternative,
                expires_at,
            },
        );
        match bearer {
            Bearer::Radio => {
                self.radio_ids
                    .insert((hm_xfer::object_id(&wire_object), peer), r.id);
                let link = LinkKey {
                    from: self.id.me,
                    to: peer,
                    bearer: Bearer::Radio,
                };
                let price = self.settings.costs.airtime(self.channel_busy)[Bearer::Radio];
                out.push(Command::Radio(RadioCmd::Send {
                    object: wire_object,
                    to: peer,
                    precedence: r.precedence,
                    belief: PeerBelief {
                        erasure: self.beliefs.erasure(link, now),
                        open: self.beliefs.p_open(link, now, now),
                        airtime_cost: price / worth.max(1.0e-6),
                    },
                }));
            }
            Bearer::Internet => out.push(Command::NetDeliver {
                id: r.id,
                peer,
                object: wire_object,
            }),
            Bearer::Modem => out.push(Command::ModemDeliver {
                id: r.id,
                peer,
                object: wire_object,
            }),
        }
    }
}
