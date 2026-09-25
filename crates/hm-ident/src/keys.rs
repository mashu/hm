use core::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use minicbor::decode::{self, Decoder};
use minicbor::encode::{self, Encoder, Write};
use minicbor::{Decode, Encode};

use crate::IdentError;

/// An Ed25519 public key (32 bytes). Encoded in CBOR as a 32-byte string.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    /// Verify `sig` over `msg` with strict Ed25519 rules (no malleable or
    /// small-order signatures and keys).
    pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> Result<(), IdentError> {
        let vk = VerifyingKey::from_bytes(&self.0).map_err(|_| IdentError::BadKey)?;
        vk.verify_strict(msg, &Signature::from_bytes(sig))
            .map_err(|_| IdentError::BadSignature)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey(")?;
        for b in &self.0[..6] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

impl<C> Encode<C> for PublicKey {
    fn encode<W: Write>(&self, e: &mut Encoder<W>, _: &mut C) -> Result<(), encode::Error<W::Error>> {
        e.bytes(&self.0)?;
        Ok(())
    }
}

impl<'b, C> Decode<'b, C> for PublicKey {
    fn decode(d: &mut Decoder<'b>, _: &mut C) -> Result<Self, decode::Error> {
        let pos = d.position();
        let b = d.bytes()?;
        let arr: [u8; 32] = b
            .try_into()
            .map_err(|_| decode::Error::message("public key must be 32 bytes").at(pos))?;
        Ok(PublicKey(arr))
    }
}

/// A station's private signing identity.
///
/// Created from 32 secret bytes; the daemon generates them from the OS RNG and
/// the client offers a printable backup. The protocol crates never generate
/// keys themselves, so tests and simulations stay deterministic.
pub struct Identity {
    key: SigningKey,
}

impl Identity {
    pub fn from_secret(secret: [u8; 32]) -> Identity {
        Identity {
            key: SigningKey::from_bytes(&secret),
        }
    }

    /// The 32 secret bytes, for backup. Handle with care.
    pub fn secret(&self) -> [u8; 32] {
        self.key.to_bytes()
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.key.verifying_key().to_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({:?})", self.public())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let id = Identity::from_secret([7; 32]);
        let sig = id.sign(b"hello");
        assert!(id.public().verify(b"hello", &sig).is_ok());
        assert_eq!(id.public().verify(b"hellO", &sig), Err(IdentError::BadSignature));
        let other = Identity::from_secret([8; 32]);
        assert_eq!(
            other.public().verify(b"hello", &sig),
            Err(IdentError::BadSignature)
        );
    }

    #[test]
    fn secret_roundtrip() {
        let id = Identity::from_secret([9; 32]);
        assert_eq!(Identity::from_secret(id.secret()).public(), id.public());
    }
}
