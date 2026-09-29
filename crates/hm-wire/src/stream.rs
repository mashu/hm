//! Framing on reliable byte streams: an internet link's QUIC streams and an
//! ARQ modem's connection (SPEC section 10).
//!
//! ```text
//! object    "HMD0" | length u32 BE | object bytes
//! control   "HMC0" | length u32 BE | control bytes
//! stored    0x00 | receipt (64 B)
//! rejected  0x01 | reason length u16 BE | reason (UTF-8)
//! busy      0x02 | retry after u16 BE (seconds) | reason length u16 BE | reason
//! ```
//!
//! A control message is answered on its own QUIC stream with 0x00 alone
//! ([`CONTROL_TAKEN`]) or a rejection; ARQ connections carry no control messages.
//!
//! [`next_message`] looks at what has arrived so far and says whether it
//! starts with a whole message, needs more bytes, or can never be a message.
//! It borrows from its input and never allocates, so a peer that announces a
//! large object makes no reader set memory aside for it.

use alloc::vec::Vec;

pub const OBJECT_MAGIC: &[u8; 4] = b"HMD0";
pub const CONTROL_MAGIC: &[u8; 4] = b"HMC0";
/// Longest reason an answer is sent with (longer ones are cut).
pub const MAX_REASON: usize = 512;
/// The whole answer to a control message that was taken.
pub const CONTROL_TAKEN: [u8; 1] = [0x00];
const STORED: u8 = 0x00;
const REJECTED: u8 = 0x01;
const BUSY: u8 = 0x02;

/// One message on a stream, borrowed from the bytes it was read from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StreamMessage<'a> {
    /// A bundle (or other object) for the receiver to take custody of.
    Object(&'a [u8]),
    /// Authenticated pairwise control data (SYNC).
    Control(&'a [u8]),
    /// The receiver holds the object: its signed receipt.
    Stored([u8; 64]),
    /// Not taken, for this reason; do not send it again as it is.
    Rejected(&'a [u8]),
    /// Not taken now; try again after `retry_after` seconds.
    Busy { retry_after: u16, reason: &'a [u8] },
}

/// Why bytes can never be read as a message.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// Not the start of any message.
    UnknownType,
    /// An object or control message longer than the reader accepts.
    TooLarge { len: usize, limit: usize },
}

impl core::fmt::Display for StreamError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StreamError::UnknownType => f.write_str("unknown message type"),
            StreamError::TooLarge { len, limit } => write!(f, "message of {len} bytes exceeds {limit}"),
        }
    }
}

/// The longest object and control message a reader takes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StreamLimits {
    pub max_object: usize,
    pub max_control: usize,
}

/// The kind and length an object or control header announces, checked
/// against `limits`: what a reader needs to know before taking the body.
/// `Ok(None)` while fewer than 8 bytes have arrived but they could still start one.
pub fn request_header(buf: &[u8], limits: StreamLimits) -> Result<Option<(bool, usize)>, StreamError> {
    let magic_len = buf.len().min(4);
    let control = if buf[..magic_len] == OBJECT_MAGIC[..magic_len] {
        false
    } else if buf[..magic_len] == CONTROL_MAGIC[..magic_len] {
        true
    } else {
        return Err(StreamError::UnknownType);
    };
    if buf.len() < 8 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let limit = if control {
        limits.max_control
    } else {
        limits.max_object
    };
    if len > limit {
        return Err(StreamError::TooLarge { len, limit });
    }
    Ok(Some((control, len)))
}

/// The message `buf` starts with and the bytes it takes, `Ok(None)` if
/// more bytes are needed to tell, or why `buf` can never start a message.
pub fn next_message(
    buf: &[u8],
    limits: StreamLimits,
) -> Result<Option<(StreamMessage<'_>, usize)>, StreamError> {
    let Some(&first) = buf.first() else {
        return Ok(None);
    };
    match first {
        STORED => {
            if buf.len() < 65 {
                return Ok(None);
            }
            let receipt: [u8; 64] = buf[1..65].try_into().expect("64 bytes");
            Ok(Some((StreamMessage::Stored(receipt), 65)))
        }
        REJECTED => {
            if buf.len() < 3 {
                return Ok(None);
            }
            let n = usize::from(u16::from_be_bytes([buf[1], buf[2]]));
            if buf.len() < 3 + n {
                return Ok(None);
            }
            Ok(Some((StreamMessage::Rejected(&buf[3..3 + n]), 3 + n)))
        }
        BUSY => {
            if buf.len() < 5 {
                return Ok(None);
            }
            let retry_after = u16::from_be_bytes([buf[1], buf[2]]);
            let n = usize::from(u16::from_be_bytes([buf[3], buf[4]]));
            if buf.len() < 5 + n {
                return Ok(None);
            }
            Ok(Some((
                StreamMessage::Busy {
                    retry_after,
                    reason: &buf[5..5 + n],
                },
                5 + n,
            )))
        }
        _ => {
            let Some((control, len)) = request_header(buf, limits)? else {
                return Ok(None);
            };
            if buf.len() < 8 + len {
                return Ok(None);
            }
            let body = &buf[8..8 + len];
            let message = if control {
                StreamMessage::Control(body)
            } else {
                StreamMessage::Object(body)
            };
            Ok(Some((message, 8 + len)))
        }
    }
}

impl StreamMessage<'_> {
    /// The message's bytes on the stream. Reasons longer than [`MAX_REASON`]
    /// are cut; objects and control messages must fit a u32 length.
    pub fn encode(&self) -> Vec<u8> {
        let request = |magic: &[u8; 4], body: &[u8]| {
            let mut out = Vec::with_capacity(8 + body.len());
            out.extend_from_slice(magic);
            out.extend_from_slice(&u32::try_from(body.len()).expect("fits u32").to_be_bytes());
            out.extend_from_slice(body);
            out
        };
        let reason_bytes = |reason: &[u8]| {
            let r = &reason[..reason.len().min(MAX_REASON)];
            let mut out = Vec::with_capacity(2 + r.len());
            out.extend_from_slice(&(r.len() as u16).to_be_bytes());
            out.extend_from_slice(r);
            out
        };
        match *self {
            StreamMessage::Object(body) => request(OBJECT_MAGIC, body),
            StreamMessage::Control(body) => request(CONTROL_MAGIC, body),
            StreamMessage::Stored(receipt) => {
                let mut out = Vec::with_capacity(65);
                out.push(STORED);
                out.extend_from_slice(&receipt);
                out
            }
            StreamMessage::Rejected(reason) => {
                let mut out = alloc::vec![REJECTED];
                out.extend(reason_bytes(reason));
                out
            }
            StreamMessage::Busy { retry_after, reason } => {
                let mut out = alloc::vec![BUSY];
                out.extend_from_slice(&retry_after.to_be_bytes());
                out.extend(reason_bytes(reason));
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: StreamLimits = StreamLimits {
        max_object: 1_000,
        max_control: 100,
    };

    fn every_message() -> Vec<StreamMessage<'static>> {
        vec![
            StreamMessage::Object(b"a bundle"),
            StreamMessage::Control(b"sync"),
            StreamMessage::Stored([7; 64]),
            StreamMessage::Rejected(b"no"),
            StreamMessage::Busy {
                retry_after: 90,
                reason: b"full",
            },
            StreamMessage::Object(b""),
        ]
    }

    #[test]
    fn every_message_reads_back_and_no_prefix_reads_as_one() {
        for message in every_message() {
            let bytes = message.encode();
            assert_eq!(next_message(&bytes, LIMITS), Ok(Some((message, bytes.len()))));
            for cut in 0..bytes.len() {
                assert_eq!(
                    next_message(&bytes[..cut], LIMITS),
                    Ok(None),
                    "{message:?} cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn messages_follow_each_other_on_one_stream() {
        let mut stream = Vec::new();
        for message in every_message() {
            stream.extend(message.encode());
        }
        let mut rest = &stream[..];
        let mut read = Vec::new();
        while let Some((message, used)) = next_message(rest, LIMITS).unwrap() {
            read.push(message);
            rest = &rest[used..];
        }
        assert_eq!(read, every_message());
        assert!(rest.is_empty());
    }

    #[test]
    fn what_can_never_be_a_message_is_refused_at_once() {
        assert_eq!(next_message(b"X", LIMITS), Err(StreamError::UnknownType));
        assert_eq!(next_message(b"HMX", LIMITS), Err(StreamError::UnknownType));
        assert_eq!(next_message(b"HM", LIMITS), Ok(None));
        let mut big = b"HMD0".to_vec();
        big.extend_from_slice(&1_001u32.to_be_bytes());
        assert_eq!(
            next_message(&big, LIMITS),
            Err(StreamError::TooLarge {
                len: 1_001,
                limit: 1_000
            })
        );
        let mut control = b"HMC0".to_vec();
        control.extend_from_slice(&101u32.to_be_bytes());
        assert!(matches!(
            next_message(&control, LIMITS),
            Err(StreamError::TooLarge { .. })
        ));
    }

    #[test]
    fn long_reasons_are_cut() {
        let reason = [b'x'; 600];
        let bytes = StreamMessage::Rejected(&reason).encode();
        assert_eq!(bytes.len(), 3 + MAX_REASON);
    }
}
