use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use hm_wire::{Callsign, ObjectId};
use minicbor::decode::{self, Decoder};
use minicbor::encode::{self, Encoder, Write};
use minicbor::{Decode, Encode};

use crate::{BundleError, MAX_EMAIL, MAX_NAME};

macro_rules! u8_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $var:ident = $val:expr),* $(,)? }) => {
        $(#[$m])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vm])* $var,)*
            /// A value defined by a newer version; preserved as-is.
            Other(u8),
        }

        impl $name {
            pub fn to_u8(self) -> u8 {
                match self {
                    $($name::$var => $val,)*
                    $name::Other(x) => x,
                }
            }

            pub fn from_u8(x: u8) -> $name {
                match x {
                    $($val => $name::$var,)*
                    other => $name::Other(other),
                }
            }
        }

        impl<C> Encode<C> for $name {
            fn encode<W: Write>(&self, e: &mut Encoder<W>, _: &mut C) -> Result<(), encode::Error<W::Error>> {
                e.u8(self.to_u8())?;
                Ok(())
            }
        }

        impl<'b, C> Decode<'b, C> for $name {
            fn decode(d: &mut Decoder<'b>, _: &mut C) -> Result<Self, decode::Error> {
                Ok($name::from_u8(d.u8()?))
            }
        }
    };
}

u8_enum!(
    /// What a bundle is.
    Kind {
        Mail = 0,
        Chat = 1,
        /// Structured form: schema id + fields, rendered by the client.
        Form = 2,
        Bulletin = 3,
        Position = 4,
        /// Signed delivery receipt; `reply_to` names the confirmed bundle.
        Receipt = 5,
    }
);

u8_enum!(
    /// Handling precedence, lowest first.
    Precedence {
        Routine = 0,
        Priority = 1,
        Immediate = 2,
        Flash = 3,
    }
);

u8_enum!(
    /// How the body bytes are encoded.
    Codec {
        /// UTF-8 text.
        Plain = 0,
        /// Zstandard frame using [`ZSTD_DICTIONARY`].
        Zstd = 1,
    }
);

pub const ZSTD_DICTIONARY: &[u8] = include_bytes!("hm-net-v0.dict");
pub const ZSTD_DICTIONARY_BLAKE3: &str = "4b09accfcc88e3a776ce40e97a841debebf4fd4b1d7b574089fbc18d512905de";
pub const MAX_DECOMPRESSED_BODY: usize = 1024 * 1024;

impl Precedence {
    /// Queue rank: higher goes first. Unknown values rank as routine.
    pub fn rank(self) -> u8 {
        match self {
            Precedence::Other(_) => 0,
            p => p.to_u8(),
        }
    }
}

/// Message body. CBOR `array(2) [codec, bstr data]`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(array)]
pub struct Body {
    #[n(0)]
    pub codec: Codec,
    #[n(1)]
    #[cbor(with = "minicbor::bytes")]
    pub data: Vec<u8>,
}

impl Body {
    pub fn text(s: &str) -> Body {
        #[cfg(feature = "std")]
        if let Ok(data) = compress_text(s.as_bytes()) {
            if data.len() < s.len() {
                return Body {
                    codec: Codec::Zstd,
                    data,
                };
            }
        }
        Body {
            codec: Codec::Plain,
            data: s.as_bytes().to_vec(),
        }
    }

    pub fn as_text(&self) -> Result<Cow<'_, str>, BundleError> {
        match self.codec {
            Codec::Plain => core::str::from_utf8(&self.data)
                .map(Cow::Borrowed)
                .map_err(|_| BundleError::NotText),
            Codec::Zstd => decode_zstd(&self.data),
            Codec::Other(c) => Err(BundleError::UnsupportedCodec(c)),
        }
    }
}

#[cfg(feature = "std")]
fn compress_text(plain: &[u8]) -> Result<Vec<u8>, BundleError> {
    let mut compressor = zstd::bulk::Compressor::with_dictionary(3, ZSTD_DICTIONARY)
        .map_err(|error| BundleError::Decode(error.to_string()))?;
    compressor
        .compress(plain)
        .map_err(|error| BundleError::Decode(error.to_string()))
}

#[cfg(feature = "std")]
fn decode_zstd(data: &[u8]) -> Result<Cow<'_, str>, BundleError> {
    let mut decompressor = zstd::bulk::Decompressor::with_dictionary(ZSTD_DICTIONARY)
        .map_err(|error| BundleError::Decode(error.to_string()))?;
    let plain = decompressor
        .decompress(data, MAX_DECOMPRESSED_BODY)
        .map_err(|error| BundleError::Decode(error.to_string()))?;
    String::from_utf8(plain)
        .map(Cow::Owned)
        .map_err(|_| BundleError::NotText)
}

#[cfg(not(feature = "std"))]
fn decode_zstd(_: &[u8]) -> Result<Cow<'_, str>, BundleError> {
    Err(BundleError::UnsupportedCodec(Codec::Zstd.to_u8()))
}

/// An attachment, sent as a separate content-addressed object that the
/// recipient pulls on demand. CBOR map `0 hash, 1 size, 2 mime, 3 name?, 4 thumb?`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct PartRef {
    #[n(0)]
    pub hash: ObjectId,
    #[n(1)]
    pub size: u64,
    #[n(2)]
    pub mime: String,
    #[n(3)]
    pub name: Option<String>,
    /// Small preview (for example a tiny JPEG) shown before fetching.
    #[n(4)]
    #[cbor(with = "minicbor::bytes")]
    pub thumb: Option<Vec<u8>>,
}

/// A recipient. CBOR `array(2) [tag, value]`:
/// 0 station (6-byte callsign), 1 group, 2 tactical, 3 email (text).
/// Unknown tags are kept with their raw CBOR value and re-encoded verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Address {
    Station(Callsign),
    /// A named group or chat room, e.g. `SK-EMCOMM`.
    Group(String),
    /// A role independent of the operator, e.g. `EOC-UPPSALA`.
    Tactical(String),
    /// An internet address, delivered through an email gateway.
    Email(String),
    Other {
        tag: u8,
        raw: Vec<u8>,
    },
}

impl Address {
    pub fn validate(&self) -> Result<(), BundleError> {
        match self {
            Address::Station(_) | Address::Other { .. } => Ok(()),
            Address::Group(n) | Address::Tactical(n) => {
                if n.is_empty() || n.len() > MAX_NAME {
                    Err(BundleError::Invalid("group or tactical name length out of range"))
                } else {
                    Ok(())
                }
            }
            Address::Email(e) => {
                let at = e.find('@');
                if e.len() > MAX_EMAIL || at.is_none_or(|i| i == 0 || i == e.len() - 1) {
                    Err(BundleError::Invalid("malformed email address"))
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl<C> Encode<C> for Address {
    fn encode<W: Write>(&self, e: &mut Encoder<W>, ctx: &mut C) -> Result<(), encode::Error<W::Error>> {
        e.array(2)?;
        match self {
            Address::Station(c) => {
                e.u8(0)?;
                c.encode(e, ctx)?;
            }
            Address::Group(s) => {
                e.u8(1)?.str(s)?;
            }
            Address::Tactical(s) => {
                e.u8(2)?.str(s)?;
            }
            Address::Email(s) => {
                e.u8(3)?.str(s)?;
            }
            Address::Other { tag, raw } => {
                e.u8(*tag)?;
                e.writer_mut().write_all(raw).map_err(encode::Error::write)?;
            }
        }
        Ok(())
    }
}

impl<'b, C> Decode<'b, C> for Address {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let pos = d.position();
        if d.array()? != Some(2) {
            return Err(decode::Error::message("address must be a 2-element array").at(pos));
        }
        Ok(match d.u8()? {
            0 => Address::Station(Callsign::decode(d, ctx)?),
            1 => Address::Group(String::from(d.str()?)),
            2 => Address::Tactical(String::from(d.str()?)),
            3 => Address::Email(String::from(d.str()?)),
            tag => {
                let start = d.position();
                d.skip()?;
                let raw = d.input()[start..d.position()].to_vec();
                Address::Other { tag, raw }
            }
        })
    }
}
