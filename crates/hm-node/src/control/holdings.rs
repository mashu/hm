//! Pairwise holdings reconciliation: a FILTER of what the receiver has, an
//! OFFER of what it lacks, a WANT of what it asks for.

use hm_wire::{Callsign, ObjectId, SyncFilter, SyncOffer, MAX_FILTER_BYTES, MAX_OFFER};

const FILTER_FALSE_POSITIVE: f64 = 0.01;

pub const PAIRWISE_STATE_SECS: u64 = 5 * 60;

/// Build a compact Bloom filter with approximately one-percent false
/// positives, bounded by the wire maximum.
pub fn holdings_filter(scope: u8, salt: u32, ids: &[ObjectId]) -> Result<SyncFilter, &'static str> {
    let count = ids.len().max(1) as f64;
    let ideal_bits = (-count * FILTER_FALSE_POSITIVE.ln() / core::f64::consts::LN_2.powi(2)).ceil() as usize;
    let bytes = ideal_bits.div_ceil(8).clamp(1, MAX_FILTER_BYTES);
    let bits = bytes * 8;
    let hashes = ((bits as f64 / count) * core::f64::consts::LN_2)
        .round()
        .clamp(1.0, 16.0) as u8;
    let mut filter = SyncFilter::new(scope, hashes, salt, bytes).map_err(|_| "invalid holdings filter")?;
    for id in ids {
        filter.insert(id).map_err(|_| "invalid holdings filter")?;
    }
    Ok(filter)
}

pub fn offer_pages(ids: &[ObjectId], filter: &SyncFilter) -> Result<Vec<SyncOffer>, &'static str> {
    let missing = ids
        .iter()
        .filter_map(|id| match filter.contains(id) {
            Ok(false) => Some(Ok(id.prefix8())),
            Ok(true) => None,
            Err(_) => Some(Err("invalid holdings filter")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(missing
        .chunks(MAX_OFFER)
        .map(|prefixes| SyncOffer {
            scope: filter.scope,
            prefixes: prefixes.to_vec(),
        })
        .collect())
}

pub(super) fn sync_salt(peer: Callsign, now: u64) -> u32 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hm/sync/salt/v0");
    hasher.update(&peer.to_bytes());
    hasher.update(&(now / PAIRWISE_STATE_SECS).to_be_bytes());
    u32::from_be_bytes(hasher.finalize().as_bytes()[..4].try_into().expect("four bytes"))
}
