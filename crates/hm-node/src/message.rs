//! Objects received, opened and checked against the trusted keys.

use std::borrow::Cow;

use hm_bundle::{Bundle, Opened};
use hm_ident::PublicKey;
use hm_wire::{Callsign, ObjectId};
use hm_xfer::object_id;

use crate::Trust;

/// Sealed bulletin object size cap (smaller than routine mail on air).
pub const MAX_BULLETIN_BYTES: usize = 4096;
/// Inbound bulletins kept per origin in a rolling hour (flood guard).
pub const MAX_INBOUND_BULLETINS_PER_ORIGIN_HOUR: usize = 12;

/// What the trusted keys could say about a received bundle's signature.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Verification {
    /// Signed by the key listed for the sender.
    Verified,
    /// No key for the sender, so the signature was not checked.
    Unverified,
    /// A key is listed and the signature does not match it.
    BadSignature,
}

/// One object received, decoded as far as it will go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: ObjectId,
    pub via: Callsign,
    pub verification: Verification,
    pub bundle: Option<Bundle>,
    pub error: Option<String>,
}

impl Message {
    /// UTF-8 body, when the bundle has one this build understands.
    pub fn text(&self) -> Option<Cow<'_, str>> {
        self.bundle.as_ref()?.body.as_ref()?.as_text().ok()
    }
}

/// Decode `object` and check its signature against `trust`. `via` is who sent the frame.
/// When the sender is not in `trust`, `peer_key` (from an open-hub TLS session) may
/// still verify messages that claim to be from `via`.
pub fn open_message(via: Callsign, object: &[u8], trust: &Trust, peer_key: Option<&PublicKey>) -> Message {
    let opened = match Opened::decode(object) {
        Ok(opened) => opened,
        Err(e) => {
            return Message {
                id: object_id(object),
                via,
                verification: Verification::Unverified,
                bundle: None,
                error: Some(e.to_string()),
            };
        }
    };
    let from = opened.bundle.from;
    let verification = match trust.key_for(from) {
        Some(key) => match opened.clone().verify(&key) {
            Ok(_) => Verification::Verified,
            Err(_) => Verification::BadSignature,
        },
        None => match peer_key {
            Some(key) if from == via || from.base() == via.base() => match opened.clone().verify(key) {
                Ok(_) => Verification::Verified,
                Err(_) => Verification::BadSignature,
            },
            _ => Verification::Unverified,
        },
    };
    Message {
        id: opened.id,
        via,
        verification,
        bundle: Some(opened.bundle),
        error: None,
    }
}
