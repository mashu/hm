use alloc::vec;
use alloc::vec::Vec;

use crate::{Callsign, ObjectId, WireError};

pub const SYNC_CONTACT: u8 = 0x01;
pub const SYNC_FILTER: u8 = 0x02;
pub const SYNC_WANT: u8 = 0x03;
pub const SYNC_OFFER: u8 = 0x04;
pub const CONTACT_LEN: usize = 101;
pub const CONTACT_SIG_PREFIX: &[u8] = b"hm/contact/v0";
pub const MAX_FILTER_BYTES: usize = 256;
pub const MAX_WANT: usize = 16;
pub const MAX_OFFER: usize = 16;

const CONTACT_UNSIGNED_END: usize = 37;
const FILTER_HEADER_LEN: usize = 8;
const WANT_HEADER_LEN: usize = 2;
const OFFER_HEADER_LEN: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ContactBearer {
    Radio = 0,
    Internet = 1,
}

impl TryFrom<u8> for ContactBearer {
    type Error = WireError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Radio),
            1 => Ok(Self::Internet),
            _ => Err(WireError::OutOfRange),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContactAdvert {
    pub origin: Callsign,
    pub sequence: u32,
    pub start: u32,
    pub end: u32,
    pub peer: Callsign,
    pub bearer: ContactBearer,
    pub success_permyriad: u16,
    pub rate_bps: u32,
    pub capacity_bytes: u32,
    pub flags: u8,
    pub signature: [u8; 64],
}

impl ContactAdvert {
    pub fn encode(&self) -> Result<[u8; CONTACT_LEN], WireError> {
        validate_contact(self)?;
        let mut out = [0_u8; CONTACT_LEN];
        out[0] = SYNC_CONTACT;
        out[1..7].copy_from_slice(&self.origin.to_bytes());
        out[7..11].copy_from_slice(&self.sequence.to_be_bytes());
        out[11..15].copy_from_slice(&self.start.to_be_bytes());
        out[15..19].copy_from_slice(&self.end.to_be_bytes());
        out[19..25].copy_from_slice(&self.peer.to_bytes());
        out[25] = self.bearer as u8;
        out[26..28].copy_from_slice(&self.success_permyriad.to_be_bytes());
        out[28..32].copy_from_slice(&self.rate_bps.to_be_bytes());
        out[32..36].copy_from_slice(&self.capacity_bytes.to_be_bytes());
        out[36] = self.flags;
        out[37..].copy_from_slice(&self.signature);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() != CONTACT_LEN || bytes[0] != SYNC_CONTACT {
            return Err(WireError::OutOfRange);
        }
        let advert = Self {
            origin: Callsign::from_bytes(copy_array(&bytes[1..7])?)?,
            sequence: u32::from_be_bytes(copy_array(&bytes[7..11])?),
            start: u32::from_be_bytes(copy_array(&bytes[11..15])?),
            end: u32::from_be_bytes(copy_array(&bytes[15..19])?),
            peer: Callsign::from_bytes(copy_array(&bytes[19..25])?)?,
            bearer: ContactBearer::try_from(bytes[25])?,
            success_permyriad: u16::from_be_bytes(copy_array(&bytes[26..28])?),
            rate_bps: u32::from_be_bytes(copy_array(&bytes[28..32])?),
            capacity_bytes: u32::from_be_bytes(copy_array(&bytes[32..36])?),
            flags: bytes[36],
            signature: copy_array(&bytes[37..101])?,
        };
        validate_contact(&advert)?;
        Ok(advert)
    }

    pub fn signing_statement(&self) -> Result<Vec<u8>, WireError> {
        let encoded = self.encode()?;
        let mut statement = Vec::with_capacity(CONTACT_SIG_PREFIX.len() + CONTACT_UNSIGNED_END - 1);
        statement.extend_from_slice(CONTACT_SIG_PREFIX);
        statement.extend_from_slice(&encoded[1..CONTACT_UNSIGNED_END]);
        Ok(statement)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncFilter {
    pub scope: u8,
    pub hashes: u8,
    pub salt: u32,
    pub bits: Vec<u8>,
}

impl SyncFilter {
    pub fn new(scope: u8, hashes: u8, salt: u32, bytes: usize) -> Result<Self, WireError> {
        if scope > 1 || hashes == 0 || hashes > 16 || bytes == 0 || bytes > MAX_FILTER_BYTES {
            return Err(WireError::OutOfRange);
        }
        Ok(Self {
            scope,
            hashes,
            salt,
            bits: vec![0; bytes],
        })
    }

    pub fn insert(&mut self, id: &ObjectId) -> Result<(), WireError> {
        for position in self.positions(id)? {
            self.bits[position / 8] |= 1 << (position % 8);
        }
        Ok(())
    }

    pub fn contains(&self, id: &ObjectId) -> Result<bool, WireError> {
        Ok(self
            .positions(id)?
            .into_iter()
            .all(|position| self.bits[position / 8] & (1 << (position % 8)) != 0))
    }

    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        validate_filter(self)?;
        let mut out = Vec::with_capacity(FILTER_HEADER_LEN + self.bits.len());
        out.push(SYNC_FILTER);
        out.push(self.scope);
        out.push(self.hashes);
        out.push(0);
        out.extend_from_slice(&self.salt.to_be_bytes());
        out.extend_from_slice(&self.bits);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < FILTER_HEADER_LEN + 1 {
            return Err(WireError::TooShort);
        }
        if bytes[0] != SYNC_FILTER || bytes[3] != 0 {
            return Err(WireError::OutOfRange);
        }
        let filter = Self {
            scope: bytes[1],
            hashes: bytes[2],
            salt: u32::from_be_bytes(copy_array(&bytes[4..8])?),
            bits: bytes[FILTER_HEADER_LEN..].to_vec(),
        };
        validate_filter(&filter)?;
        Ok(filter)
    }

    fn positions(&self, id: &ObjectId) -> Result<Vec<usize>, WireError> {
        validate_filter(self)?;
        let bit_len = self.bits.len() * 8;
        let mut positions = Vec::with_capacity(usize::from(self.hashes));
        for index in 0..self.hashes {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"HMNET-SYNC-FILTER-V1\0");
            hasher.update(&self.salt.to_be_bytes());
            hasher.update(&id.0);
            hasher.update(&[index]);
            let digest = hasher.finalize();
            let word = u32::from_be_bytes(copy_array(&digest.as_bytes()[..4])?);
            positions.push((word as usize) % bit_len);
        }
        Ok(positions)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncWant {
    pub prefixes: Vec<[u8; 8]>,
}

/// A bounded page of holdings the sender can transfer. The receiver answers
/// with [`SyncWant`] for the prefixes it does not already hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOffer {
    pub scope: u8,
    pub prefixes: Vec<[u8; 8]>,
}

impl SyncOffer {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        if self.scope > 1 || self.prefixes.is_empty() || self.prefixes.len() > MAX_OFFER {
            return Err(WireError::OutOfRange);
        }
        let count = u8::try_from(self.prefixes.len()).map_err(|_| WireError::OutOfRange)?;
        let mut out = Vec::with_capacity(OFFER_HEADER_LEN + self.prefixes.len() * 8);
        out.extend_from_slice(&[SYNC_OFFER, self.scope, count, 0]);
        for prefix in &self.prefixes {
            out.extend_from_slice(prefix);
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < OFFER_HEADER_LEN || bytes[0] != SYNC_OFFER {
            return Err(WireError::TooShort);
        }
        let count = usize::from(bytes[2]);
        if bytes[1] > 1
            || count == 0
            || count > MAX_OFFER
            || bytes[3] != 0
            || bytes.len() != OFFER_HEADER_LEN + count * 8
        {
            return Err(WireError::OutOfRange);
        }
        Ok(Self {
            scope: bytes[1],
            prefixes: bytes[OFFER_HEADER_LEN..].as_chunks::<8>().0.to_vec(),
        })
    }
}

impl SyncWant {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        if self.prefixes.is_empty() || self.prefixes.len() > MAX_WANT {
            return Err(WireError::OutOfRange);
        }
        let count = u8::try_from(self.prefixes.len()).map_err(|_| WireError::OutOfRange)?;
        let mut out = Vec::with_capacity(WANT_HEADER_LEN + self.prefixes.len() * 8);
        out.push(SYNC_WANT);
        out.push(count);
        for prefix in &self.prefixes {
            out.extend_from_slice(prefix);
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < WANT_HEADER_LEN || bytes[0] != SYNC_WANT {
            return Err(WireError::TooShort);
        }
        let count = usize::from(bytes[1]);
        if count == 0 || count > MAX_WANT || bytes.len() != WANT_HEADER_LEN + count * 8 {
            return Err(WireError::OutOfRange);
        }
        let prefixes = bytes[WANT_HEADER_LEN..].as_chunks::<8>().0.to_vec();
        Ok(Self { prefixes })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncMessage {
    Contact(ContactAdvert),
    Filter(SyncFilter),
    Want(SyncWant),
    Offer(SyncOffer),
}

impl SyncMessage {
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        match bytes.first() {
            Some(&SYNC_CONTACT) => ContactAdvert::decode(bytes).map(Self::Contact),
            Some(&SYNC_FILTER) => SyncFilter::decode(bytes).map(Self::Filter),
            Some(&SYNC_WANT) => SyncWant::decode(bytes).map(Self::Want),
            Some(&SYNC_OFFER) => SyncOffer::decode(bytes).map(Self::Offer),
            _ => Err(WireError::OutOfRange),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        match self {
            Self::Contact(contact) => contact.encode().map(|bytes| bytes.to_vec()),
            Self::Filter(filter) => filter.encode(),
            Self::Want(want) => want.encode(),
            Self::Offer(offer) => offer.encode(),
        }
    }
}

fn validate_contact(contact: &ContactAdvert) -> Result<(), WireError> {
    if contact.end <= contact.start || contact.success_permyriad > 10_000 || contact.rate_bps == 0 {
        return Err(WireError::OutOfRange);
    }
    Ok(())
}

fn validate_filter(filter: &SyncFilter) -> Result<(), WireError> {
    if filter.hashes == 0
        || filter.scope > 1
        || filter.hashes > 16
        || filter.bits.is_empty()
        || filter.bits.len() > MAX_FILTER_BYTES
    {
        return Err(WireError::OutOfRange);
    }
    Ok(())
}

fn copy_array<const N: usize>(bytes: &[u8]) -> Result<[u8; N], WireError> {
    bytes.try_into().map_err(|_| WireError::TooShort)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callsign(value: &str) -> Callsign {
        value.parse().unwrap()
    }

    #[test]
    fn contact_roundtrip_and_statement_excludes_signature() {
        let contact = ContactAdvert {
            origin: callsign("M0ABC"),
            sequence: 7,
            start: 1_700_000_000,
            end: 1_700_003_600,
            peer: callsign("M0XYZ"),
            bearer: ContactBearer::Radio,
            success_permyriad: 8_750,
            rate_bps: 9_600,
            capacity_bytes: 32_768,
            flags: 0x03,
            signature: [0xA5; 64],
        };
        let encoded = contact.encode().unwrap();
        assert_eq!(ContactAdvert::decode(&encoded).unwrap(), contact);
        assert_eq!(encoded.len(), CONTACT_LEN);
        let statement = contact.signing_statement().unwrap();
        assert_eq!(&statement[..CONTACT_SIG_PREFIX.len()], CONTACT_SIG_PREFIX);
        assert_eq!(&statement[CONTACT_SIG_PREFIX.len()..], &encoded[1..37]);
        assert!(!statement.ends_with(&contact.signature));
    }

    #[test]
    fn filter_roundtrip_and_membership() {
        let present = ObjectId(*blake3::hash(b"present").as_bytes());
        let absent = ObjectId(*blake3::hash(b"absent").as_bytes());
        let mut filter = SyncFilter::new(1, 5, 0x1234_5678, 64).unwrap();
        filter.insert(&present).unwrap();
        assert!(filter.contains(&present).unwrap());
        assert!(!filter.contains(&absent).unwrap());
        let encoded = filter.encode().unwrap();
        assert_eq!(SyncFilter::decode(&encoded).unwrap(), filter);
    }

    #[test]
    fn want_roundtrip() {
        let want = SyncWant {
            prefixes: vec![[1; 8], [2; 8]],
        };
        let encoded = want.encode().unwrap();
        assert_eq!(SyncWant::decode(&encoded).unwrap(), want);
        assert_eq!(SyncMessage::decode(&encoded).unwrap(), SyncMessage::Want(want));
    }

    #[test]
    fn offer_roundtrip() {
        let offer = SyncOffer {
            scope: 0,
            prefixes: vec![[1; 8], [2; 8]],
        };
        let encoded = offer.encode().unwrap();
        assert_eq!(SyncOffer::decode(&encoded).unwrap(), offer);
        assert_eq!(SyncMessage::decode(&encoded).unwrap(), SyncMessage::Offer(offer));
    }

    #[test]
    fn rejects_nonzero_reserved_filter_byte() {
        let bytes = [SYNC_FILTER, 0, 3, 1, 0, 0, 0, 2, 1];
        assert_eq!(SyncFilter::decode(&bytes), Err(WireError::OutOfRange));
    }
}
