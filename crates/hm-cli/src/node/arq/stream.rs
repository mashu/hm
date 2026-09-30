//! Bundles on a modem connection's data stream, as on the internet:
//! `"HMD0"`, the length, the bundle; then the receipt or a reason.

use super::*;

/// One side of a connection's byte stream, parsed into messages.
#[derive(Default)]
pub(super) struct Inbox {
    pub(super) buf: Vec<u8>,
}

pub(super) enum Message {
    Bundle(Vec<u8>),
    Receipt([u8; 64]),
    Rejected(String),
    Busy { retry_after: u64, reason: String },
    Bad(String),
}

impl Inbox {
    /// The next complete message, if one has arrived. Bundles start with
    /// `"HMD0"` and answers with `0x00` or `0x01`, so both can share one
    /// connection (both stations may send at once).
    pub(super) fn next(&mut self) -> Option<Message> {
        let (msg, used) = match next_message(&self.buf, LIMITS) {
            Ok(None) => return None,
            Ok(Some((message, used))) => {
                let msg = match message {
                    StreamMessage::Object(object) => Message::Bundle(object.to_vec()),
                    StreamMessage::Stored(receipt) => Message::Receipt(receipt),
                    StreamMessage::Rejected(reason) => refusal(String::from_utf8_lossy(reason).into_owned()),
                    StreamMessage::Busy { retry_after, reason } => Message::Busy {
                        retry_after: u64::from(retry_after),
                        reason: String::from_utf8_lossy(reason).into_owned(),
                    },
                    StreamMessage::Control(_) => {
                        Message::Bad("no control messages on a modem connection".into())
                    }
                };
                (msg, used)
            }
            // Nothing after it can be told apart from it: drop what is buffered.
            Err(error) => (Message::Bad(error.to_string()), self.buf.len()),
        };
        self.buf.drain(..used);
        Some(msg)
    }
}

/// Most data kept while waiting for the CONNECTED line it belongs to: one
/// whole bundle message.
pub(super) const EARLY_LIMIT: usize = 8 + MAX_OBJECT;

/// Whether `bytes` can be the start of what a peer sends on a new connection.
/// Each side's first message is a bundle (answers only follow bundles), so
/// anything else is left over from an earlier connection.
pub(super) fn opens_connection(bytes: &[u8]) -> bool {
    let n = bytes.len().min(OBJECT_MAGIC.len());
    n > 0 && bytes[..n] == OBJECT_MAGIC[..n]
}

pub(super) fn bundle_message(object: &[u8]) -> Vec<u8> {
    StreamMessage::Object(object).encode()
}

/// A refusal. Busy is sent as a refusal saying so: modem peers of older
/// versions know no busy answer.
pub(super) fn rejection(reason: &str) -> Vec<u8> {
    StreamMessage::Rejected(reason.as_bytes()).encode()
}

/// A refusal received: busy, when it says so the way [`rejection`] does.
pub(super) fn refusal(reason: String) -> Message {
    let busy = reason.strip_prefix("busy for ").and_then(|rest| {
        let (secs, rest) = rest.split_once(" s")?;
        let retry_after = secs.parse().ok()?;
        Some((retry_after, rest.trim_start_matches(':').trim_start().to_string()))
    });
    match busy {
        Some((retry_after, reason)) => Message::Busy { retry_after, reason },
        None => Message::Rejected(reason),
    }
}
