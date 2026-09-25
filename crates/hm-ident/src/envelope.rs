use alloc::vec::Vec;

use hm_wire::ObjectId;
use minicbor::{Decoder, Encoder};

use crate::{IdentError, Identity, PublicKey};

/// Largest envelope accepted by [`Envelope::decode`] by default (64 KiB).
/// Attachments travel as separate objects, so messages themselves stay small.
pub const MAX_ENVELOPE_LEN: usize = 64 * 1024;

/// Separates the hashes and signatures of different object kinds, so a
/// signature made for one kind can never be replayed as another.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Domain {
    /// BLAKE3 `derive_key` context string used to compute the object id.
    pub hash_context: &'static str,
    /// Bytes prepended to the object id before signing.
    pub sig_prefix: &'static [u8],
}

/// Messages (mail, chat, forms, bulletins, receipts).
pub const BUNDLE: Domain = Domain {
    hash_context: "hm-net 2026-09 bundle id v0",
    sig_prefix: b"hm/bundle-sig/v0",
};

/// Callsign binding records.
pub const BINDING: Domain = Domain {
    hash_context: "hm-net 2026-09 binding id v0",
    sig_prefix: b"hm/binding-sig/v0",
};

/// A signed object: the exact signed bytes plus an Ed25519 signature.
///
/// Wire form: CBOR `array(2) [ bstr raw, bstr(64) sig ]`.
///
/// - Object id = `BLAKE3::derive_key(domain.hash_context, raw)`.
/// - Signature = `Ed25519(sig_prefix || id)`.
///
/// Ids and signatures cover `raw` exactly as transmitted. Nodes store and
/// forward the envelope bytes untouched and never re-encode signed content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub raw: Vec<u8>,
    pub sig: [u8; 64],
}

fn signing_message(domain: &Domain, id: &ObjectId) -> Vec<u8> {
    let mut m = Vec::with_capacity(domain.sig_prefix.len() + 32);
    m.extend_from_slice(domain.sig_prefix);
    m.extend_from_slice(&id.0);
    m
}

impl Envelope {
    /// Sign `raw` under `domain`.
    pub fn seal(domain: &Domain, raw: Vec<u8>, identity: &Identity) -> Envelope {
        let id = Envelope::compute_id(domain, &raw);
        let sig = identity.sign(&signing_message(domain, &id));
        Envelope { raw, sig }
    }

    pub fn compute_id(domain: &Domain, raw: &[u8]) -> ObjectId {
        ObjectId(blake3::derive_key(domain.hash_context, raw))
    }

    pub fn id(&self, domain: &Domain) -> ObjectId {
        Envelope::compute_id(domain, &self.raw)
    }

    /// Verify the signature; returns the object id on success.
    pub fn verify(&self, domain: &Domain, key: &PublicKey) -> Result<ObjectId, IdentError> {
        let id = self.id(domain);
        key.verify(&signing_message(domain, &id), &self.sig)?;
        Ok(id)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut e = Encoder::new(Vec::with_capacity(self.raw.len() + 72));
        // Writing into a Vec cannot fail.
        let _ = e
            .array(2)
            .and_then(|e| e.bytes(&self.raw))
            .and_then(|e| e.bytes(&self.sig));
        e.into_writer()
    }

    /// Decode with the default size limit.
    pub fn decode(bytes: &[u8]) -> Result<Envelope, IdentError> {
        Envelope::decode_limited(bytes, MAX_ENVELOPE_LEN)
    }

    pub fn decode_limited(bytes: &[u8], max_len: usize) -> Result<Envelope, IdentError> {
        if bytes.len() > max_len {
            return Err(IdentError::TooLarge);
        }
        let mut d = Decoder::new(bytes);
        if d.array()? != Some(2) {
            return Err(IdentError::Invalid("envelope must be a 2-element array"));
        }
        let raw = d.bytes()?.to_vec();
        let sig: [u8; 64] = d
            .bytes()?
            .try_into()
            .map_err(|_| IdentError::Invalid("signature must be 64 bytes"))?;
        if d.position() != bytes.len() {
            return Err(IdentError::Trailing);
        }
        Ok(Envelope { raw, sig })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use proptest::prelude::*;

    #[test]
    fn seal_verify_roundtrip() {
        let id = Identity::from_secret([1; 32]);
        let env = Envelope::seal(&BUNDLE, b"payload".to_vec(), &id);
        let bytes = env.to_vec();
        let back = Envelope::decode(&bytes).unwrap();
        assert_eq!(back, env);
        assert_eq!(back.verify(&BUNDLE, &id.public()).unwrap(), env.id(&BUNDLE));
    }

    #[test]
    fn domains_do_not_cross() {
        let id = Identity::from_secret([1; 32]);
        let env = Envelope::seal(&BUNDLE, b"payload".to_vec(), &id);
        assert_ne!(env.id(&BUNDLE), env.id(&BINDING));
        assert_eq!(env.verify(&BINDING, &id.public()), Err(IdentError::BadSignature));
    }

    #[test]
    fn any_bit_flip_breaks_the_signature() {
        let id = Identity::from_secret([2; 32]);
        let env = Envelope::seal(&BUNDLE, b"sixteen byte msg".to_vec(), &id);
        for i in 0..env.raw.len() {
            let mut t = env.clone();
            t.raw[i] ^= 0x01;
            assert!(t.verify(&BUNDLE, &id.public()).is_err());
        }
        for i in 0..64 {
            let mut t = env.clone();
            t.sig[i] ^= 0x80;
            assert!(t.verify(&BUNDLE, &id.public()).is_err());
        }
    }

    #[test]
    fn rejects_malformed() {
        let id = Identity::from_secret([3; 32]);
        let mut bytes = Envelope::seal(&BUNDLE, vec![1, 2, 3], &id).to_vec();
        bytes.push(0);
        assert_eq!(Envelope::decode(&bytes), Err(IdentError::Trailing));
        assert_eq!(Envelope::decode_limited(&bytes, 10), Err(IdentError::TooLarge));
        assert!(Envelope::decode(&[0x82, 0x40, 0x41, 0x00]).is_err());
        assert!(Envelope::decode(&[0x81, 0x40]).is_err());
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            if let Ok(env) = Envelope::decode(&bytes) {
                // The decoder may accept non-minimal CBOR lengths, so compare
                // decoded values rather than bytes.
                prop_assert_eq!(Envelope::decode(&env.to_vec()).unwrap(), env);
            }
        }
    }
}
