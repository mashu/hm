//! An ARQ modem (VARA, Mercury or ARDOP) as a bearer.
//!
//! These modems run as programs of their own and offer a TCP host interface:
//! text commands and events on one port, the data of a connection on the
//! next. A connection to another station is a reliable byte stream, repaired
//! by the modem's own ARQ, so bundles travel over it the way they travel over
//! the internet (SPEC section 10): `"HMD0"`, the length, the bundle; then
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
use hm_rig::ptt::Ptt;
use hm_wire::Callsign;
use hm_xfer::{object_id, receipt_statement};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use super::live::LiveConfig;
use crate::sound_link::PttFactory;

const MAGIC: &[u8; 4] = b"HMD0";
/// Largest bundle accepted over a modem connection.
pub const MAX_OBJECT: usize = 256 * 1024;
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
    reply: oneshot::Sender<Result<(), String>>,
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
    pub async fn deliver(&self, to: Callsign, object: Vec<u8>) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Request { to, object, reply })
            .map_err(|_| "the modem task has stopped".to_string())?;
        answer
            .await
            .unwrap_or_else(|_| Err("the modem task has stopped".into()))
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

/// One side of a connection's byte stream, parsed into messages.
#[derive(Default)]
struct Inbox {
    buf: Vec<u8>,
}

enum Message {
    Bundle(Vec<u8>),
    Receipt([u8; 64]),
    Rejected(String),
    Bad(String),
}

impl Inbox {
    /// The next complete message, if one has arrived. Bundles start with
    /// `"HMD0"` and answers with `0x00` or `0x01`, so both can share one
    /// connection (both stations may send at once).
    fn next(&mut self) -> Option<Message> {
        let b = &self.buf;
        let (msg, used) = match *b.first()? {
            0 if b.len() >= 65 => (Message::Receipt(b[1..65].try_into().expect("64 bytes")), 65),
            1 if b.len() >= 3 => {
                let n = u16::from_be_bytes([b[1], b[2]]) as usize;
                if b.len() < 3 + n {
                    return None;
                }
                (
                    Message::Rejected(String::from_utf8_lossy(&b[3..3 + n]).into_owned()),
                    3 + n,
                )
            }
            0 | 1 => return None,
            b'H' if b.len() < 8 => return None,
            b'H' if &b[..4] == MAGIC => {
                let len = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as usize;
                if len > MAX_OBJECT {
                    (
                        Message::Bad(format!("object of {len} bytes is too large")),
                        b.len(),
                    )
                } else if b.len() < 8 + len {
                    return None;
                } else {
                    (Message::Bundle(b[8..8 + len].to_vec()), 8 + len)
                }
            }
            _ => (Message::Bad("unknown message type".into()), b.len()),
        };
        self.buf.drain(..used);
        Some(msg)
    }
}

fn bundle_message(object: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + object.len());
    m.extend_from_slice(MAGIC);
    m.extend_from_slice(&(object.len() as u32).to_be_bytes());
    m.extend_from_slice(object);
    m
}

fn rejection(reason: &str) -> Vec<u8> {
    let bytes = &reason.as_bytes()[..reason.len().min(512)];
    let mut r = vec![1u8];
    r.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    r.extend_from_slice(bytes);
    r
}

/// The modem program's two ports.
struct Ports {
    cmd_r: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    cmd_w: tokio::net::tcp::OwnedWriteHalf,
    data_r: tokio::net::tcp::OwnedReadHalf,
    data_w: tokio::net::tcp::OwnedWriteHalf,
    /// ARDOP's receive framing: bytes of a frame still to come, and whether it is ARQ data.
    ardop_left: usize,
    ardop_head: Vec<u8>,
    ardop_keep: bool,
}

enum State {
    Idle,
    /// Calling a station for this delivery.
    Calling {
        req: Request,
        since: tokio::time::Instant,
    },
    /// Connected; `sent` is the delivery whose receipt we wait for. `ours`:
    /// we called, so we send and hang up; otherwise we answer.
    Connected {
        peer: Callsign,
        ours: bool,
        sent: Option<(Request, tokio::time::Instant)>,
        inbox: Inbox,
        last: tokio::time::Instant,
        hanging_up: bool,
    },
}

struct Task {
    cfg: ArqConfig,
    me: Callsign,
    identity: Identity,
    live: Arc<LiveConfig>,
    accept: Accept,
    status: Arc<Mutex<ArqStatus>>,
    requests: mpsc::UnboundedReceiver<Request>,
    waiting: VecDeque<Request>,
}

impl Task {
    async fn run(mut self) {
        let mut last_error = String::new();
        loop {
            match self.session().await {
                Ok(()) => return, // the node is shutting down
                Err(e) => {
                    let e = e.to_string();
                    if e != last_error {
                        super::log(format!("{}: {e}", self.cfg.describe()));
                        last_error = e;
                    }
                }
            }
            *self.status.lock().expect("lock") = ArqStatus::default();
            // Deliveries asked for while the modem is away fail at once.
            let deadline = tokio::time::Instant::now() + RECONNECT;
            loop {
                match tokio::time::timeout_at(deadline, self.requests.recv()).await {
                    Ok(Some(r)) => {
                        let _ = r.reply.send(Err("the modem is not reachable".into()));
                    }
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            for r in self.waiting.drain(..) {
                let _ = r.reply.send(Err("the modem is not reachable".into()));
            }
        }
    }

    async fn connect(&self) -> io::Result<Ports> {
        let addr = |p: u16| format!("{}:{p}", self.cfg.host);
        let cmd = TcpStream::connect(addr(self.cfg.port)).await?;
        let data = TcpStream::connect(addr(self.cfg.port + 1)).await?;
        let (cmd_r, cmd_w) = cmd.into_split();
        let (data_r, data_w) = data.into_split();
        Ok(Ports {
            cmd_r: tokio::io::BufReader::new(cmd_r),
            cmd_w,
            data_r,
            data_w,
            ardop_left: 0,
            ardop_head: Vec::new(),
            ardop_keep: false,
        })
    }

    async fn command(&self, p: &mut Ports, line: &str) -> io::Result<()> {
        p.cmd_w.write_all(format!("{line}\r").as_bytes()).await
    }

    async fn send_data(&self, p: &mut Ports, bytes: &[u8]) -> io::Result<()> {
        match self.cfg.kind {
            Kind::Vara => p.data_w.write_all(bytes).await,
            Kind::Ardop => {
                for chunk in bytes.chunks(4096) {
                    let mut f = (chunk.len() as u16).to_be_bytes().to_vec();
                    f.extend_from_slice(chunk);
                    p.data_w.write_all(&f).await?;
                }
                Ok(())
            }
        }
    }

    /// Received data bytes, unwrapped from ARDOP's framing where needed.
    fn unwrap_data(&self, p: &mut Ports, bytes: &[u8], out: &mut Vec<u8>) {
        if self.cfg.kind == Kind::Vara {
            out.extend_from_slice(bytes);
            return;
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            if p.ardop_left == 0 {
                // Length (2 bytes) and tag (3 bytes) first.
                let need = 5 - p.ardop_head.len();
                let take = need.min(rest.len());
                p.ardop_head.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if p.ardop_head.len() == 5 {
                    let len = u16::from_be_bytes([p.ardop_head[0], p.ardop_head[1]]) as usize;
                    p.ardop_keep = &p.ardop_head[2..5] == b"ARQ";
                    p.ardop_left = len.saturating_sub(3);
                    p.ardop_head.clear();
                }
                continue;
            }
            let take = p.ardop_left.min(rest.len());
            if p.ardop_keep {
                out.extend_from_slice(&rest[..take]);
            }
            p.ardop_left -= take;
            rest = &rest[take..];
        }
    }

    fn set_status(&self, up: bool, peer: Option<Callsign>) {
        *self.status.lock().expect("lock") = ArqStatus { up, peer };
    }

    /// Talk to the modem until the link to it fails (`Err`) or the node stops (`Ok`).
    async fn session(&mut self) -> io::Result<()> {
        let mut p = self.connect().await?;
        let mut keyer = Keyer(match &self.cfg.ptt {
            Some(f) => Some(f()?),
            None => None,
        });
        match self.cfg.kind {
            Kind::Vara => {
                self.command(&mut p, &format!("MYCALL {}", self.me)).await?;
                if self.cfg.bandwidth > 0 {
                    self.command(&mut p, &format!("BW{}", self.cfg.bandwidth)).await?;
                }
                self.command(&mut p, "LISTEN ON").await?;
            }
            Kind::Ardop => {
                self.command(&mut p, "INITIALIZE").await?;
                self.command(&mut p, &format!("MYCALL {}", self.me)).await?;
                self.command(&mut p, "PROTOCOLMODE ARQ").await?;
                if self.cfg.bandwidth > 0 {
                    self.command(&mut p, &format!("ARQBW {}MAX", self.cfg.bandwidth))
                        .await?;
                }
                self.command(&mut p, "LISTEN TRUE").await?;
            }
        }
        self.set_status(true, None);
        let mut state = State::Idle;
        let mut line = Vec::new();
        let mut buf = vec![0u8; 4096];
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            // Start the next delivery when free.
            if matches!(state, State::Idle) {
                if let Some(req) = self.waiting.pop_front() {
                    let call = match self.cfg.kind {
                        Kind::Vara => format!("CONNECT {} {}", self.me, req.to),
                        Kind::Ardop => format!("ARQCALL {} 5", req.to),
                    };
                    self.command(&mut p, &call).await?;
                    state = State::Calling {
                        req,
                        since: tokio::time::Instant::now(),
                    };
                }
            }
            tokio::select! {
                r = self.requests.recv() => match r {
                    Some(req) => self.waiting.push_back(req),
                    None => return Ok(()),
                },
                r = tokio::io::AsyncBufReadExt::read_until(&mut p.cmd_r, b'\r', &mut line) => {
                    if r? == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the modem closed its command port"));
                    }
                    let text = String::from_utf8_lossy(&line).trim().to_string();
                    line.clear();
                    state = self.on_event(&mut p, &mut keyer, state, &text).await?;
                }
                r = p.data_r.read(&mut buf) => {
                    let n = r?;
                    if n == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the modem closed its data port"));
                    }
                    let mut bytes = Vec::new();
                    let chunk = buf[..n].to_vec();
                    self.unwrap_data(&mut p, &chunk, &mut bytes);
                    state = self.on_data(&mut p, state, &bytes).await?;
                }
                _ = tick.tick() => {
                    state = self.on_tick(&mut p, state).await?;
                }
            }
        }
    }

    async fn hang_up(&self, p: &mut Ports) -> io::Result<()> {
        self.command(p, "DISCONNECT").await
    }

    async fn on_event(
        &mut self,
        p: &mut Ports,
        keyer: &mut Keyer,
        state: State,
        text: &str,
    ) -> io::Result<State> {
        let words: Vec<&str> = text.split_whitespace().collect();
        match words.as_slice() {
            ["PTT", "ON" | "TRUE"] => keyer.set(true),
            ["PTT", "OFF" | "FALSE"] => keyer.set(false),
            ["CONNECTED", rest @ ..] => {
                // VARA: CONNECTED <caller> <called> [bw]; ARDOP: CONNECTED <remote> <bw>.
                let calls: Vec<Callsign> = rest
                    .iter()
                    .filter(|w| !w.chars().all(|c| c.is_ascii_digit())) // the bandwidth
                    .filter_map(|w| Callsign::parse(w).ok())
                    .collect();
                let peer = calls.iter().copied().find(|c| *c != self.me);
                let Some(peer) = peer else {
                    super::log(format!("modem: {text}: no station named"));
                    self.hang_up(p).await?;
                    return Ok(state);
                };
                self.set_status(true, Some(peer));
                let now = tokio::time::Instant::now();
                // VARA names the caller first; ARDOP does not say, so a call of
                // ours that connects to the station we called is taken as ours.
                let we_called = match self.cfg.kind {
                    Kind::Vara => calls.first() == Some(&self.me),
                    Kind::Ardop => true,
                };
                return Ok(match state {
                    current @ State::Connected {
                        peer: current_peer, ..
                    } => {
                        if current_peer != peer {
                            super::log(format!(
                                "modem: ignored a second connection to {peer} while connected to {current_peer}"
                            ));
                            self.set_status(true, Some(current_peer));
                        }
                        current
                    }
                    State::Calling { req, .. } if req.to == peer && we_called => {
                        super::log(format!("modem: connected to {peer}"));
                        self.send_data(p, &bundle_message(&req.object)).await?;
                        State::Connected {
                            peer,
                            ours: true,
                            sent: Some((req, now)),
                            inbox: Inbox::default(),
                            last: now,
                            hanging_up: false,
                        }
                    }
                    State::Calling { req, .. } => {
                        // Someone else called at the same moment: our call waits.
                        super::log(format!("modem: {peer} called in"));
                        self.waiting.push_front(req);
                        State::Connected {
                            peer,
                            ours: false,
                            sent: None,
                            inbox: Inbox::default(),
                            last: now,
                            hanging_up: false,
                        }
                    }
                    _ => {
                        super::log(format!("modem: {peer} called in"));
                        State::Connected {
                            peer,
                            ours: false,
                            sent: None,
                            inbox: Inbox::default(),
                            last: now,
                            hanging_up: false,
                        }
                    }
                });
            }
            ["DISCONNECTED", ..] | ["REJECTEDBUSY", ..] | ["REJECTEDBW", ..] | ["CANCELPENDING", ..] => {
                if matches!(words.first(), Some(&"CANCELPENDING")) && !matches!(state, State::Calling { .. })
                {
                    return Ok(state);
                }
                self.set_status(true, None);
                match state {
                    State::Calling { req, .. } => {
                        let _ = req.reply.send(Err(format!("no connection: {text}")));
                    }
                    State::Connected {
                        sent: Some((req, _)), ..
                    } => {
                        let _ = req.reply.send(Err("disconnected before the receipt came".into()));
                    }
                    _ => {}
                }
                return Ok(State::Idle);
            }
            _ => {} // OK, BUFFER, BUSY, NEWSTATE, IAMALIVE and the rest
        }
        Ok(state)
    }

    async fn on_data(&mut self, p: &mut Ports, state: State, bytes: &[u8]) -> io::Result<State> {
        let State::Connected {
            peer,
            ours,
            mut sent,
            mut inbox,
            hanging_up,
            ..
        } = state
        else {
            return Ok(state); // data outside a connection is dropped
        };
        inbox.buf.extend_from_slice(bytes);
        while let Some(msg) = inbox.next() {
            match msg {
                Message::Bundle(object) => {
                    let id = object_id(&object);
                    let accept = self.accept.clone();
                    let verdict = tokio::task::spawn_blocking(move || accept(peer, object))
                        .await
                        .unwrap_or_else(|_| Verdict::Rejected("internal error".into()));
                    let reply = match verdict {
                        Verdict::Stored | Verdict::Duplicate => {
                            let sig =
                                self.identity
                                    .sign(&receipt_statement(self.me.base(), peer.base(), 0, &id));
                            let mut r = vec![0u8];
                            r.extend_from_slice(&sig);
                            r
                        }
                        Verdict::Busy { retry_after, reason } => {
                            rejection(&format!("busy for {retry_after} s: {reason}"))
                        }
                        Verdict::Rejected(reason) => rejection(&reason),
                    };
                    self.send_data(p, &reply).await?;
                }
                Message::Receipt(_) | Message::Rejected(_) if sent.is_none() => {} // not ours
                Message::Receipt(sig) => {
                    let (req, _) = sent.take().expect("checked above");
                    let key = self.live.get().trust.key_for(req.to);
                    let statement =
                        receipt_statement(req.to.base(), self.me.base(), 0, &object_id(&req.object));
                    let result = match key {
                        Some(k) if k.verify(&statement, &sig).is_ok() => Ok(()),
                        Some(_) => Err("the receipt does not verify".into()),
                        None => Err(format!("no key for {} to check its receipt", req.to)),
                    };
                    let _ = req.reply.send(result);
                }
                Message::Rejected(reason) => {
                    let (req, _) = sent.take().expect("checked above");
                    let _ = req.reply.send(Err(format!("refused: {reason}")));
                }
                Message::Bad(why) => {
                    if let Some((req, _)) = sent.take() {
                        let _ = req.reply.send(Err(why.clone()));
                    } else {
                        self.send_data(p, &rejection(&why)).await?;
                    }
                }
            }
        }
        let now = tokio::time::Instant::now();
        let mut hanging_up = hanging_up;
        if sent.is_none() && !hanging_up {
            // The next bundle for this station goes on this connection,
            // whoever called; with nothing more, the caller hangs up.
            sent = self.send_next(p, peer).await?;
            if sent.is_none() && ours {
                self.hang_up(p).await?;
                hanging_up = true;
            }
        }
        Ok(State::Connected {
            peer,
            ours,
            sent,
            inbox,
            last: now,
            hanging_up,
        })
    }

    /// Send the next waiting bundle for `peer`, if any.
    async fn send_next(
        &mut self,
        p: &mut Ports,
        peer: Callsign,
    ) -> io::Result<Option<(Request, tokio::time::Instant)>> {
        let Some(i) = self.waiting.iter().position(|r| r.to == peer) else {
            return Ok(None);
        };
        let req = self.waiting.remove(i).expect("found");
        self.send_data(p, &bundle_message(&req.object)).await?;
        Ok(Some((req, tokio::time::Instant::now())))
    }

    async fn on_tick(&mut self, p: &mut Ports, state: State) -> io::Result<State> {
        let now = tokio::time::Instant::now();
        // Something for the connected station, asked for while the line was busy.
        let state = match state {
            State::Connected {
                peer,
                ours,
                sent: None,
                inbox,
                last: _,
                hanging_up: false,
            } if self.waiting.iter().any(|request| request.to == peer) => State::Connected {
                peer,
                ours,
                sent: self.send_next(p, peer).await?,
                inbox,
                last: now,
                hanging_up: false,
            },
            current => current,
        };
        match state {
            State::Calling { req, since } if now - since > CALL_TIMEOUT => {
                let _ = req.reply.send(Err("no answer to the call".into()));
                self.hang_up(p).await?;
                Ok(State::Idle)
            }
            State::Connected {
                sent: Some((req, at)),
                ..
            } if now - at > RECEIPT_TIMEOUT => {
                let _ = req.reply.send(Err("no receipt in time".into()));
                self.hang_up(p).await?;
                Ok(State::Idle)
            }
            State::Connected {
                peer,
                ours,
                sent: None,
                inbox,
                last,
                hanging_up: false,
            } if now - last > IDLE_TIMEOUT => {
                self.hang_up(p).await?;
                Ok(State::Connected {
                    peer,
                    ours,
                    sent: None,
                    inbox,
                    last,
                    hanging_up: true,
                })
            }
            s => Ok(s),
        }
    }
}
