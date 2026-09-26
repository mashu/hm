//! Runs a sans-IO machine in real time.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hm_core::{Input, Machine, Millis, Output, Port};

/// Something that carries hm frames to and from a radio.
pub trait Link {
    fn send(&mut self, frame: &[u8]) -> io::Result<()>;
    /// Wait up to `wait` for a frame; `Ok(None)` on timeout.
    fn recv_timeout(&mut self, wait: Duration) -> io::Result<Option<Vec<u8>>>;
    /// What `peer` said it supports (`hm_wire::FEATURE_*` bits), for links
    /// that can frame traffic to it differently.
    fn peer_features(&mut self, _peer: hm_wire::Callsign, _features: u32) {}
    /// Feature bits this link adds to the station's OPEN.
    fn features(&self) -> u32 {
        0
    }
}

/// What the event callback wants next.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Why [`run`] returned.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum End {
    Stopped,
    TimedOut,
    Interrupted,
}

/// Longest the loop sleeps, so timeouts and the stop flag are noticed.
const MAX_WAIT: Duration = Duration::from_millis(200);

/// Feed `initial` commands to `m`, then run it against `link` until the event
/// callback says stop, `timeout` passes, or `stop` is set. Frames go out and
/// come in on `port`. The machine's clock is milliseconds since the call.
pub fn run<M, C, E, L>(
    m: &mut M,
    link: &mut L,
    port: Port,
    initial: Vec<C>,
    timeout: Option<Duration>,
    stop: Option<&AtomicBool>,
    mut on_event: impl FnMut(Millis, E) -> Flow,
) -> io::Result<End>
where
    M: Machine<Input = Input<C>, Output = Output<E>>,
    L: Link,
{
    let start = Instant::now();
    let now = || Millis(start.elapsed().as_millis() as u64);
    let mut out = Vec::new();
    for c in initial {
        m.handle(now(), Input::Command(c), &mut out);
    }
    loop {
        // Transmissions first, so an ACK queued with an event still goes out.
        let mut stopping = false;
        for o in out.drain(..) {
            match o {
                Output::Transmit { port: p, data } if p == port => link.send(&data)?,
                Output::Transmit { .. } => {}
                Output::Event(e) => stopping |= on_event(now(), e) == Flow::Stop,
            }
        }
        if stopping {
            return Ok(End::Stopped);
        }
        if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            return Ok(End::Interrupted);
        }
        if timeout.is_some_and(|t| start.elapsed() >= t) {
            return Ok(End::TimedOut);
        }
        let t = now();
        match m.next_deadline() {
            Some(d) if d <= t => m.on_deadline(t, &mut out),
            next => {
                let wait = next
                    .map_or(MAX_WAIT, |d| Duration::from_millis(d.0 - t.0))
                    .min(MAX_WAIT);
                if let Some(frame) = link.recv_timeout(wait)? {
                    m.handle(now(), Input::Frame { port, data: frame }, &mut out);
                }
            }
        }
    }
}
