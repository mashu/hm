//! A KISS-over-TCP link (Direwolf listens on port 8001 by default).
//!
//! Outgoing hm frames are wrapped in AX.25 UI frames from the station's
//! callsign to `HMNET`; incoming KISS data frames on our TNC port are unwrapped
//! and anything that is not an hm frame (APRS, other traffic) is ignored.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use hm_bearer::{ax25, kiss};
use hm_wire::Callsign;

use crate::driver::Link;

/// Largest KISS frame accepted from the TNC.
const MAX_KISS_FRAME: usize = 4096;

pub struct KissLink {
    writer: TcpStream,
    frames: Receiver<Vec<u8>>,
    me: Callsign,
    tnc_port: u8,
}

impl KissLink {
    /// Connect to a KISS TCP server; `me` is the callsign transmitted as the AX.25 source.
    pub fn connect(addr: &str, me: Callsign, tnc_port: u8) -> io::Result<KissLink> {
        ax25::Address::from_callsign(me).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{me} cannot be an AX.25 source (at most 6 letters or digits, SSID 0-15)"),
            )
        })?;
        let writer = TcpStream::connect(addr)?;
        writer.set_nodelay(true)?;
        let mut reader = writer.try_clone()?;
        let (tx, frames) = mpsc::channel();
        thread::Builder::new().name("kiss-reader".into()).spawn(move || {
            let mut decoder = kiss::Decoder::new(MAX_KISS_FRAME);
            let mut buf = [0u8; 2048];
            let mut decoded = Vec::new();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return, // dropping `tx` tells the driver
                    Ok(n) => n,
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
        })
    }
}

impl Link for KissLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        let ui = ax25::wrap(self.me, frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e:?}")))?;
        self.writer.write_all(&kiss::data_frame(self.tnc_port, &ui))
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
