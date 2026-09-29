//! The built-in AFSK 1200 modem as a radio link.
//!
//! A thread captures audio, demodulates it and passes up the hm frames found
//! inside AX.25 UI frames, exactly as on the KISS path, so stations using the
//! built-in modem and stations using Direwolf understand each other.
//!
//! Frames to send are gathered for a moment, so a whole transfer burst goes out
//! in one key-up, and wait for the channel: p-persistent CSMA on the
//! demodulator's carrier detect (while the channel is busy, wait a slot; when
//! clear, transmit with probability (p + 1) / 256, else wait a slot).
//! PTT is released on every exit path and no transmission exceeds `max_tx`.
//!
//! Frames go out as AX.25 in HDLC, which every packet station decodes, or in
//! IL2P, whose Reed–Solomon parity carries them through far more noise
//! ([`Framing`]). Both are always decoded.

use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hm_bearer::ax25;
use hm_core::DetRng;
use hm_modem_afsk::{Demodulator, DemodulatorConfig, Modulator};
use hm_rig::ptt::Ptt;
use hm_rig::AudioPort;
use hm_wire::{Callsign, Dest, FrameHeader, FEATURE_COMPACT, FEATURE_IL2P};

use crate::driver::Link;

pub type AudioFactory = Arc<dyn Fn() -> io::Result<Box<dyn AudioPort>> + Send + Sync>;
pub type PttFactory = Arc<dyn Fn() -> io::Result<Box<dyn Ptt>> + Send + Sync>;

/// How frames go on air.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Framing {
    /// AX.25 in HDLC, as every packet station decodes.
    #[default]
    Ax25,
    /// IL2P with maximum FEC: NinoTNC, Direwolf 1.7 and hm decode it.
    Il2p,
    /// IL2P to stations whose OPEN says they decode it, AX.25 to the rest
    /// (and for beacons, which everyone should hear).
    Auto,
}

impl Framing {
    pub fn parse(s: &str) -> Result<Framing, String> {
        match s {
            "ax25" => Ok(Framing::Ax25),
            "il2p" => Ok(Framing::Il2p),
            "auto" => Ok(Framing::Auto),
            other => Err(format!("framing {other:?}: use ax25, il2p or auto")),
        }
    }
}

/// Channel access, keying and framing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Csma {
    /// Transmit probability per clear slot is (persist + 1) / 256; 63 is the usual 25%.
    pub persist: u8,
    pub slot: Duration,
    pub txdelay_ms: u32,
    /// Longest single transmission; longer bursts are split.
    pub max_tx: Duration,
    pub framing: Framing,
}

impl Default for Csma {
    fn default() -> Self {
        Csma {
            persist: 63,
            slot: Duration::from_millis(100),
            txdelay_ms: 300,
            max_tx: Duration::from_secs(30),
            framing: Framing::Ax25,
        }
    }
}

const GATHER: Duration = Duration::from_millis(20);
const CAPTURE_WAIT: Duration = Duration::from_millis(20);

/// A frame to send, and whether it goes in IL2P.
type Outgoing = (Vec<u8>, bool);

/// What has gone out on air: frames in all, and when the last key-up ended.
#[derive(Default)]
struct Aired {
    frames: u64,
    at: Option<Instant>,
}

pub struct SoundLink {
    me: Callsign,
    framing: Framing,
    /// Stations that told us they decode IL2P.
    il2p_peers: BTreeSet<Callsign>,
    /// Stations that told us they read compact frames.
    compact_peers: BTreeSet<Callsign>,
    to_air: Option<Sender<Outgoing>>,
    from_air: Receiver<Vec<u8>>,
    /// Frames given to the modem thread so far, and what it has sent of them.
    handed: u64,
    aired: Arc<Mutex<Aired>>,
    /// The end of the key-up [`Link::drained`] last reported.
    reported: Option<Instant>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    failure: Arc<Mutex<Option<String>>>,
}

/// Unkeys the transmitter when dropped, whatever happened.
struct Keyed<'a>(&'a mut dyn Ptt);

impl Drop for Keyed<'_> {
    fn drop(&mut self) {
        let _ = self.0.set(false);
    }
}

impl SoundLink {
    /// Open PTT and audio (audio inside the link's thread) and start listening.
    pub fn start(me: Callsign, audio: AudioFactory, ptt: PttFactory, csma: Csma) -> io::Result<SoundLink> {
        ax25::Address::from_callsign(me).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{me} cannot be an AX.25 source"),
            )
        })?;
        let mut ptt = ptt()?;
        ptt.set(false)?;
        let (to_air, air_in) = mpsc::channel::<Outgoing>();
        let (air_out, from_air) = mpsc::channel::<Vec<u8>>();
        let (opened_tx, opened_rx) = mpsc::channel::<io::Result<()>>();
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let aired = Arc::new(Mutex::new(Aired::default()));
        let (st, fail, sent) = (stop.clone(), failure.clone(), aired.clone());
        let thread = thread::Builder::new().name("modem".into()).spawn(move || {
            let port = match audio() {
                Ok(p) => {
                    let _ = opened_tx.send(Ok(()));
                    p
                }
                Err(e) => {
                    let _ = opened_tx.send(Err(e));
                    return;
                }
            };
            if let Err(e) = run(port, ptt.as_mut(), csma, air_in, air_out, &sent, &st) {
                *fail.lock().expect("lock") = Some(e.to_string());
            }
        })?;
        opened_rx
            .recv()
            .map_err(|_| io::Error::other("modem thread ended"))??;
        Ok(SoundLink {
            me,
            framing: csma.framing,
            il2p_peers: BTreeSet::new(),
            compact_peers: BTreeSet::new(),
            to_air: Some(to_air),
            from_air,
            handed: 0,
            aired,
            reported: None,
            stop,
            thread: Some(thread),
            failure,
        })
    }

    fn failed(&self) -> io::Error {
        let why = self
            .failure
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_else(|| "modem stopped".into());
        io::Error::new(io::ErrorKind::BrokenPipe, why)
    }
}

impl Drop for SoundLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.to_air.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Link for SoundLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        let to = match FrameHeader::decode(frame) {
            Ok((
                FrameHeader {
                    dst: Dest::Station(c),
                    ..
                },
                _,
            )) => Some(c),
            _ => None,
        };
        let compact = to.is_some_and(|c| self.compact_peers.contains(&c));
        let ui = ax25::wrap_frame(self.me, frame, compact)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e:?}")))?;
        let il2p = match self.framing {
            Framing::Ax25 => false,
            Framing::Il2p => true,
            Framing::Auto => to.is_some_and(|c| self.il2p_peers.contains(&c)),
        };
        let tx = self.to_air.as_ref().expect("open until dropped");
        tx.send((ui, il2p)).map_err(|_| self.failed())?;
        self.handed += 1;
        Ok(())
    }

    fn drained(&mut self) -> Option<Instant> {
        let aired = self.aired.lock().expect("lock");
        if aired.frames < self.handed || aired.at == self.reported {
            return None;
        }
        self.reported = aired.at;
        aired.at
    }

    fn peer_features(&mut self, peer: Callsign, features: u32) {
        for (feature, peers) in [
            (FEATURE_IL2P, &mut self.il2p_peers),
            (FEATURE_COMPACT, &mut self.compact_peers),
        ] {
            if features & feature != 0 {
                peers.insert(peer);
            } else {
                peers.remove(&peer);
            }
        }
    }

    /// The modem decodes IL2P whatever it sends, and reads compact frames.
    fn features(&self) -> u32 {
        FEATURE_IL2P | FEATURE_COMPACT
    }

    fn recv_timeout(&mut self, wait: Duration) -> io::Result<Option<Vec<u8>>> {
        match self.from_air.recv_timeout(wait) {
            Ok(f) => Ok(Some(f)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(self.failed()),
        }
    }
}

/// Seconds a frame of `len` bytes takes on air, with room for bit stuffing.
fn frame_secs(len: usize) -> f64 {
    (len + 4) as f64 * 8.0 * 1.2 / 1200.0
}

fn run(
    mut port: Box<dyn AudioPort>,
    ptt: &mut dyn Ptt,
    csma: Csma,
    air_in: Receiver<Outgoing>,
    air_out: Sender<Vec<u8>>,
    aired: &Mutex<Aired>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let fs = port.sample_rate();
    let mut demod = Demodulator::new(DemodulatorConfig::new(fs));
    let modulator = Modulator::new(fs);
    let mut rng = DetRng::from_seed(getrandom::u64().unwrap_or(0x5eed));
    let mut pending: VecDeque<Outgoing> = VecDeque::new();
    let mut last_enqueue = Instant::now();
    let mut next_try = Instant::now();
    let (mut audio, mut frames) = (Vec::new(), Vec::new());
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        audio.clear();
        port.capture(&mut audio, CAPTURE_WAIT)?;
        demod.process(&audio, &mut frames);
        for f in frames.drain(..) {
            if let Some(hm) = ax25::unwrap_frame(&f) {
                if air_out.send(hm).is_err() {
                    return Ok(());
                }
            }
        }
        loop {
            match air_in.try_recv() {
                Ok(f) => {
                    pending.push_back(f);
                    last_enqueue = Instant::now();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        let now = Instant::now();
        if pending.is_empty() || now < next_try || now - last_enqueue < GATHER {
            continue;
        }
        if demod.dcd() || rng.below(256) > csma.persist as u64 {
            next_try = now + csma.slot;
            continue;
        }
        // One key-up carries frames of one framing, up to `max_tx`.
        let mut batch = Vec::new();
        let il2p = pending.front().is_some_and(|(_, il2p)| *il2p);
        let mut secs = csma.txdelay_ms as f64 / 1000.0;
        while let Some((f, i)) = pending.front() {
            let t = frame_secs(f.len());
            if *i != il2p || (!batch.is_empty() && secs + t > csma.max_tx.as_secs_f64()) {
                break;
            }
            secs += t;
            batch.push(pending.pop_front().expect("front exists").0);
        }
        let refs: Vec<&[u8]> = batch.iter().map(|f| f.as_slice()).collect();
        let samples = if il2p {
            modulator.modulate_il2p(&refs, csma.txdelay_ms, true)
        } else {
            modulator.modulate(&refs, csma.txdelay_ms)
        };
        ptt.set(true)?;
        let keyed = Keyed(ptt);
        port.play(&samples)?;
        drop(keyed);
        let mut sent = aired.lock().expect("lock");
        sent.frames += batch.len() as u64;
        sent.at = Some(Instant::now());
    }
}
