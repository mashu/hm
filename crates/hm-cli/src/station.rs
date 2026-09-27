//! Build, send and receive bundles over a radio [`Link`](crate::driver::Link).
//!
//! Used by `hm send` / `hm listen` and by the node. The transfer engine is the
//! same sans-IO machine the simulator drives; this module only supplies the clock,
//! the station key and the trusted stations.

use std::borrow::Cow;
use std::io;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hm_bundle::{Address, Bundle, BundleError, Kind, Opened, Precedence, SignedBundle};
use hm_core::{DetRng, Millis};
use hm_ident::Identity;
use hm_wire::{Callsign, ObjectId};
use hm_xfer::{object_id, Command, Config, Event, Failure, Receipt, Xfer};

use crate::driver::{run, End, Flow, Link};
use crate::files::{KeyFile, Trust};

/// Chat expires after an hour; mail after a week. Relays use this; the local
/// queue retries on its own schedule.
const CHAT_TTL: u32 = 3600;
const MAIL_TTL: u32 = 7 * 86_400;

/// Link parameters the transfer engine uses to time overs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LinkTiming {
    pub bitrate_bps: u32,
    pub txdelay_ms: u64,
    pub guard_ms: u64,
    pub max_rounds: u8,
}

/// One station's key, trust and link timing, borrowed for a send or a listen.
pub struct Station<'a> {
    pub key: &'a KeyFile,
    pub trust: &'a Trust,
    pub me: Callsign,
    pub timing: LinkTiming,
}

/// Result of [`Station::send_object`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Delivered {
        rounds: u8,
        after: Millis,
        receipt: Receipt,
    },
    Failed(Failure),
    TimedOut,
}

/// What the trusted keys could say about a received bundle's signature.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Verification {
    /// Signed by the key listed for the sender.
    Verified,
    /// No key for the sender, so the signature was not checked.
    Unverified,
    /// A key is listed and the signature does not match it.
    BadSignature,
}

/// One object pulled off the air, decoded as far as it will go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: ObjectId,
    pub via: Callsign,
    pub verification: Verification,
    pub bundle: Option<Bundle>,
    pub error: Option<String>,
}

impl Message {
    /// UTF-8 body, when the bundle has one this build understands.
    pub fn text(&self) -> Option<Cow<'_, str>> {
        self.bundle.as_ref()?.body.as_ref()?.as_text().ok()
    }
}

impl Station<'_> {
    /// Transfer engine for this station, trusting every trusted key.
    pub fn engine(&self) -> io::Result<Xfer> {
        let mut cfg = Config::vhf_1200(self.me);
        cfg.bitrate_bps = self.timing.bitrate_bps;
        cfg.txdelay = Millis(self.timing.txdelay_ms);
        cfg.ack_guard = Millis(self.timing.guard_ms);
        cfg.max_rounds = self.timing.max_rounds;
        let mut xfer = Xfer::new(cfg, Identity::from_secret(self.key.identity.secret()), rng())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        for (call, key) in self.trust.iter() {
            xfer.trust(call, key);
        }
        Ok(xfer)
    }

    /// Send `object` to `to` and wait until it is confirmed, fails, or `timeout` passes.
    pub fn send_object(
        &self,
        link: &mut impl Link,
        to: Callsign,
        object: Vec<u8>,
        precedence: u8,
        timeout: Duration,
    ) -> io::Result<SendOutcome> {
        let mut xfer = self.engine()?;
        let port = xfer.config().port;
        let mut outcome = None;
        let end = run(
            &mut xfer,
            link,
            port,
            vec![Command::Send {
                to,
                object,
                precedence,
            }],
            Some(timeout),
            None,
            |now, event| match event {
                Event::Delivered { rounds, receipt, .. } => {
                    outcome = Some(SendOutcome::Delivered {
                        rounds,
                        after: now,
                        receipt,
                    });
                    Flow::Stop
                }
                Event::Failed { reason, .. } => {
                    outcome = Some(SendOutcome::Failed(reason));
                    Flow::Stop
                }
                Event::Received { .. } => Flow::Continue,
            },
        )?;
        Ok(match end {
            End::Stopped => outcome.unwrap_or(SendOutcome::TimedOut),
            End::TimedOut | End::Interrupted => SendOutcome::TimedOut,
        })
    }

    /// Receive until `timeout`, `stop` is set, or the callback returns [`Flow::Stop`].
    pub fn listen(
        &self,
        link: &mut impl Link,
        timeout: Option<Duration>,
        stop: Option<&AtomicBool>,
        mut on_message: impl FnMut(Message) -> Flow,
    ) -> io::Result<()> {
        let mut xfer = self.engine()?;
        let port = xfer.config().port;
        let trust = self.trust;
        run(
            &mut xfer,
            link,
            port,
            Vec::new(),
            timeout,
            stop,
            |_now, event| match event {
                Event::Received { from, object, .. } => on_message(open_message(from, &object, trust)),
                Event::Delivered { .. } | Event::Failed { .. } => Flow::Continue,
            },
        )?;
        Ok(())
    }
}

/// Seal a text message from `me` to one station. A subject makes it mail; otherwise chat.
pub fn build_bundle(
    key: &KeyFile,
    me: Callsign,
    to: Callsign,
    text: &str,
    subject: Option<&str>,
    prec: Precedence,
) -> Result<SignedBundle, BundleError> {
    let (kind, ttl) = match subject {
        Some(_) => (Kind::Mail, MAIL_TTL),
        None => (Kind::Chat, CHAT_TTL),
    };
    let mut bundle = Bundle::new(me, kind, unix_now(), ttl)
        .to(Address::Station(to))
        .with_text(text)
        .with_precedence(prec);
    if let Some(subject) = subject {
        bundle = bundle.with_subject(subject);
    }
    bundle.seal(&key.identity)
}

/// Decode `object` and check its signature against `trust`. `via` is who sent the frame.
pub fn open_message(via: Callsign, object: &[u8], trust: &Trust) -> Message {
    let opened = match Opened::decode(object) {
        Ok(opened) => opened,
        Err(e) => {
            return Message {
                id: object_id(object),
                via,
                verification: Verification::Unverified,
                bundle: None,
                error: Some(e.to_string()),
            };
        }
    };
    let verification = match trust.key_for(opened.bundle.from) {
        None => Verification::Unverified,
        Some(key) => match opened.clone().verify(&key) {
            Ok(_) => Verification::Verified,
            Err(_) => Verification::BadSignature,
        },
    };
    Message {
        id: opened.id,
        via,
        verification,
        bundle: Some(opened.bundle),
        error: None,
    }
}

/// Unix seconds, or 0 if the clock is before the epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SSZ` for a Unix timestamp.
pub fn utc_clock(unix: u64) -> String {
    let secs = unix % 86_400;
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
}

/// Days since 1970-01-01 to a civil date. Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

fn rng() -> DetRng {
    let mut bytes = [0u8; 8];
    let seed = if getrandom::fill(&mut bytes).is_ok() {
        u64::from_le_bytes(bytes)
    } else {
        unix_now()
    };
    DetRng::from_seed(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_bundle::Precedence;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    #[test]
    fn utc_clock_formats_the_epoch_and_a_known_instant() {
        assert_eq!(utc_clock(0), "1970-01-01 00:00:00Z");
        assert_eq!(utc_clock(1_790_000_000), "2026-09-21 14:13:20Z");
    }

    #[test]
    fn subject_selects_mail_and_signature_follows_the_trust_file() {
        let alice = KeyFile::generate(call("SA0KAM")).unwrap();
        let bob = KeyFile::generate(call("SO5KM")).unwrap();
        let chat = build_bundle(
            &alice,
            call("SA0KAM"),
            call("SO5KM-1"),
            "73",
            None,
            Precedence::Routine,
        )
        .unwrap();
        assert_eq!(chat.bundle().kind, Kind::Chat);
        let mail = build_bundle(
            &alice,
            call("SA0KAM"),
            call("SO5KM-1"),
            "sked",
            Some("40m"),
            Precedence::Priority,
        )
        .unwrap();
        assert_eq!(mail.bundle().kind, Kind::Mail);

        let wire = chat.to_vec();
        let unknown = open_message(call("SA0KAM"), &wire, &Trust::default());
        assert_eq!(unknown.verification, Verification::Unverified);
        assert_eq!(unknown.text().as_deref(), Some("73"));

        let mut trust = Trust::default();
        trust.insert(alice.call, alice.identity.public());
        let good = open_message(call("SA0KAM"), &wire, &trust);
        assert_eq!(good.verification, Verification::Verified);
        assert_eq!(good.id, chat.id());

        trust.insert(alice.call, bob.identity.public());
        let bad = open_message(call("SA0KAM"), &wire, &trust);
        assert_eq!(bad.verification, Verification::BadSignature);

        let junk = open_message(call("SA0KAM"), b"not a bundle", &trust);
        assert!(junk.bundle.is_none());
        assert!(junk.error.is_some());
    }
}
