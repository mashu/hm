//! Contact adverts, signed, and passed on by Trickle: each station picks
//! its own moment in the interval, and a copy heard from another silences it.

use std::collections::BTreeMap;

use hm_ident::{Identity, PublicKey};
use hm_wire::{Callsign, ContactAdvert, ContactBearer};

pub const TRICKLE_MIN_MS: u64 = 5_000;

pub const TRICKLE_MAX_MS: u64 = 60 * 60 * 1_000;

pub const TRICKLE_REDUNDANCY: u8 = 2;

/// A live contact's advert is valid this long, and refreshed this often:
/// long enough to outlast a Trickle interval (at most an hour) with margin.
pub const LIVE_ADVERT_VALIDITY_SECS: u64 = 2 * 3600;

pub const LIVE_ADVERT_REFRESH_SECS: u64 = 30 * 60;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AdvertKey {
    pub origin: Callsign,
    pub peer: Callsign,
    pub bearer: ContactBearer,
    pub start: u32,
}

impl From<&ContactAdvert> for AdvertKey {
    fn from(advert: &ContactAdvert) -> Self {
        Self {
            origin: advert.origin,
            peer: advert.peer,
            bearer: advert.bearer,
            start: advert.start,
        }
    }
}

impl AdvertKey {
    pub(super) fn slot(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"hm/sync-slot/contact");
        hasher.update(&self.origin.to_bytes());
        hasher.update(&self.peer.to_bytes());
        hasher.update(&[self.bearer as u8]);
        hasher.update(&self.start.to_be_bytes());
        *hasher.finalize().as_bytes()
    }
}

/// Where a CONTACT advert came from, which decides where it may go.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AdvertSource {
    /// Our own claim; `on_air` if it belongs on the radio.
    Own { on_air: bool },
    /// Heard on the radio from another station.
    Radio,
    /// Received over an internet link.
    Internet,
}

/// An advert whose Trickle moment has come, and where to send it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueAdvert {
    pub advert: ContactAdvert,
    pub on_air: bool,
    pub internet: bool,
}

#[derive(Clone, Debug)]
struct TrickleEntry {
    advert: ContactAdvert,
    /// Hash of what the advert says about the contact: not its sequence,
    /// validity or signature, which change on every refresh.
    digest: [u8; 32],
    interval_ms: u64,
    interval_started_ms: u64,
    transmit_at_ms: u64,
    /// Consistent copies heard this interval, on the radio and over the internet.
    heard_on_air: u8,
    heard_on_net: u8,
    considered: bool,
    /// Whether it may go on the radio: heard there, or our own radio claim.
    on_air: bool,
}

/// RFC 6206-style suppression for versioned CONTACT adverts.
///
/// An advert keeps its identity ([`AdvertKey`]) across refreshes. A refresh
/// that says the same thing with a newer sequence number replaces the copy
/// we pass on, but is consistent: it neither resets the interval nor counts
/// as news. Each station picks its moment in an interval from its own `salt`,
/// so the stations that heard an advert at the same instant do not all send
/// it again at the same instant, where they would collide and never hear
/// each other's copy.
pub struct Trickle {
    entries: BTreeMap<AdvertKey, TrickleEntry>,
    min_ms: u64,
    max_ms: u64,
    redundancy: u8,
    salt: u64,
}

impl Default for Trickle {
    fn default() -> Self {
        Self::new(TRICKLE_MIN_MS, TRICKLE_MAX_MS, TRICKLE_REDUNDANCY)
            .expect("control-plane constants are valid")
    }
}

impl Trickle {
    pub fn new(min_ms: u64, max_ms: u64, redundancy: u8) -> Result<Self, &'static str> {
        if min_ms < 2 || max_ms < min_ms || redundancy == 0 {
            return Err("invalid Trickle parameters");
        }
        Ok(Self {
            entries: BTreeMap::new(),
            min_ms,
            max_ms,
            redundancy,
            salt: 0,
        })
    }

    /// This station's own randomness for picking moments in an interval.
    pub fn with_salt(mut self, salt: u64) -> Self {
        self.salt = salt;
        self
    }

    /// Observe a verified advert. Returns true only for new content.
    pub fn observe(
        &mut self,
        advert: ContactAdvert,
        now_ms: u64,
        source: AdvertSource,
    ) -> Result<bool, &'static str> {
        let digest = semantic_digest(&advert)?;
        let key = AdvertKey::from(&advert);
        // Internet links are never worth a radio transmission.
        let allowed_on_air = advert.bearer != ContactBearer::Internet;
        let on_air = allowed_on_air
            && match source {
                AdvertSource::Own { on_air } => on_air,
                AdvertSource::Radio => true,
                AdvertSource::Internet => false,
            };
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.on_air |= on_air;
            if entry.digest == digest {
                if newer_serial(advert.sequence, entry.advert.sequence) {
                    entry.advert = advert;
                }
                match source {
                    AdvertSource::Radio => entry.heard_on_air = entry.heard_on_air.saturating_add(1),
                    AdvertSource::Internet => entry.heard_on_net = entry.heard_on_net.saturating_add(1),
                    AdvertSource::Own { .. } => {}
                }
                return Ok(false);
            }
            if !newer_serial(advert.sequence, entry.advert.sequence) {
                return Ok(false);
            }
        }
        let on_air = on_air || self.entries.get(&key).is_some_and(|entry| entry.on_air);
        let interval_ms = self.min_ms;
        self.entries.insert(
            key,
            TrickleEntry {
                advert,
                digest,
                interval_ms,
                interval_started_ms: now_ms,
                transmit_at_ms: transmit_time(self.salt, digest, now_ms, interval_ms),
                heard_on_air: 0,
                heard_on_net: 0,
                considered: false,
                on_air,
            },
        );
        Ok(true)
    }

    /// Adverts whose moment in their interval has arrived, and where each may
    /// still go: nowhere a consistent copy was heard often enough this interval.
    pub fn poll(&mut self, now_ms: u64) -> Vec<DueAdvert> {
        let mut due = Vec::new();
        for entry in self.entries.values_mut() {
            advance_interval(entry, now_ms, self.max_ms, self.salt);
            if !entry.considered && now_ms >= entry.transmit_at_ms {
                entry.considered = true;
                let on_air = entry.on_air && entry.heard_on_air < self.redundancy;
                let internet = entry.heard_on_net < self.redundancy;
                if on_air || internet {
                    due.push(DueAdvert {
                        advert: entry.advert.clone(),
                        on_air,
                        internet,
                    });
                }
            }
        }
        due
    }

    #[cfg(test)]
    pub fn next_deadline(&self) -> Option<u64> {
        self.entries
            .values()
            .filter(|entry| !entry.considered)
            .map(|entry| entry.transmit_at_ms)
            .min()
    }

    pub fn expire(&mut self, unix_now: u64) {
        self.entries
            .retain(|_, entry| u64::from(entry.advert.end) > unix_now);
    }

    pub fn adverts(&self) -> impl Iterator<Item = &ContactAdvert> {
        self.entries.values().map(|entry| &entry.advert)
    }
}

/// RFC 1982 serial-number order, as the contact graph uses for sequences.
pub(super) fn newer_serial(candidate: u32, current: u32) -> bool {
    candidate != current && candidate.wrapping_sub(current) < 0x8000_0000
}

/// What an advert says about its contact: everything but the sequence number,
/// the end of its validity and the signature.
fn semantic_digest(advert: &ContactAdvert) -> Result<[u8; 32], &'static str> {
    let encoded = advert.encode().map_err(|_| "invalid contact advert")?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&encoded[1..7]);
    hasher.update(&encoded[11..15]);
    hasher.update(&encoded[19..37]);
    Ok(*hasher.finalize().as_bytes())
}

fn transmit_time(salt: u64, digest: [u8; 32], started_ms: u64, interval_ms: u64) -> u64 {
    let half = interval_ms / 2;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hm/trickle/v1");
    hasher.update(&salt.to_be_bytes());
    hasher.update(&digest);
    hasher.update(&started_ms.to_be_bytes());
    let word = u64::from_be_bytes(hasher.finalize().as_bytes()[..8].try_into().expect("eight bytes"));
    started_ms + half + word % (interval_ms - half)
}

fn advance_interval(entry: &mut TrickleEntry, now_ms: u64, max_ms: u64, salt: u64) {
    while now_ms >= entry.interval_started_ms.saturating_add(entry.interval_ms) {
        entry.interval_started_ms = entry.interval_started_ms.saturating_add(entry.interval_ms);
        entry.interval_ms = entry.interval_ms.saturating_mul(2).min(max_ms);
        entry.transmit_at_ms =
            transmit_time(salt, entry.digest, entry.interval_started_ms, entry.interval_ms);
        entry.heard_on_air = 0;
        entry.heard_on_net = 0;
        entry.considered = false;
    }
}

pub fn sign_contact(identity: &Identity, mut advert: ContactAdvert) -> Result<ContactAdvert, &'static str> {
    advert.signature = [0; 64];
    let statement = advert.signing_statement().map_err(|_| "invalid contact advert")?;
    advert.signature = identity.sign(&statement);
    Ok(advert)
}

pub fn verify_contact(advert: &ContactAdvert, key: PublicKey) -> bool {
    advert
        .signing_statement()
        .ok()
        .is_some_and(|statement| key.verify(&statement, &advert.signature).is_ok())
}
