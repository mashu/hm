//! Our signed contact adverts and the flags they carry.

use hm_ident::Identity;
use hm_model::Bearer;
use hm_route::ScheduledContact;
use hm_wire::{Callsign, ContactAdvert, ContactBearer, FLAG_INTERNET, FLAG_MAILBOX, FLAG_RELAY};

use super::control::{sign_contact, LIVE_ADVERT_VALIDITY_SECS};
use crate::config::RelaySettings;

/// The flags our beacons and adverts carry: internet, relay, mailbox.
pub(crate) fn advertised_flags(has_internet: bool, relay: &RelaySettings) -> u8 {
    let mut flags = if has_internet { FLAG_INTERNET } else { 0 };
    if relay.enabled {
        flags |= FLAG_RELAY;
    }
    if relay.mailbox {
        flags |= FLAG_MAILBOX;
    }
    flags
}

/// Our signed claim for one of our own scheduled contacts. The flags come from
/// the relay settings in use (`relay`), so a change made while the node runs is
/// advertised the next time the schedules are published.
pub(crate) fn scheduled_advert(
    me: Callsign,
    identity: &Identity,
    has_internet: bool,
    relay: &RelaySettings,
    schedule: ScheduledContact,
    sequence: u32,
) -> Result<Option<ContactAdvert>, String> {
    if schedule.from != me {
        return Ok(None);
    }
    let start = u32::try_from(schedule.start).map_err(|_| "contact start exceeds wire range")?;
    let end = u32::try_from(schedule.end).map_err(|_| "contact end exceeds wire range")?;
    let capacity_bytes =
        u32::try_from(schedule.capacity_bytes).map_err(|_| "contact capacity exceeds wire range")?;
    let advert = ContactAdvert {
        origin: me,
        sequence,
        start,
        end,
        peer: schedule.to,
        bearer: ContactBearer::from(schedule.bearer),
        success_permyriad: schedule.success_permyriad.unwrap_or(5_000),
        rate_bps: schedule.rate_bps,
        capacity_bytes,
        flags: schedule.flags | advertised_flags(has_internet, relay),
        signature: [0; 64],
    };
    sign_contact(identity, advert).map(Some).map_err(str::to_string)
}

/// Our signed claim of a live contact to `peer`, lasting since `since`:
/// numbered by `now`, valid for [`LIVE_ADVERT_VALIDITY_SECS`] from now, with
/// our belief that a handoff over it completes (`success`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn live_advert(
    me: Callsign,
    identity: &Identity,
    has_internet: bool,
    relay: &RelaySettings,
    peer: Callsign,
    bearer: Bearer,
    success: f64,
    rate_bps: u32,
    capacity_bytes: u64,
    since: u64,
    now: u64,
) -> Result<ContactAdvert, String> {
    let start = u32::try_from(since).map_err(|_| "contact start exceeds wire range")?;
    let sequence = u32::try_from(now).map_err(|_| "contact time exceeds wire range")?;
    let end = u32::try_from(now.saturating_add(LIVE_ADVERT_VALIDITY_SECS))
        .map_err(|_| "contact end exceeds wire range")?;
    let capacity_bytes = u32::try_from(capacity_bytes.min(u64::from(u32::MAX))).expect("bounded to u32");
    sign_contact(
        identity,
        ContactAdvert {
            origin: me,
            sequence,
            start,
            end,
            peer,
            bearer: ContactBearer::from(bearer),
            success_permyriad: (success.clamp(0.0, 1.0) * 10_000.0).round() as u16,
            rate_bps,
            capacity_bytes,
            flags: advertised_flags(has_internet, relay),
            signature: [0; 64],
        },
    )
    .map_err(str::to_string)
}
