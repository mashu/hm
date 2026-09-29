//! Build, send and receive bundles over a radio [`Link`](crate::driver::Link).
//!
//! Used by `hm send` / `hm listen` and by the node. The transfer engine is the
//! same sans-IO machine the simulator drives; this module only supplies the clock,
//! the station key and the trusted stations.

use std::io;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hm_bundle::{Address, Bundle, BundleError, Kind, Precedence, SignedBundle};
use hm_core::{DetRng, Millis};
use hm_ident::Identity;
use hm_wire::Callsign;
use hm_xfer::{Command, Config, Event, Failure, Receipt, Xfer};

use crate::driver::{run, End, Flow, Link};
use crate::files::{KeyFile, Trust};

/// Chat expires after an hour; mail after a week. Relays use this; the local
/// queue retries on its own schedule.
const CHAT_TTL: u32 = 3600;
const MAIL_TTL: u32 = 7 * 86_400;
/// Bulletins stay useful for a day on a shared channel.
pub const BULLETIN_TTL: u32 = 86_400;
/// Local publishes allowed in a rolling hour.
pub const MAX_BULLETINS_PER_HOUR: usize = 4;

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

pub use hm_node::message::{
    open_message, Message, Verification, MAX_BULLETIN_BYTES, MAX_INBOUND_BULLETINS_PER_ORIGIN_HOUR,
};
pub use hm_node::utc_clock;

impl Station<'_> {
    /// Transfer engine for this station, trusting every trusted key.
    pub fn engine(&self) -> io::Result<Xfer> {
        // Symbols and overs sized for the link's rate: an HF link at 300 bd
        // gets small symbols and short overs, not the VHF 1200 ones.
        let mut cfg = Config::for_link(self.me, self.timing.bitrate_bps, Millis(self.timing.txdelay_ms));
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
                Event::Received { .. } | Event::Over { .. } => Flow::Continue,
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
                Event::Received { from, object, .. } => on_message(open_message(from, &object, trust, None)),
                Event::Delivered { .. } | Event::Failed { .. } | Event::Over { .. } => Flow::Continue,
            },
        )?;
        Ok(())
    }
}

/// Seal a text message from `me` to one station. A subject makes it mail; otherwise chat.
/// `seq` is the directed conversation sequence for chat (ignored for mail).
pub fn build_bundle(
    key: &KeyFile,
    me: Callsign,
    to: Callsign,
    text: &str,
    subject: Option<&str>,
    prec: Precedence,
    seq: Option<u64>,
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
    if kind == Kind::Chat {
        if let Some(seq) = seq {
            bundle = bundle.with_seq(seq);
        }
    }
    bundle.seal(&key.identity)
}

/// Seal an RF bulletin to a named group. Always routine precedence; no station
/// recipient. Callers must enforce [`MAX_BULLETIN_BYTES`] on the sealed bytes.
pub fn build_bulletin(
    key: &KeyFile,
    me: Callsign,
    group: &str,
    text: &str,
    subject: Option<&str>,
) -> Result<SignedBundle, BundleError> {
    let group = group.trim();
    let address = Address::Group(group.to_string());
    address.validate()?;
    let mut bundle = Bundle::new(me, Kind::Bulletin, unix_now(), BULLETIN_TTL)
        .to(address)
        .with_text(text)
        .with_precedence(Precedence::Routine);
    if let Some(subject) = subject.map(str::trim).filter(|s| !s.is_empty()) {
        bundle = bundle.with_subject(subject);
    }
    bundle.seal(&key.identity)
}

/// Unix seconds, or 0 if the clock is before the epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
            Some(1),
        )
        .unwrap();
        assert_eq!(chat.bundle().kind, Kind::Chat);
        assert_eq!(chat.bundle().seq, Some(1));
        let mail = build_bundle(
            &alice,
            call("SA0KAM"),
            call("SO5KM-1"),
            "sked",
            Some("40m"),
            Precedence::Priority,
            None,
        )
        .unwrap();
        assert_eq!(mail.bundle().kind, Kind::Mail);
        assert!(mail.bundle().seq.is_none());

        let wire = chat.to_vec();
        let unknown = open_message(call("SA0KAM"), &wire, &Trust::default(), None);
        assert_eq!(unknown.verification, Verification::Unverified);
        assert_eq!(unknown.text().as_deref(), Some("73"));

        let mut trust = Trust::default();
        trust.insert(alice.call, alice.identity.public());
        let good = open_message(call("SA0KAM"), &wire, &trust, None);
        assert_eq!(good.verification, Verification::Verified);
        assert_eq!(good.id, chat.id());

        trust.insert(alice.call, bob.identity.public());
        let bad = open_message(call("SA0KAM"), &wire, &trust, None);
        assert_eq!(bad.verification, Verification::BadSignature);

        let peer = alice.identity.public();
        let tofu = open_message(call("SA0KAM"), &wire, &Trust::default(), Some(&peer));
        assert_eq!(tofu.verification, Verification::Verified);

        let junk = open_message(call("SA0KAM"), b"not a bundle", &trust, None);
        assert!(junk.bundle.is_none());
        assert!(junk.error.is_some());
    }

    #[test]
    fn bulletin_addresses_a_group_without_a_station() {
        let alice = KeyFile::generate(call("SA0KAM")).unwrap();
        let sealed =
            build_bulletin(&alice, call("SA0KAM"), "SK-EMCOMM", "net open", Some("check-in")).unwrap();
        assert_eq!(sealed.bundle().kind, Kind::Bulletin);
        assert_eq!(sealed.bundle().to, vec![Address::Group("SK-EMCOMM".into())]);
        assert!(sealed.to_vec().len() <= MAX_BULLETIN_BYTES);
        assert!(build_bulletin(&alice, call("SA0KAM"), "", "x", None).is_err());
    }
}
