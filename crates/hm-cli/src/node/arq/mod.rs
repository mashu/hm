//! An ARQ modem (VARA, Mercury or ARDOP) as a bearer.
//!
//! These modems run as programs of their own and offer a TCP host interface:
//! text commands and events on one port, the data of a connection on the
//! next. A connection to another station is a reliable byte stream, repaired
//! by the modem's own ARQ, so bundles travel over it the way they travel over
//! the internet (`docs/spec.md`, section 10): `"HMD0"`, the length, the bundle; then
//! `0x00` and the receiver's signed receipt (base callsigns, session 0, as on
//! the internet), or `0x01` and a reason. The receipt is checked against the
//! receiver's trusted key and proves next-hop custody. Final delivery still
//! requires the destination's end-to-end bundle receipt.
//!
//! One connection at a time, as the modems allow: deliveries to other
//! stations wait their turn. A delivery calls the station, sends every waiting
//! bundle for it, then hangs up. Calls from other stations are answered.
//!
//! VARA (and Mercury, which speaks the same interface) sends data as a plain
//! stream; ARDOP frames it with a length (and on receive a 3-byte tag). When
//! the modem asks the host to key the radio (`PTT ON` / `PTT TRUE`), the
//! node keys it with the usual PTT options, and always unkeys it again.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hm_ident::Identity;
use hm_net::{Accept, Verdict};
use hm_node::Transfer;
use hm_rig::ptt::Ptt;
use hm_wire::stream::{next_message, StreamLimits, StreamMessage, OBJECT_MAGIC};
use hm_wire::Callsign;
use hm_xfer::{object_id, receipt_statement};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use super::live::LiveConfig;
use crate::sound_link::PttFactory;

/// Largest bundle accepted over a modem connection.
pub const MAX_OBJECT: usize = 256 * 1024;
/// A modem connection carries bundles and their answers, no control messages.
const LIMITS: StreamLimits = StreamLimits {
    max_object: MAX_OBJECT,
    max_control: 0,
};
/// A call not answered in this long has failed.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest wait for a receipt once a bundle is sent (HF is slow).
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(600);
/// A connection with nothing happening for this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const RECONNECT: Duration = Duration::from_secs(5);

/// Which host interface the modem speaks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// VARA HF and FM, and Mercury.
    Vara,
    /// ARDOP (ardopc, ardopcf).
    Ardop,
}

impl Kind {
    pub fn parse(s: &str) -> Result<Kind, String> {
        match s {
            "vara" | "mercury" => Ok(Kind::Vara),
            "ardop" => Ok(Kind::Ardop),
            other => Err(format!("modem kind {other:?}: use vara, mercury or ardop")),
        }
    }
}

#[derive(Clone)]
pub struct ArqConfig {
    pub kind: Kind,
    /// Host of the modem program.
    pub host: String,
    /// Its command port; data is on the next port.
    pub port: u16,
    /// Bandwidth to ask for, in Hz (VARA HF: 500, 2300, 2750; ARDOP: 200 to 2000). 0 leaves it.
    pub bandwidth: u32,
    /// Keys the radio when the modem asks the host to; `None` when the modem keys it itself.
    pub ptt: Option<PttFactory>,
}

impl ArqConfig {
    pub fn describe(&self) -> String {
        let kind = match self.kind {
            Kind::Vara => "VARA",
            Kind::Ardop => "ARDOP",
        };
        format!("{kind} modem at {}:{}", self.host, self.port)
    }

    /// Conservative effective rate for route capacity and airtime estimates.
    /// Host protocols expose occupied bandwidth, not the currently negotiated
    /// modulation rate, so use one bit/s per configured Hz.
    pub fn estimated_rate_bps(&self) -> u32 {
        if self.bandwidth > 0 {
            self.bandwidth
        } else {
            match self.kind {
                Kind::Vara => 1_200,
                Kind::Ardop => 500,
            }
        }
    }
}

/// What the modem is doing, for the status page.
#[derive(Clone, Debug, Default)]
pub struct ArqStatus {
    /// Connected to the modem program.
    pub up: bool,
    /// The station a connection is open with.
    pub peer: Option<Callsign>,
}

struct Request {
    to: Callsign,
    object: Vec<u8>,
    reply: oneshot::Sender<Transfer>,
}

/// Handle to the modem task.
pub struct Arq {
    tx: mpsc::UnboundedSender<Request>,
    status: Arc<Mutex<ArqStatus>>,
    describe: String,
}

impl Arq {
    /// Start the modem task on the current tokio runtime.
    pub fn start(
        cfg: ArqConfig,
        me: Callsign,
        identity: Identity,
        live: Arc<LiveConfig>,
        accept: Accept,
    ) -> Arq {
        let (tx, rx) = mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(ArqStatus::default()));
        let describe = cfg.describe();
        let task = Task {
            cfg,
            me,
            identity,
            live,
            accept,
            status: status.clone(),
            requests: rx,
            waiting: VecDeque::new(),
        };
        tokio::spawn(task.run());
        Arq { tx, status, describe }
    }

    pub fn status(&self) -> ArqStatus {
        self.status.lock().expect("lock").clone()
    }

    pub fn describe(&self) -> &str {
        &self.describe
    }

    /// Send `object` to `to` and wait for its verified receipt.
    pub async fn deliver(&self, to: Callsign, object: Vec<u8>) -> Transfer {
        let (reply, answer) = oneshot::channel();
        if self.tx.send(Request { to, object, reply }).is_err() {
            return Transfer::Failed("the modem task has stopped".into());
        }
        answer
            .await
            .unwrap_or_else(|_| Transfer::Failed("the modem task has stopped".into()))
    }
}

/// Unkeys the radio when dropped.
struct Keyer(Option<Box<dyn Ptt>>);

impl Keyer {
    fn set(&mut self, on: bool) {
        if let Some(p) = self.0.as_mut() {
            if let Err(e) = p.set(on) {
                super::log(format!("modem PTT: {e}"));
            }
        }
    }
}

impl Drop for Keyer {
    fn drop(&mut self) {
        self.set(false);
    }
}

mod stream;
mod task;

use stream::*;
use task::*;

#[cfg(test)]
mod tests;
