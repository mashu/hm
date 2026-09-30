//! Low-airtime Phase 2 control-plane primitives.
//!
//! CONTACT adverts use Trickle suppression. Holdings reconciliation is
//! pairwise: FILTER (what the receiver already has), OFFER (enumerable ids),
//! then WANT (the missing subset).
//!
//! On a shared radio channel the control plane must cost a bounded share of
//! the channel however many stations share it. So:
//!
//! - Beacons are the radio topology: each says who its sender hears, which
//!   gives every listener its one- and two-hop neighbourhood. Their rate falls
//!   as more stations share the channel ([`beacon_interval_ms`]).
//! - Live contacts (who hears whom right now) are advertised to the internet
//!   core only, where gateways need them to route into radio areas. On the
//!   radio they would repeat what the beacons say, once per pair of stations.
//! - What does go on air (scheduled contacts) goes by Trickle, with each
//!   station choosing its own moment in the interval, so a copy heard from
//!   one station silences the others.
//! - The stations heard share one control budget ([`control_share_permyriad`]),
//!   and a SYNC frame waiting for it is replaced by a newer one about the same
//!   thing, or dropped once what it says has expired ([`SyncQueue`]).
//! - Holdings are pulled over the radio only from stations whose beacon says
//!   they hold something ([`radio_pull_due`]).

mod budget;
mod holdings;
mod sync_queue;
mod trickle;

use std::collections::BTreeMap;

use hm_ident::PublicKey;
use hm_route::{ContactGraph, Merge};
use hm_store::{Direction, State, Store};
use hm_wire::{Callsign, ContactAdvert, ObjectId, SyncMessage, SyncWant};

use crate::Trust;
use holdings::sync_salt;

pub use budget::{
    beacon_interval_ms, control_share_permyriad, live_window_secs, radio_pull_due, ControlBudget,
    BEACON_CHANNEL_PERMYRIAD, CONTROL_BUDGET_PERMYRIAD, CONTROL_BUDGET_WINDOW_MS, RADIO_PULL_SECS,
};
pub use holdings::{holdings_filter, offer_pages, PAIRWISE_STATE_SECS};
pub use sync_queue::{SyncQueue, SYNC_QUEUE_FRAMES};
pub use trickle::{
    sign_contact, verify_contact, AdvertKey, AdvertSource, Trickle, LIVE_ADVERT_REFRESH_SECS,
    LIVE_ADVERT_VALIDITY_SECS, TRICKLE_MAX_MS, TRICKLE_MIN_MS, TRICKLE_REDUNDANCY,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Reply(Vec<u8>),
    Requested { id: ObjectId, peer: Callsign },
}

/// An advert due for dissemination, encoded, and where it may go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueContact {
    pub payload: Vec<u8>,
    pub on_air: bool,
    pub internet: bool,
}

/// Contact dissemination and pairwise holdings state. Transport remains in
/// the node so the same logic is shared by radio and internet neighbours.
#[derive(Default)]
pub struct ControlPlane {
    trickle: Trickle,
    offered: BTreeMap<(Callsign, [u8; 8]), u64>,
    wanted: BTreeMap<(ObjectId, Callsign), u64>,
}

impl ControlPlane {
    /// The control plane of station `me`, whose callsign seeds its Trickle moments.
    pub fn for_station(me: Callsign) -> Self {
        Self {
            trickle: Trickle::default().with_salt(me.packed()),
            ..Self::default()
        }
    }

    /// Our own claim. `on_air`: it belongs on the radio as well as the internet
    /// (a scheduled contact); live contacts do not (see the module notes).
    pub fn observe_local(&mut self, advert: ContactAdvert, now_ms: u64, on_air: bool) -> Result<(), String> {
        self.trickle
            .observe(advert, now_ms, AdvertSource::Own { on_air })
            .map(|_| ())
            .map_err(str::to_string)
    }

    pub fn contact_messages(&self) -> Result<Vec<Vec<u8>>, String> {
        self.trickle
            .adverts()
            .map(|advert| {
                SyncMessage::Contact(advert.clone())
                    .encode()
                    .map_err(|error| error.to_string())
            })
            .collect()
    }

    pub fn due_contacts(&mut self, now_ms: u64, unix_now: u64) -> Result<Vec<DueContact>, String> {
        self.trickle.expire(unix_now);
        self.trickle
            .poll(now_ms)
            .into_iter()
            .map(|due| {
                Ok(DueContact {
                    payload: SyncMessage::Contact(due.advert)
                        .encode()
                        .map_err(|error| error.to_string())?,
                    on_air: due.on_air,
                    internet: due.internet,
                })
            })
            .collect()
    }

    pub fn filters(
        &self,
        peer: Callsign,
        store: &Store,
        relayable: bool,
        now: u64,
    ) -> Result<Vec<Vec<u8>>, String> {
        let ids = store.object_ids().map_err(|error| error.to_string())?;
        let salt = sync_salt(peer, now);
        let mut messages = vec![SyncMessage::Filter(holdings_filter(0, salt, &ids)?)
            .encode()
            .map_err(|error| error.to_string())?];
        if relayable {
            messages.push(
                SyncMessage::Filter(holdings_filter(1, salt ^ 0xA5A5_5A5A, &ids)?)
                    .encode()
                    .map_err(|error| error.to_string())?,
            );
        }
        Ok(messages)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn receive(
        &mut self,
        from: Callsign,
        payload: &[u8],
        trust: &Trust,
        // Transport-authenticated key for `from` (QUIC session on an open hub).
        peer_key: Option<PublicKey>,
        local: (Callsign, PublicKey),
        store: &Store,
        graph: &mut ContactGraph,
        accept_relay: bool,
        // Heard on the radio (else over an internet link).
        over_radio: bool,
        now: u64,
    ) -> Result<Vec<ControlAction>, String> {
        // RF SYNC still needs a trust entry. Internet open-hub dialers are
        // authenticated by their QUIC certificate (`peer_key`) instead.
        if trust.key_for(from).is_none() && peer_key.is_none() {
            return Err(format!("untrusted SYNC peer {from}"));
        }
        self.prune_pairwise(now);
        match SyncMessage::decode(payload).map_err(|error| error.to_string())? {
            SyncMessage::Contact(advert) => {
                let key = if advert.origin == local.0 {
                    local.1
                } else if let Some(key) = trust.key_for(advert.origin) {
                    key
                } else if advert.origin == from {
                    peer_key.ok_or_else(|| format!("untrusted CONTACT origin {}", advert.origin))?
                } else {
                    return Err(format!("untrusted CONTACT origin {}", advert.origin));
                };
                if !verify_contact(&advert, key) {
                    return Err(format!("bad CONTACT signature from {}", advert.origin));
                }
                match graph
                    .merge_advert(&advert, now)
                    .map_err(|error| error.to_string())?
                {
                    Merge::Inserted | Merge::Updated | Merge::Duplicate => {
                        let source = if over_radio {
                            AdvertSource::Radio
                        } else {
                            AdvertSource::Internet
                        };
                        self.trickle
                            .observe(advert, now.saturating_mul(1_000), source)
                            .map_err(str::to_string)?;
                    }
                    Merge::Stale => {}
                }
                Ok(vec![])
            }
            SyncMessage::Filter(filter) => {
                // Holdings to carry on are offered over the internet only
                // (see `Node::send_filters`); on the radio, a station's own
                // mail and the bulletins.
                let ids = store
                    .holding_ids(from, filter.scope == 1 && !over_radio, now)
                    .map_err(|error| error.to_string())?;
                let offers = offer_pages(&ids, &filter)?;
                let expires = now.saturating_add(PAIRWISE_STATE_SECS);
                let mut actions = Vec::with_capacity(offers.len());
                for offer in offers {
                    for prefix in &offer.prefixes {
                        self.offered.insert((from, *prefix), expires);
                    }
                    actions.push(ControlAction::Reply(
                        SyncMessage::Offer(offer)
                            .encode()
                            .map_err(|error| error.to_string())?,
                    ));
                }
                Ok(actions)
            }
            SyncMessage::Offer(offer) => {
                if offer.scope == 1 && !accept_relay {
                    return Ok(vec![]);
                }
                let mut prefixes = Vec::new();
                for prefix in offer.prefixes {
                    if store
                        .object_with_prefix(prefix)
                        .map_err(|error| error.to_string())?
                        .is_none()
                    {
                        prefixes.push(prefix);
                    }
                }
                if prefixes.is_empty() {
                    Ok(vec![])
                } else {
                    Ok(vec![ControlAction::Reply(
                        SyncMessage::Want(SyncWant { prefixes })
                            .encode()
                            .map_err(|error| error.to_string())?,
                    )])
                }
            }
            SyncMessage::Want(want) => {
                let mut actions = Vec::new();
                let bulletin_dest = Callsign::parse("ALL").expect("ALL is a valid callsign");
                for prefix in want.prefixes {
                    if self
                        .offered
                        .get(&(from, prefix))
                        .is_none_or(|expires| *expires <= now)
                    {
                        continue;
                    }
                    let Some((id, _)) = store
                        .object_with_prefix(prefix)
                        .map_err(|error| error.to_string())?
                    else {
                        continue;
                    };
                    let Some(record) = store.record(id).map_err(|error| error.to_string())? else {
                        continue;
                    };
                    let bulletin = record.direction == Direction::Out
                        && record.final_destination() == bulletin_dest
                        && matches!(record.state, State::Queued | State::Delivered);
                    if !bulletin
                        && (record.state != State::Queued
                            || !matches!(record.direction, Direction::Out | Direction::Relay))
                    {
                        continue;
                    }
                    self.wanted
                        .insert((id, from), now.saturating_add(PAIRWISE_STATE_SECS));
                    actions.push(ControlAction::Requested { id, peer: from });
                }
                Ok(actions)
            }
        }
    }

    pub fn target_for(&mut self, id: ObjectId, now: u64) -> Option<Callsign> {
        self.prune_pairwise(now);
        self.wanted
            .iter()
            .find_map(|((wanted, peer), _)| (*wanted == id).then_some(*peer))
    }

    pub fn clear_request(&mut self, id: ObjectId, peer: Callsign) {
        self.wanted.remove(&(id, peer));
    }

    fn prune_pairwise(&mut self, now: u64) {
        self.offered.retain(|_, expires| *expires > now);
        self.wanted.retain(|_, expires| *expires > now);
    }
}

#[cfg(test)]
mod tests;
