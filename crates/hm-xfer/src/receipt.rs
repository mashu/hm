//! Object ids and the receipt a receiver signs.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use hm_ident::PublicKey;
use hm_wire::{Callsign, ObjectId};

/// BLAKE3 `derive_key` context for the object hash carried in OFFER.
pub const HASH_CONTEXT: &str = "hm-net 2026-09 xfer v0";

/// Domain prefix of the receipt signature.
pub const RECEIPT_PREFIX: &[u8] = b"hm/xfer-receipt/v0";

/// The statement a receiver signs to prove it holds object `id`.
pub fn receipt_statement(receiver: Callsign, sender: Callsign, session: u16, id: &ObjectId) -> Vec<u8> {
    let mut m = Vec::with_capacity(RECEIPT_PREFIX.len() + 6 + 6 + 2 + 32);
    m.extend_from_slice(RECEIPT_PREFIX);
    m.extend_from_slice(&receiver.to_bytes());
    m.extend_from_slice(&sender.to_bytes());
    m.extend_from_slice(&session.to_be_bytes());
    m.extend_from_slice(&id.0);
    m
}

/// The key for a station: its own, else its base callsign's.
pub(crate) fn key_for(keys: &BTreeMap<Callsign, PublicKey>, call: Callsign) -> Option<&PublicKey> {
    keys.get(&call).or_else(|| keys.get(&call.base()))
}

/// The hash a transfer is verified against.
pub fn object_id(bytes: &[u8]) -> ObjectId {
    ObjectId(blake3::derive_key(HASH_CONTEXT, bytes))
}
