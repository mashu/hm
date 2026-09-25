use core::fmt;

use minicbor::decode::{self, Decoder};
use minicbor::encode::{self, Encoder, Write};
use minicbor::{Decode, Encode};

/// A 32-byte content hash naming a bundle, record or attachment.
///
/// How the hash is computed depends on the object's domain; see `hm-ident`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    /// The first 8 bytes, used where airtime matters more than collision
    /// resistance (ACK completion lists, reconciliation hints).
    pub fn prefix8(&self) -> [u8; 8] {
        let mut p = [0u8; 8];
        p.copy_from_slice(&self.0[..8]);
        p
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId(")?;
        for b in &self.0[..8] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

impl<C> Encode<C> for ObjectId {
    fn encode<W: Write>(&self, e: &mut Encoder<W>, _: &mut C) -> Result<(), encode::Error<W::Error>> {
        e.bytes(&self.0)?;
        Ok(())
    }
}

impl<'b, C> Decode<'b, C> for ObjectId {
    fn decode(d: &mut Decoder<'b>, _: &mut C) -> Result<Self, decode::Error> {
        let pos = d.position();
        let b = d.bytes()?;
        let arr: [u8; 32] = b
            .try_into()
            .map_err(|_| decode::Error::message("object id must be 32 bytes").at(pos))?;
        Ok(ObjectId(arr))
    }
}
