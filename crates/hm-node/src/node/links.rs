//! Internet links, our claims of live contacts and schedules, contact
//! adverts, and pairwise holdings SYNC.

use std::collections::BTreeSet;

use hm_ident::PublicKey;
use hm_model::{Bearer, LinkKey, LinkObservation};
use hm_route::LiveContact;
use hm_wire::{Callsign, Dest, FLAG_INTERNET};

use super::custody::wake_for;
use super::{Command, Node};
use crate::adverts::{live_advert, scheduled_advert};
use crate::control::{ControlAction, LIVE_ADVERT_REFRESH_SECS, LIVE_ADVERT_VALIDITY_SECS};
use crate::{log, short, RadioCmd};

/// Holdings are pulled from an internet peer at least this often.
const INTERNET_PULL_SECS: u64 = 5 * 60;
/// What an internet link is taken to carry.
const INTERNET_RATE_BPS: u32 = 10_000_000;
const INTERNET_CAPACITY: u64 = 64 * 1024 * 1024;
/// A repeat of the same SYNC complaint from one peer is not logged again
/// within this long.
const SYNC_COMPLAINT_SECS: u64 = 300;

/// Our claim of a live contact: since when it has lasted, and when we last
/// claimed it. Refreshes keep `since`, so peers see one contact whose
/// validity grows, not a new contact every time.
#[derive(Copy, Clone, Debug)]
pub(crate) struct LiveClaim {
    since: u64,
    claimed_at: u64,
}

impl LiveClaim {
    /// Make the claim due for refresh now (the flags it carries changed).
    pub fn expire(&mut self, now: u64) {
        self.claimed_at = now.saturating_sub(LIVE_ADVERT_REFRESH_SECS);
    }
}

impl Node {
    /// Publish signed claims for our own scheduled contacts, numbered from
    /// the schedule base. Peers keep the claim with the newest sequence
    /// number, so republishing with a higher base replaces what they hold,
    /// for example after the relay or mailbox flags changed.
    pub(super) fn advertise_schedules(&mut self, now: u64) {
        for (index, schedule) in self.id.schedules.iter().copied().enumerate() {
            let sequence = self.schedule_base.wrapping_add(index as u32);
            match scheduled_advert(
                self.id.me,
                &self.id.identity,
                self.id.has_internet,
                &self.settings.relay,
                schedule,
                sequence,
            ) {
                Ok(Some(advert)) => {
                    if let Err(error) = self
                        .control
                        .observe_local(advert, now.saturating_mul(1_000), true)
                    {
                        log(format!("ignored local CONTACT advert: {error}"));
                    }
                }
                Ok(None) => {}
                Err(error) => log(format!("ignored local CONTACT advert: {error}")),
            }
        }
    }

    /// Claim (or refresh) our live contact to `peer` over `bearer`, with our
    /// belief that a handoff over it completes. Live claims go to the
    /// internet core only: on the radio, beacons already tell every station
    /// in reach who hears whom.
    pub(super) fn claim_live(
        &mut self,
        peer: Callsign,
        bearer: Bearer,
        success: f64,
        rate_bps: u32,
        capacity_bytes: u64,
        now: u64,
    ) {
        let key = (peer, bearer);
        let claim = self.advertised_live.get(&key).copied();
        if claim.is_some_and(|c| now.saturating_sub(c.claimed_at) < LIVE_ADVERT_REFRESH_SECS) {
            return;
        }
        // The same stretch of contact while our last claim of it is still valid.
        let since = match claim {
            Some(c) if now.saturating_sub(c.claimed_at) < LIVE_ADVERT_VALIDITY_SECS => c.since,
            _ => now,
        };
        let advert = live_advert(
            self.id.me,
            &self.id.identity,
            self.id.has_internet,
            &self.settings.relay,
            peer,
            bearer,
            success,
            rate_bps,
            capacity_bytes,
            since,
            now,
        );
        match advert {
            Ok(advert) => match self
                .control
                .observe_local(advert, now.saturating_mul(1_000), false)
            {
                Ok(()) => {
                    self.advertised_live.insert(
                        key,
                        LiveClaim {
                            since,
                            claimed_at: now,
                        },
                    );
                }
                Err(error) => log(format!("ignored local CONTACT advert: {error}")),
            },
            Err(error) => log(format!("ignored local CONTACT advert: {error}")),
        }
    }

    /// The internet peers linked now: links that came up or went down are
    /// news, live links are contacts we claim, and holdings are pulled.
    pub(super) fn internet_links(&mut self, now: u64, current: BTreeSet<Callsign>, out: &mut Vec<Command>) {
        let me = self.id.me;
        let internet = |from, to| LinkKey {
            from,
            to,
            bearer: Bearer::Internet,
        };
        for peer in self.links.difference(&current) {
            self.beliefs
                .observe_link(internet(me, *peer), now, LinkObservation::Down);
            self.beliefs
                .observe_link(internet(*peer, me), now, LinkObservation::Down);
        }
        for peer in &current {
            for (from, to) in [(me, *peer), (*peer, me)] {
                if let Err(error) = self.graph.observe_live_link(LiveContact {
                    from,
                    to,
                    bearer: Bearer::Internet,
                    rate_bps: INTERNET_RATE_BPS,
                    capacity_bytes: INTERNET_CAPACITY,
                    flags: FLAG_INTERNET,
                    observed_at: now,
                }) {
                    log(format!("ignored internet contact {from} -> {to}: {error}"));
                }
            }
            let success = self.beliefs.link_success(internet(me, *peer), now, now);
            self.claim_live(
                *peer,
                Bearer::Internet,
                success,
                INTERNET_RATE_BPS,
                INTERNET_CAPACITY,
                now,
            );
            if !self.links.contains(peer) {
                self.beliefs
                    .observe_link(internet(me, *peer), now, LinkObservation::Up);
                self.beliefs
                    .observe_link(internet(*peer, me), now, LinkObservation::Up);
                wake_for(&self.store, *peer, "linked", now);
                match self.control.contact_messages() {
                    Ok(messages) => {
                        for payload in messages {
                            self.send_sync(Bearer::Internet, *peer, payload, out);
                        }
                    }
                    Err(error) => log(format!("could not encode CONTACT adverts: {error}")),
                }
                self.send_filters(*peer, Bearer::Internet, now, out);
            }
            let synced_at = self
                .last_pairwise_sync
                .get(&(*peer, Bearer::Internet))
                .copied()
                .unwrap_or(0);
            if now.saturating_sub(synced_at) >= INTERNET_PULL_SECS {
                self.send_filters(*peer, Bearer::Internet, now, out);
            }
        }
        self.links = current;
    }

    /// Contact adverts due for dissemination, on the air and to internet peers.
    pub(super) fn due_contacts(&mut self, now: u64, out: &mut Vec<Command>) {
        match self.control.due_contacts(now.saturating_mul(1_000), now) {
            Ok(due) => {
                for contact in due {
                    if self.radio_up && contact.on_air {
                        out.push(Command::Radio(RadioCmd::Sync {
                            to: Dest::Broadcast,
                            payload: contact.payload.clone(),
                        }));
                    }
                    if contact.internet {
                        for peer in &self.links {
                            out.push(Command::NetSync {
                                peer: *peer,
                                payload: contact.payload.clone(),
                            });
                        }
                    }
                }
            }
            Err(error) => log(format!("could not encode CONTACT advert: {error}")),
        }
    }

    /// Ask `peer` for what it holds for us (and for relaying, if we relay).
    pub(super) fn send_filters(&mut self, peer: Callsign, bearer: Bearer, now: u64, out: &mut Vec<Command>) {
        // Over the internet a relay also collects what it could carry on;
        // on the radio that would copy every holding to every relay in reach,
        // against the routes: a relay that hears a station tries what waits
        // for it anyway.
        let relaying =
            (self.settings.relay.enabled || self.settings.relay.mailbox) && bearer == Bearer::Internet;
        match self.control.filters(peer, &self.store, relaying, now) {
            Ok(filters) => {
                for payload in filters {
                    self.send_sync(bearer, peer, payload, out);
                }
                self.last_pairwise_sync.insert((peer, bearer), now);
            }
            Err(error) => log(format!("could not SYNC with {peer}: {error}")),
        }
    }

    pub(super) fn send_sync(&self, bearer: Bearer, to: Callsign, payload: Vec<u8>, out: &mut Vec<Command>) {
        match bearer {
            Bearer::Radio => out.push(Command::Radio(RadioCmd::Sync {
                to: Dest::Station(to),
                payload,
            })),
            Bearer::Internet => out.push(Command::NetSync { peer: to, payload }),
            Bearer::Modem => {}
        }
    }

    /// SYNC from `from` over `bearer`; `peer_key` is the key an internet link
    /// proved for it.
    pub(super) fn receive_sync(
        &mut self,
        now: u64,
        bearer: Bearer,
        from: Callsign,
        payload: &[u8],
        peer_key: Option<PublicKey>,
        out: &mut Vec<Command>,
    ) {
        let relaying = self.settings.relay.enabled || self.settings.relay.mailbox;
        let actions = match self.control.receive(
            from,
            payload,
            &self.settings.trust,
            peer_key,
            (self.id.me, self.id.identity.public()),
            &self.store,
            &mut self.graph,
            relaying,
            bearer == Bearer::Radio,
            now,
        ) {
            Ok(actions) => actions,
            Err(error) => {
                let repeat = self
                    .last_sync_ignore
                    .is_some_and(|(peer, at)| peer == from && now.saturating_sub(at) < SYNC_COMPLAINT_SECS);
                if !repeat {
                    log(format!("ignored SYNC from {from}: {error}"));
                    self.last_sync_ignore = Some((from, now));
                }
                return;
            }
        };
        for action in actions {
            match action {
                ControlAction::Reply(payload) => self.send_sync(bearer, from, payload, out),
                ControlAction::Requested { id, peer } => {
                    log(format!("{} requested custody of {}", peer, short(&id)));
                }
            }
        }
    }
}
