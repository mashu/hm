//! The task that drives the modem's host interface: registering the
//! callsign, calling, answering, sending and receiving bundles, hanging up.

use super::*;

/// The modem program's two ports.
pub(super) struct Ports {
    cmd_r: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    cmd_w: tokio::net::tcp::OwnedWriteHalf,
    data_r: tokio::net::tcp::OwnedReadHalf,
    data_w: tokio::net::tcp::OwnedWriteHalf,
    /// ARDOP's receive framing: bytes of a frame still to come, and whether it is ARQ data.
    ardop_left: usize,
    ardop_head: Vec<u8>,
    ardop_keep: bool,
}

pub(super) enum State {
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

pub(super) struct Task {
    pub(super) cfg: ArqConfig,
    pub(super) me: Callsign,
    pub(super) identity: Identity,
    pub(super) live: Arc<LiveConfig>,
    pub(super) accept: Accept,
    pub(super) status: Arc<Mutex<ArqStatus>>,
    pub(super) requests: mpsc::UnboundedReceiver<Request>,
    pub(super) waiting: VecDeque<Request>,
}

impl Task {
    pub(super) async fn run(mut self) {
        let mut last_error = String::new();
        loop {
            match self.session().await {
                Ok(()) => return, // the node is shutting down
                Err(e) => {
                    let e = e.to_string();
                    if e != last_error {
                        crate::node::log(format!("{}: {e}", self.cfg.describe()));
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
                        let _ = r
                            .reply
                            .send(Transfer::Failed("the modem is not reachable".into()));
                    }
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            for r in self.waiting.drain(..) {
                let _ = r
                    .reply
                    .send(Transfer::Failed("the modem is not reachable".into()));
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
        // Data that arrives before the CONNECTED line saying whose it is: the
        // modem's command and data ports are separate streams, so a peer that
        // starts sending as soon as it is connected can beat that line here.
        let mut early: Vec<u8> = Vec::new();
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
                    let was_connected = matches!(state, State::Connected { .. });
                    state = self.on_event(&mut p, &mut keyer, state, &text).await?;
                    match (matches!(state, State::Connected { .. }), was_connected) {
                        (true, false) => {
                            let bytes = std::mem::take(&mut early);
                            if opens_connection(&bytes) {
                                state = self.on_data(&mut p, state, &bytes).await?;
                            } else if !bytes.is_empty() {
                                crate::node::log(format!(
                                    "modem: dropped {} bytes left over from an earlier connection",
                                    bytes.len()
                                ));
                            }
                        }
                        // The connection ended: nothing before it belongs to the next one.
                        (false, true) => early.clear(),
                        _ => {}
                    }
                }
                r = p.data_r.read(&mut buf) => {
                    let n = r?;
                    if n == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the modem closed its data port"));
                    }
                    let mut bytes = Vec::new();
                    let chunk = buf[..n].to_vec();
                    self.unwrap_data(&mut p, &chunk, &mut bytes);
                    if matches!(state, State::Connected { .. }) {
                        state = self.on_data(&mut p, state, &bytes).await?;
                    } else if early.len() + bytes.len() <= EARLY_LIMIT {
                        early.extend_from_slice(&bytes);
                    } else {
                        early.clear();
                    }
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
                    crate::node::log(format!("modem: {text}: no station named"));
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
                            crate::node::log(format!(
                                "modem: ignored a second connection to {peer} while connected to {current_peer}"
                            ));
                            self.set_status(true, Some(current_peer));
                        }
                        current
                    }
                    State::Calling { req, .. } if req.to == peer && we_called => {
                        crate::node::log(format!("modem: connected to {peer}"));
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
                        crate::node::log(format!("modem: {peer} called in"));
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
                        crate::node::log(format!("modem: {peer} called in"));
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
                        let _ = req.reply.send(Transfer::Failed(format!("no connection: {text}")));
                    }
                    State::Connected {
                        sent: Some((req, _)), ..
                    } => {
                        let _ = req
                            .reply
                            .send(Transfer::Failed("disconnected before the receipt came".into()));
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
                            StreamMessage::Stored(sig).encode()
                        }
                        Verdict::Busy { retry_after, reason } => {
                            rejection(&format!("busy for {retry_after} s: {reason}"))
                        }
                        Verdict::Rejected(reason) => rejection(&reason),
                    };
                    self.send_data(p, &reply).await?;
                }
                Message::Receipt(_) | Message::Rejected(_) | Message::Busy { .. } if sent.is_none() => {} // not ours
                Message::Receipt(sig) => {
                    let (req, _) = sent.take().expect("checked above");
                    let key = self.live.get().trust.key_for(req.to);
                    let statement =
                        receipt_statement(req.to.base(), self.me.base(), 0, &object_id(&req.object));
                    let result = match key {
                        Some(k) if k.verify(&statement, &sig).is_ok() => Transfer::Delivered,
                        Some(_) => Transfer::Failed("the receipt does not verify".into()),
                        None => Transfer::Failed(format!("no key for {} to check its receipt", req.to)),
                    };
                    let _ = req.reply.send(result);
                }
                Message::Rejected(reason) => {
                    let (req, _) = sent.take().expect("checked above");
                    let _ = req.reply.send(Transfer::Refused {
                        reason,
                        permanent: true,
                    });
                }
                Message::Busy { retry_after, reason } => {
                    let (req, _) = sent.take().expect("checked above");
                    let _ = req.reply.send(Transfer::Busy { retry_after, reason });
                }
                Message::Bad(why) => {
                    if let Some((req, _)) = sent.take() {
                        let _ = req.reply.send(Transfer::Failed(why.clone()));
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
                let _ = req.reply.send(Transfer::Failed("no answer to the call".into()));
                self.hang_up(p).await?;
                Ok(State::Idle)
            }
            State::Connected {
                sent: Some((req, at)),
                ..
            } if now - at > RECEIPT_TIMEOUT => {
                let _ = req.reply.send(Transfer::Failed("no receipt in time".into()));
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
