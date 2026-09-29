use alloc::vec::Vec;

use hm_wire::{Callsign, ObjectId};
use minicbor::{Decode, Encode};

use crate::{Envelope, IdentError, Identity, PublicKey, BINDING};

/// Current binding record format version.
pub const BINDING_VERSION: u8 = 0;

const ATTEST_PREFIX: &[u8] = b"hm/attest/v0";

/// "This key speaks for this callsign", signed by the key itself.
///
/// A callsign with an SSID binds that station only; one without binds every
/// SSID that has no record of its own. A record with a higher `seq` replaces
/// older ones.
///
/// CBOR map:
/// `0 v`, `1 callsign`, `2 key`, `3 seq`, `4 created` (Unix seconds),
/// `5 homes` (optional), `6 attestations` (optional).
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct BindingRecord {
    #[n(0)]
    pub v: u8,
    #[n(1)]
    pub callsign: Callsign,
    #[n(2)]
    pub key: PublicKey,
    #[n(3)]
    pub seq: u64,
    #[n(4)]
    pub created: u64,
    /// Mailbox nodes that hold mail for this station.
    #[n(5)]
    pub homes: Option<Vec<Callsign>>,
    /// Other stations vouching for this binding.
    #[n(6)]
    pub attestations: Option<Vec<Attestation>>,
}

/// Station `by` (with key `by_key`) vouches that `key` belongs to `callsign`.
///
/// CBOR `array(3) [by, by_key, sig]`; `sig` = Ed25519 over
/// `"hm/attest/v0" || callsign (6 bytes) || key (32 bytes)`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(array)]
pub struct Attestation {
    #[n(0)]
    pub by: Callsign,
    #[n(1)]
    pub by_key: PublicKey,
    #[n(2)]
    #[cbor(with = "minicbor::bytes")]
    pub sig: [u8; 64],
}

fn attest_statement(callsign: Callsign, key: &PublicKey) -> Vec<u8> {
    let mut m = Vec::with_capacity(ATTEST_PREFIX.len() + 38);
    m.extend_from_slice(ATTEST_PREFIX);
    m.extend_from_slice(&callsign.to_bytes());
    m.extend_from_slice(&key.0);
    m
}

impl Attestation {
    /// `attester` (station `by`) vouches for `callsign` ↔ `key`.
    pub fn make(by: Callsign, attester: &Identity, callsign: Callsign, key: &PublicKey) -> Attestation {
        Attestation {
            by,
            by_key: attester.public(),
            sig: attester.sign(&attest_statement(callsign, key)),
        }
    }

    pub fn verify(&self, callsign: Callsign, key: &PublicKey) -> Result<(), IdentError> {
        self.by_key.verify(&attest_statement(callsign, key), &self.sig)
    }
}

impl BindingRecord {
    pub fn new(callsign: Callsign, key: PublicKey, seq: u64, created: u64) -> BindingRecord {
        BindingRecord {
            v: BINDING_VERSION,
            callsign,
            key,
            seq,
            created,
            homes: None,
            attestations: None,
        }
    }

    fn check(&self) -> Result<(), IdentError> {
        if self.v != BINDING_VERSION {
            return Err(IdentError::UnsupportedVersion(self.v));
        }
        if self.homes.as_ref().is_some_and(|h| h.is_empty())
            || self.attestations.as_ref().is_some_and(|a| a.is_empty())
        {
            return Err(IdentError::Invalid("empty lists must be omitted"));
        }
        Ok(())
    }

    /// Sign the record with the identity it names.
    pub fn seal(self, identity: &Identity) -> Result<SignedBinding, IdentError> {
        self.check()?;
        if identity.public() != self.key {
            return Err(IdentError::KeyMismatch);
        }
        let raw = minicbor::to_vec(&self).map_err(|_| IdentError::Invalid("encoding failed"))?;
        let envelope = Envelope::seal(&BINDING, raw, identity);
        Ok(SignedBinding {
            envelope,
            record: self,
        })
    }
}

/// A binding record together with its verified envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedBinding {
    envelope: Envelope,
    record: BindingRecord,
}

impl SignedBinding {
    /// Decode and verify a self-signed binding from its envelope bytes.
    pub fn decode(bytes: &[u8]) -> Result<SignedBinding, IdentError> {
        let envelope = Envelope::decode(bytes)?;
        let record: BindingRecord = minicbor::decode(&envelope.raw)?;
        record.check()?;
        envelope.verify(&BINDING, &record.key)?;
        Ok(SignedBinding { envelope, record })
    }

    pub fn record(&self) -> &BindingRecord {
        &self.record
    }

    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    pub fn id(&self) -> ObjectId {
        self.envelope.id(&BINDING)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.envelope.to_vec()
    }

    /// Callsigns of the attesters whose signatures verify.
    pub fn valid_attesters(&self) -> Vec<Callsign> {
        let r = &self.record;
        r.attestations
            .iter()
            .flatten()
            .filter(|a| a.verify(r.callsign, &r.key).is_ok())
            .map(|a| a.by)
            .collect()
    }

    /// True if `self` should replace `other`: same callsign and key, higher
    /// `seq`. Anyone can self-sign a binding for any callsign, so a record
    /// under another key never replaces one by sequence number alone; moving
    /// a callsign to a new key is for the trust list to decide.
    pub fn supersedes(&self, other: &SignedBinding) -> bool {
        self.record.callsign == other.record.callsign
            && self.record.key == other.record.key
            && self.record.seq > other.record.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    #[test]
    fn seal_decode_roundtrip() {
        let me = Identity::from_secret([11; 32]);
        let mut rec = BindingRecord::new(call("SA0KAM-7"), me.public(), 1, 1_790_000_000);
        rec.homes = Some(vec![call("SA0KAM-10"), call("SO5KM-10")]);
        // The record binds this one station: SSIDs are stations of their own.
        assert_eq!(rec.callsign, call("SA0KAM-7"));
        let signed = rec.clone().seal(&me).unwrap();
        let back = SignedBinding::decode(&signed.to_vec()).unwrap();
        assert_eq!(back.record(), &rec);
        assert_eq!(back.id(), signed.id());
    }

    #[test]
    fn wrong_key_is_refused() {
        let me = Identity::from_secret([11; 32]);
        let other = Identity::from_secret([12; 32]);
        let rec = BindingRecord::new(call("SA0KAM"), me.public(), 1, 0);
        assert_eq!(rec.seal(&other).unwrap_err(), IdentError::KeyMismatch);
    }

    #[test]
    fn tampered_record_fails() {
        let me = Identity::from_secret([11; 32]);
        let signed = BindingRecord::new(call("SA0KAM"), me.public(), 1, 0)
            .seal(&me)
            .unwrap();
        let mut env = signed.envelope().clone();
        let mut rec: BindingRecord = minicbor::decode(&env.raw).unwrap();
        rec.seq = 99;
        env.raw = minicbor::to_vec(&rec).unwrap();
        assert_eq!(
            SignedBinding::decode(&env.to_vec()).unwrap_err(),
            IdentError::BadSignature
        );
    }

    #[test]
    fn attestations_verify_individually() {
        let me = Identity::from_secret([11; 32]);
        let club = Identity::from_secret([21; 32]);
        let liar = Identity::from_secret([22; 32]);
        let good = Attestation::make(call("Q0CLUB"), &club, call("SA0KAM"), &me.public());
        let mut bad = Attestation::make(call("Q0LIAR"), &liar, call("SA0KAM"), &me.public());
        bad.sig[0] ^= 1;
        let mut rec = BindingRecord::new(call("SA0KAM"), me.public(), 2, 0);
        rec.attestations = Some(vec![good, bad]);
        let signed = rec.seal(&me).unwrap();
        let back = SignedBinding::decode(&signed.to_vec()).unwrap();
        assert_eq!(back.valid_attesters(), vec![call("Q0CLUB")]);
    }

    #[test]
    fn higher_seq_supersedes() {
        let me = Identity::from_secret([11; 32]);
        let a = BindingRecord::new(call("SA0KAM"), me.public(), 1, 0)
            .seal(&me)
            .unwrap();
        let b = BindingRecord::new(call("SA0KAM"), me.public(), 2, 0)
            .seal(&me)
            .unwrap();
        assert!(b.supersedes(&a));
        assert!(!a.supersedes(&b));
        let other = Identity::from_secret([12; 32]);
        let c = BindingRecord::new(call("SA0KAM"), other.public(), 3, 0)
            .seal(&other)
            .unwrap();
        assert!(!c.supersedes(&b), "another key, whatever its sequence");
    }

    #[test]
    fn empty_lists_are_rejected() {
        let me = Identity::from_secret([11; 32]);
        let mut rec = BindingRecord::new(call("SA0KAM"), me.public(), 1, 0);
        rec.homes = Some(vec![]);
        assert!(rec.seal(&me).is_err());
        let mut rec = BindingRecord::new(call("SA0KAM"), me.public(), 1, 0);
        rec.attestations = Some(vec![]);
        assert!(rec.seal(&me).is_err());
    }
}
