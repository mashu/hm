//! Bundles: one message (mail, chat, form, bulletin, position or receipt).
//!
//! A bundle is a CBOR map sealed in an [`hm_ident::Envelope`] under the
//! [`hm_ident::BUNDLE`] domain. Its id is the BLAKE3 hash of the exact signed
//! bytes, so duplicate detection, resume and receipts all key on it, and relays
//! can forward bundles carrying fields they do not understand.
//!
//! Receiving is two-step on purpose: [`Opened`] gives routing information
//! (recipients, precedence, expiry) without a key; [`Opened::verify`] needs
//! the sender's key from their binding record.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod types;

use alloc::string::String;
use alloc::vec::Vec;

use hm_ident::{Envelope, IdentError, Identity, PublicKey, BUNDLE};
use hm_wire::{Callsign, ObjectId};
use minicbor::{Decode, Encode};

pub use types::{
    Address, Body, Codec, Kind, PartRef, Precedence, MAX_DECOMPRESSED_BODY, ZSTD_DICTIONARY,
    ZSTD_DICTIONARY_BLAKE3,
};

/// Current bundle format version.
pub const BUNDLE_VERSION: u8 = 0;
/// Most recipients one bundle may name.
pub const MAX_RECIPIENTS: usize = 32;
/// Longest subject in bytes.
pub const MAX_SUBJECT: usize = 128;
/// Longest group or tactical name in bytes.
pub const MAX_NAME: usize = 32;
/// Longest email address in bytes.
pub const MAX_EMAIL: usize = 254;
/// Default route hop limit when key 11 is absent.
pub const DEFAULT_MAX_HOPS: u8 = 8;
/// Protocol-wide route hop limit.
pub const MAX_HOPS: u8 = 16;

/// Errors from building, sealing or opening bundles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleError {
    Ident(IdentError),
    Decode(String),
    UnsupportedVersion(u8),
    /// Content violates a rule (message says which).
    Invalid(&'static str),
    /// Body codec not supported by this implementation.
    UnsupportedCodec(u8),
    /// Body is not valid UTF-8.
    NotText,
}

impl core::fmt::Display for BundleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BundleError::Ident(e) => write!(f, "{e}"),
            BundleError::Decode(m) => write!(f, "decode error: {m}"),
            BundleError::UnsupportedVersion(v) => write!(f, "unsupported bundle version {v}"),
            BundleError::Invalid(m) => write!(f, "invalid bundle: {m}"),
            BundleError::UnsupportedCodec(c) => write!(f, "unsupported body codec {c}"),
            BundleError::NotText => f.write_str("body is not UTF-8 text"),
        }
    }
}

impl core::error::Error for BundleError {}

impl From<IdentError> for BundleError {
    fn from(e: IdentError) -> Self {
        BundleError::Ident(e)
    }
}

impl From<minicbor::decode::Error> for BundleError {
    fn from(e: minicbor::decode::Error) -> Self {
        BundleError::Decode(alloc::format!("{e}"))
    }
}

/// One message. CBOR map with integer keys; see `SPEC.md` for the table.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Bundle {
    #[n(0)]
    pub v: u8,
    #[n(1)]
    pub from: Callsign,
    #[n(2)]
    pub to: Vec<Address>,
    #[n(3)]
    pub kind: Kind,
    /// Absent means [`Precedence::Routine`].
    #[n(4)]
    pub prec: Option<Precedence>,
    /// Unix seconds.
    #[n(5)]
    pub created: u64,
    /// Seconds after `created` when relays may drop the bundle.
    #[n(6)]
    pub ttl: u32,
    #[n(7)]
    pub subject: Option<String>,
    #[n(8)]
    pub body: Option<Body>,
    #[n(9)]
    pub parts: Option<Vec<PartRef>>,
    #[n(10)]
    pub reply_to: Option<ObjectId>,
    /// Absent means [`DEFAULT_MAX_HOPS`].
    #[n(11)]
    pub max_hops: Option<u8>,
}

impl Bundle {
    pub fn new(from: Callsign, kind: Kind, created: u64, ttl: u32) -> Bundle {
        Bundle {
            v: BUNDLE_VERSION,
            from,
            to: Vec::new(),
            kind,
            prec: None,
            created,
            ttl,
            subject: None,
            body: None,
            parts: None,
            reply_to: None,
            max_hops: None,
        }
    }

    /// A delivery receipt for bundle `id`, addressed back to its sender.
    pub fn receipt(
        from: Callsign,
        original_sender: Callsign,
        id: ObjectId,
        created: u64,
        ttl: u32,
    ) -> Bundle {
        Bundle::new(from, Kind::Receipt, created, ttl)
            .to(Address::Station(original_sender))
            .in_reply_to(id)
    }

    pub fn to(mut self, a: Address) -> Bundle {
        self.to.push(a);
        self
    }

    pub fn with_subject(mut self, s: &str) -> Bundle {
        self.subject = Some(String::from(s));
        self
    }

    pub fn with_text(mut self, s: &str) -> Bundle {
        self.body = Some(Body::text(s));
        self
    }

    pub fn with_precedence(mut self, p: Precedence) -> Bundle {
        self.prec = if p == Precedence::Routine { None } else { Some(p) };
        self
    }

    pub fn with_part(mut self, p: PartRef) -> Bundle {
        self.parts.get_or_insert_with(Vec::new).push(p);
        self
    }

    pub fn in_reply_to(mut self, id: ObjectId) -> Bundle {
        self.reply_to = Some(id);
        self
    }

    pub fn with_max_hops(mut self, max_hops: u8) -> Bundle {
        self.max_hops = (max_hops != DEFAULT_MAX_HOPS).then_some(max_hops);
        self
    }

    pub fn max_hops(&self) -> u8 {
        self.max_hops.unwrap_or(DEFAULT_MAX_HOPS)
    }

    pub fn precedence(&self) -> Precedence {
        self.prec.unwrap_or(Precedence::Routine)
    }

    /// Unix second after which the bundle may be dropped.
    pub fn expires_at(&self) -> u64 {
        self.created.saturating_add(self.ttl as u64)
    }

    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at()
    }

    /// Check every rule a sender must follow.
    pub fn validate(&self) -> Result<(), BundleError> {
        if self.v != BUNDLE_VERSION {
            return Err(BundleError::UnsupportedVersion(self.v));
        }
        if self.to.is_empty() || self.to.len() > MAX_RECIPIENTS {
            return Err(BundleError::Invalid("recipient count out of range"));
        }
        for a in &self.to {
            a.validate()?;
        }
        if self.ttl == 0 {
            return Err(BundleError::Invalid("ttl must be positive"));
        }
        if self
            .subject
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > MAX_SUBJECT)
        {
            return Err(BundleError::Invalid("subject length out of range"));
        }
        if self.parts.as_ref().is_some_and(|p| p.is_empty()) {
            return Err(BundleError::Invalid("empty lists must be omitted"));
        }
        if self.prec == Some(Precedence::Routine) {
            return Err(BundleError::Invalid("routine precedence must be omitted"));
        }
        if self.max_hops == Some(DEFAULT_MAX_HOPS) {
            return Err(BundleError::Invalid("default max_hops must be omitted"));
        }
        if !(1..=MAX_HOPS).contains(&self.max_hops()) {
            return Err(BundleError::Invalid("max_hops out of range"));
        }
        if self.kind == Kind::Receipt {
            self.validate_receipt()?;
        }
        Ok(())
    }

    pub fn validate_receipt(&self) -> Result<(), BundleError> {
        if self.kind != Kind::Receipt {
            return Err(BundleError::Invalid("not a receipt"));
        }
        if self.reply_to.is_none() {
            return Err(BundleError::Invalid("a receipt must name the bundle it confirms"));
        }
        if self.to.len() != 1 || !matches!(self.to.first(), Some(Address::Station(_))) {
            return Err(BundleError::Invalid(
                "a receipt must have exactly one station recipient",
            ));
        }
        if self.subject.is_some() || self.body.is_some() || self.parts.is_some() {
            return Err(BundleError::Invalid("a receipt must not carry content"));
        }
        Ok(())
    }

    /// Validate, encode and sign.
    pub fn seal(self, identity: &Identity) -> Result<SignedBundle, BundleError> {
        self.validate()?;
        let raw = minicbor::to_vec(&self).map_err(|_| BundleError::Invalid("encoding failed"))?;
        let envelope = Envelope::seal(&BUNDLE, raw, identity);
        let id = envelope.id(&BUNDLE);
        Ok(SignedBundle {
            envelope,
            bundle: self,
            id,
        })
    }
}

/// A decoded bundle whose signature has not been checked yet.
///
/// Relays may use it for routing decisions; nothing should be shown to a user
/// or acknowledged until [`Opened::verify`] succeeds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub envelope: Envelope,
    pub bundle: Bundle,
    pub id: ObjectId,
}

impl Opened {
    /// Decode envelope and bundle from wire bytes.
    pub fn decode(bytes: &[u8]) -> Result<Opened, BundleError> {
        let envelope = Envelope::decode(bytes)?;
        let bundle: Bundle = minicbor::decode(&envelope.raw)?;
        if bundle.v != BUNDLE_VERSION {
            return Err(BundleError::UnsupportedVersion(bundle.v));
        }
        let id = envelope.id(&BUNDLE);
        Ok(Opened { envelope, bundle, id })
    }

    /// Verify with the sender's key (from the binding record of `bundle.from`).
    pub fn verify(self, sender_key: &PublicKey) -> Result<SignedBundle, BundleError> {
        self.envelope.verify(&BUNDLE, sender_key)?;
        Ok(SignedBundle {
            envelope: self.envelope,
            bundle: self.bundle,
            id: self.id,
        })
    }
}

/// A bundle whose signature has been verified (or which we signed ourselves).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedBundle {
    envelope: Envelope,
    bundle: Bundle,
    id: ObjectId,
}

impl SignedBundle {
    pub fn id(&self) -> ObjectId {
        self.id
    }

    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    /// Wire bytes, exactly as signed.
    pub fn to_vec(&self) -> Vec<u8> {
        self.envelope.to_vec()
    }
}

#[cfg(test)]
mod tests;
