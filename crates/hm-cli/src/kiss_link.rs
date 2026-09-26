//! A link to a KISS TNC, over TCP (Direwolf listens on port 8001 by default)
//! or a serial port (hardware TNCs: NinoTNC, Mobilinkd, TNC-Pi, a TNC-2 in
//! KISS mode, or Direwolf on a pseudo-terminal).
//!
//! Outgoing hm frames are wrapped in AX.25 UI frames from the station's
//! callsign to `HMNET`; incoming KISS data frames on our TNC port are unwrapped
//! and anything that is not an hm frame (APRS, other traffic) is ignored.
//!
//! A hardware TNC keys the radio and runs channel access itself, with the
//! TXDELAY, persistence and slot time its host gives it, so on a serial port
//! the link sends those as KISS parameters when it opens. Direwolf takes them
//! from its own configuration, so on TCP they are left alone.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use hm_bearer::ax25;
use hm_bearer::kiss::{self, Command};
use hm_wire::Callsign;

use crate::driver::Link;

/// Largest KISS frame accepted from the TNC.
const MAX_KISS_FRAME: usize = 4096;
/// How often a serial reader looks up to see whether the link was closed.
const SERIAL_POLL: Duration = Duration::from_millis(100);
pub const DEFAULT_BAUD: u32 = 9600;

/// Where the TNC is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KissTarget {
    /// `HOST:PORT`, or `tcp:HOST:PORT`.
    Tcp(String),
    /// `serial:DEVICE[:BAUD]`, or a bare `/dev/...` or `COMn` device.
    Serial { device: String, baud: u32 },
}

impl KissTarget {
    pub fn parse(spec: &str) -> Result<KissTarget, String> {
        if let Some(rest) = spec.strip_prefix("serial:") {
            let (device, baud) = match rest.rsplit_once(':') {
                Some((d, b)) if !d.is_empty() && b.chars().all(|c| c.is_ascii_digit()) && !b.is_empty() => {
                    let baud = b.parse().map_err(|_| format!("{spec}: bad baud rate"))?;
                    (d, baud)
                }
                _ => (rest, DEFAULT_BAUD),
            };
            if device.is_empty() {
                return Err(format!("{spec}: no serial device"));
            }
            if baud == 0 {
                return Err(format!("{spec}: bad baud rate"));
            }
            return Ok(KissTarget::Serial {
                device: device.to_string(),
                baud,
            });
        }
        let upper = spec.to_ascii_uppercase();
        let com_port =
            upper.len() > 3 && upper.starts_with("COM") && upper[3..].chars().all(|c| c.is_ascii_digit());
        if spec.starts_with("/dev/") || com_port {
            return Ok(KissTarget::Serial {
                device: spec.to_string(),
                baud: DEFAULT_BAUD,
            });
        }
        let addr = spec.strip_prefix("tcp:").unwrap_or(spec);
        if !addr.contains(':') {
            return Err(format!(
                "{spec}: expected HOST:PORT for a KISS TCP server, or serial:DEVICE[:BAUD]"
            ));
        }
        Ok(KissTarget::Tcp(addr.to_string()))
    }

    pub fn describe(&self) -> String {
        match self {
            KissTarget::Tcp(addr) => format!("KISS TNC {addr}"),
            KissTarget::Serial { device, baud } => format!("KISS TNC on {device} at {baud} Bd"),
        }
    }
}

/// Channel access a hardware TNC is told to use.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TncParams {
    pub txdelay_ms: u32,
    /// Transmit probability per clear slot is (persist + 1) / 256.
    pub persist: u8,
    pub slot_ms: u32,
}

impl Default for TncParams {
    fn default() -> Self {
        TncParams {
            txdelay_ms: 300,
            persist: 63,
            slot_ms: 100,
        }
    }
}

impl TncParams {
    /// The KISS parameter frames for `tnc_port` (times in 10 ms units, at most 2.55 s).
    pub fn frames(&self, tnc_port: u8) -> Vec<u8> {
        let tens = |ms: u32| ms.div_ceil(10).min(255) as u8;
        let mut out = kiss::param_frame(tnc_port, Command::TxDelay, tens(self.txdelay_ms));
        out.extend(kiss::param_frame(tnc_port, Command::Persistence, self.persist));
        out.extend(kiss::param_frame(tnc_port, Command::SlotTime, tens(self.slot_ms)));
        out
    }
}

pub struct KissLink {
    writer: Box<dyn Write + Send>,
    frames: Receiver<Vec<u8>>,
    me: Callsign,
    tnc_port: u8,
    stop: Arc<AtomicBool>,
    /// Unblocks the reader thread when the link is dropped.
    close: Option<Box<dyn FnOnce() + Send>>,
}

fn check_source(me: Callsign) -> io::Result<()> {
    ax25::Address::from_callsign(me).map(|_| ()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{me} cannot be an AX.25 source (at most 6 letters or digits, SSID 0-15)"),
        )
    })
}

impl KissLink {
    /// Open the TNC at `target`; `me` is the callsign transmitted as the AX.25 source.
    pub fn open(target: &KissTarget, me: Callsign, tnc_port: u8, params: TncParams) -> io::Result<KissLink> {
        match target {
            KissTarget::Tcp(addr) => KissLink::connect(addr, me, tnc_port),
            KissTarget::Serial { device, baud } => KissLink::serial(device, *baud, me, tnc_port, params),
        }
    }

    /// Connect to a KISS TCP server.
    pub fn connect(addr: &str, me: Callsign, tnc_port: u8) -> io::Result<KissLink> {
        check_source(me)?;
        let writer = TcpStream::connect(addr)?;
        writer.set_nodelay(true)?;
        let reader = writer.try_clone()?;
        let closer = writer.try_clone()?;
        KissLink::start(
            me,
            tnc_port,
            Box::new(writer),
            reader,
            false,
            Some(Box::new(move || {
                let _ = closer.shutdown(Shutdown::Both);
            })),
        )
    }

    /// Open a KISS TNC on a serial port and set its channel access.
    pub fn serial(
        device: &str,
        baud: u32,
        me: Callsign,
        tnc_port: u8,
        params: TncParams,
    ) -> io::Result<KissLink> {
        check_source(me)?;
        let port = serialport::new(device, baud)
            .timeout(SERIAL_POLL)
            .open()
            .map_err(|e| io::Error::other(format!("{device}: {e}")))?;
        let reader = port
            .try_clone()
            .map_err(|e| io::Error::other(format!("{device}: {e}")))?;
        let mut link = KissLink::start(me, tnc_port, Box::new(port), reader, true, None)?;
        link.writer.write_all(&params.frames(tnc_port))?;
        link.writer.flush()?;
        Ok(link)
    }

    /// A link over any byte stream: `reader` runs on its own thread. With
    /// `timeouts`, a read that times out is not an error, so the thread can
    /// notice the link being dropped.
    fn start(
        me: Callsign,
        tnc_port: u8,
        writer: Box<dyn Write + Send>,
        mut reader: impl Read + Send + 'static,
        timeouts: bool,
        close: Option<Box<dyn FnOnce() + Send>>,
    ) -> io::Result<KissLink> {
        let (tx, frames) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        thread::Builder::new().name("kiss-reader".into()).spawn(move || {
            let mut decoder = kiss::Decoder::new(MAX_KISS_FRAME);
            let mut buf = [0u8; 2048];
            let mut decoded = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                let n = match reader.read(&mut buf) {
                    Ok(0) => return, // dropping `tx` tells the driver
                    Ok(n) => n,
                    Err(e) if timeouts && e.kind() == io::ErrorKind::TimedOut => continue,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                };
                decoder.push(&buf[..n], &mut decoded);
                for f in decoded.drain(..) {
                    if f.is_data() && f.port == tnc_port {
                        if let Some(hm) = ax25::unwrap(&f.data) {
                            if tx.send(hm.to_vec()).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        })?;
        Ok(KissLink {
            writer,
            frames,
            me,
            tnc_port,
            stop,
            close,
        })
    }
}

impl Drop for KissLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(close) = self.close.take() {
            close();
        }
    }
}

impl Link for KissLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        let ui = ax25::wrap(self.me, frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e:?}")))?;
        self.writer.write_all(&kiss::data_frame(self.tnc_port, &ui))?;
        self.writer.flush()
    }

    fn recv_timeout(&mut self, wait: Duration) -> io::Result<Option<Vec<u8>>> {
        match self.frames.recv_timeout(wait) {
            Ok(f) => Ok(Some(f)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "KISS TNC closed the connection",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse() {
        let serial = |d: &str, b| KissTarget::Serial {
            device: d.into(),
            baud: b,
        };
        assert_eq!(
            KissTarget::parse("127.0.0.1:8001"),
            Ok(KissTarget::Tcp("127.0.0.1:8001".into()))
        );
        assert_eq!(
            KissTarget::parse("tcp:tnc.local:8001"),
            Ok(KissTarget::Tcp("tnc.local:8001".into()))
        );
        assert_eq!(
            KissTarget::parse("/dev/ttyUSB0"),
            Ok(serial("/dev/ttyUSB0", 9600))
        );
        assert_eq!(KissTarget::parse("COM3"), Ok(serial("COM3", 9600)));
        assert_eq!(
            KissTarget::parse("serial:/dev/ttyACM0:57600"),
            Ok(serial("/dev/ttyACM0", 57600))
        );
        assert_eq!(KissTarget::parse("serial:COM12"), Ok(serial("COM12", 9600)));
        assert!(KissTarget::parse("serial:").is_err());
        assert!(KissTarget::parse("serial:/dev/ttyUSB0:0").is_err());
        assert!(KissTarget::parse("localhost").is_err());
    }

    #[test]
    fn params_become_kiss_commands_in_10_ms_units() {
        let p = TncParams {
            txdelay_ms: 305,
            persist: 63,
            slot_ms: 100,
        };
        let mut d = kiss::Decoder::new(64);
        let mut got = Vec::new();
        d.push(&p.frames(1), &mut got);
        let got: Vec<(u8, u8, Vec<u8>)> = got.into_iter().map(|f| (f.port, f.command, f.data)).collect();
        assert_eq!(got, vec![(1, 1, vec![31]), (1, 2, vec![63]), (1, 3, vec![10])]);
        // Longer than KISS can say: the most it can.
        let long = TncParams {
            txdelay_ms: 5000,
            ..p
        };
        assert_eq!(long.frames(0)[2], 255);
    }
}
