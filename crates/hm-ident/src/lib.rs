//! Station identities and everything that is signed.
//!
//! - [`Identity`] / [`PublicKey`]: Ed25519 keys.
//! - [`Envelope`]: the signed container used for every object. The signed
//!   bytes travel verbatim as a CBOR byte string, so relays forward and verify
//!   objects byte-for-byte, even ones carrying fields they do not understand.
//! - [`Domain`]: separates hashes and signatures of different object kinds.
//! - [`BindingRecord`] / [`SignedBinding`]: "this key speaks for this callsign".
//! - [`Attestation`]: another station vouching for a binding.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod binding;
mod envelope;
mod keys;

pub use binding::{Attestation, BindingRecord, SignedBinding, BINDING_VERSION};
pub use envelope::{Domain, Envelope, BINDING, BUNDLE, MAX_ENVELOPE_LEN};
pub use keys::{Identity, PublicKey};

/// Errors from signing, verifying and decoding signed objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentError {
    /// CBOR could not be decoded (message from the decoder).
    Decode(alloc::string::String),
    /// Input longer than the allowed maximum.
    TooLarge,
    /// Bytes remained after the object.
    Trailing,
    /// Signature does not verify for this key and domain.
    BadSignature,
    /// Public key bytes are not a valid Ed25519 point.
    BadKey,
    /// The signing identity does not match the key named in the record.
    KeyMismatch,
    /// Object version not supported by this implementation.
    UnsupportedVersion(u8),
    /// Record content violates a rule (message says which).
    Invalid(&'static str),
}

impl core::fmt::Display for IdentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IdentError::Decode(m) => write!(f, "decode error: {m}"),
            IdentError::TooLarge => f.write_str("object too large"),
            IdentError::Trailing => f.write_str("trailing bytes after object"),
            IdentError::BadSignature => f.write_str("signature does not verify"),
            IdentError::BadKey => f.write_str("invalid public key"),
            IdentError::KeyMismatch => f.write_str("signing key does not match record"),
            IdentError::UnsupportedVersion(v) => write!(f, "unsupported version {v}"),
            IdentError::Invalid(m) => write!(f, "invalid: {m}"),
        }
    }
}

impl core::error::Error for IdentError {}

impl From<minicbor::decode::Error> for IdentError {
    fn from(e: minicbor::decode::Error) -> Self {
        IdentError::Decode(alloc::format!("{e}"))
    }
}
