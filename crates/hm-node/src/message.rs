//! Objects received, opened and checked against the trusted keys.

use std::borrow::Cow;

use hm_bundle::{Address, Bundle, BundleError, Kind, Opened, Precedence, SignedBundle};
use hm_ident::{Identity, PublicKey};
use hm_wire::{Callsign, ObjectId};
use hm_xfer::object_id;

use crate::Trust;

/// Chat expires after an hour; mail after a week. Relays use this; the local
/// queue retries on its own schedule.
pub const CHAT_TTL: u32 = 3600;
pub const MAIL_TTL: u32 = 7 * 86_400;
/// Sealed bulletin object size cap (smaller than routine mail on air).
pub const MAX_BULLETIN_BYTES: usize = 4096;
/// Inbound bulletins kept per origin in a rolling hour (flood guard).
pub const MAX_INBOUND_BULLETINS_PER_ORIGIN_HOUR: usize = 12;

/// A text message to one station, before it is sealed.
#[derive(Copy, Clone, Debug)]
pub struct Draft<'a> {
    pub to: Callsign,
    pub text: &'a str,
    /// A subject makes it mail; otherwise it is chat.
    pub subject: Option<&'a str>,
    pub precedence: Precedence,
    /// The directed conversation sequence, for chat (ignored for mail).
    pub seq: Option<u64>,
}

impl Draft<'_> {
    /// Seal it as `me`, written at `now` (Unix seconds).
    pub fn seal(&self, identity: &Identity, me: Callsign, now: u64) -> Result<SignedBundle, BundleError> {
        let (kind, ttl) = match self.subject {
            Some(_) => (Kind::Mail, MAIL_TTL),
            None => (Kind::Chat, CHAT_TTL),
        };
        let mut bundle = Bundle::new(me, kind, now, ttl)
            .to(Address::Station(self.to))
            .with_text(self.text)
            .with_precedence(self.precedence);
        if let Some(subject) = self.subject {
            bundle = bundle.with_subject(subject);
        }
        if kind == Kind::Chat {
            if let Some(seq) = self.seq {
                bundle = bundle.with_seq(seq);
            }
        }
        bundle.seal(identity)
    }
}

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
