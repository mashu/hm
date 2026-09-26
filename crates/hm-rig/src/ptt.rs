//! Keying the transmitter.
//!
//! Specs as given on the command line:
//!
//! | Spec | Method |
//! | --- | --- |
//! | `vox` | none: the radio keys on audio |
//! | `rigctld` or `rigctld:HOST:PORT` | Hamlib's rigctld over TCP (default 127.0.0.1:4532): CAT control for most radios |
//! | `rts:DEVICE`, `dtr:DEVICE` | a serial control line, e.g. `rts:/dev/ttyUSB0` or `dtr:COM3` |
//! | `cm108:HIDRAW[:GPIO]` | CM108/CM119 sound chip GPIO (AIOC, Digirig and similar), GPIO 3 by default; Linux |

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

pub trait Ptt: Send {
    fn set(&mut self, on: bool) -> io::Result<()>;
    /// For logs and status.
    fn describe(&self) -> String;
}

pub struct Vox;

impl Ptt for Vox {
    fn set(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }
    fn describe(&self) -> String {
        "VOX".into()
    }
}

/// Hamlib's rigctld: `T 1` keys, `T 0` unkeys; it answers `RPRT 0` on success.
pub struct Rigctld {
    addr: String,
    stream: BufReader<TcpStream>,
}

impl Rigctld {
    pub fn connect(addr: &str) -> io::Result<Rigctld> {
        let s = TcpStream::connect(addr)?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.set_nodelay(true)?;
        Ok(Rigctld {
            addr: addr.to_string(),
            stream: BufReader::new(s),
        })
    }
}

impl Ptt for Rigctld {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.stream
            .get_mut()
            .write_all(if on { b"T 1\n" } else { b"T 0\n" })?;
        let mut line = String::new();
        self.stream.read_line(&mut line)?;
        if line.trim() == "RPRT 0" {
            Ok(())
        } else {
            Err(io::Error::other(format!("rigctld refused PTT: {}", line.trim())))
        }
    }
    fn describe(&self) -> String {
        format!("rigctld {}", self.addr)
    }
}

/// CM108-family GPIO through the Linux hidraw interface.
pub struct Cm108 {
    path: String,
    gpio: u8,
    file: std::fs::File,
}

impl Cm108 {
    pub fn open(path: &str, gpio: u8) -> io::Result<Cm108> {
        if !(1..=8).contains(&gpio) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CM108 GPIO must be 1-8",
            ));
        }
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        Ok(Cm108 {
            path: path.to_string(),
            gpio,
            file,
        })
    }

    /// The HID output report that sets the GPIO: report 0, then direction mask
    /// and data mask for the selected pin.
    pub fn report(gpio: u8, on: bool) -> [u8; 5] {
        let mask = 1u8 << (gpio - 1);
        [0, 0, mask, if on { mask } else { 0 }, 0]
    }
}

impl Ptt for Cm108 {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.file.write_all(&Cm108::report(self.gpio, on))
    }
    fn describe(&self) -> String {
        format!("CM108 {} GPIO {}", self.path, self.gpio)
    }
}

#[cfg(feature = "serial")]
pub struct SerialLine {
    port: Box<dyn serialport::SerialPort>,
    rts: bool,
    name: String,
}

#[cfg(feature = "serial")]
impl SerialLine {
    pub fn open(path: &str, rts: bool) -> io::Result<SerialLine> {
        let mut port = serialport::new(path, 9600)
            .open()
            .map_err(|e| io::Error::other(e.to_string()))?;
        // Start unkeyed on both lines.
        let _ = port.write_request_to_send(false);
        let _ = port.write_data_terminal_ready(false);
        Ok(SerialLine {
            port,
            rts,
            name: path.to_string(),
        })
    }
}

#[cfg(feature = "serial")]
impl Ptt for SerialLine {
    fn set(&mut self, on: bool) -> io::Result<()> {
        let r = if self.rts {
            self.port.write_request_to_send(on)
        } else {
            self.port.write_data_terminal_ready(on)
        };
        r.map_err(|e| io::Error::other(e.to_string()))
    }
    fn describe(&self) -> String {
        format!("{} on {}", if self.rts { "RTS" } else { "DTR" }, self.name)
    }
}

/// Open a PTT from its spec (see the module table).
pub fn open(spec: &str) -> io::Result<Box<dyn Ptt>> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown PTT {spec:?}; see `hm node --help`"),
        )
    };
    let (kind, rest) = spec.split_once(':').unwrap_or((spec, ""));
    Ok(match kind {
        "vox" => Box::new(Vox),
        "rigctld" => Box::new(Rigctld::connect(if rest.is_empty() {
            "127.0.0.1:4532"
        } else {
            rest
        })?),
        "cm108" => {
            let (path, gpio) = match rest.rsplit_once(':') {
                Some((p, g)) if g.parse::<u8>().is_ok() => (p, g.parse().expect("checked")),
                _ => (rest, 3),
            };
            Box::new(Cm108::open(path, gpio)?)
        }
        #[cfg(feature = "serial")]
        "rts" | "dtr" if !rest.is_empty() => Box::new(SerialLine::open(rest, kind == "rts")?),
        _ => return Err(bad()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn cm108_report_layout() {
        assert_eq!(Cm108::report(3, true), [0, 0, 0x04, 0x04, 0]);
        assert_eq!(Cm108::report(3, false), [0, 0, 0x04, 0x00, 0]);
        assert_eq!(Cm108::report(1, true), [0, 0, 0x01, 0x01, 0]);
    }

    #[test]
    fn cm108_writes_reports_to_the_device() {
        let path = std::env::temp_dir().join(format!("hm-hidraw-{}", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        let mut p = open(&format!("cm108:{}:3", path.display())).unwrap();
        p.set(true).unwrap();
        p.set(false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [0, 0, 4, 4, 0, 0, 0, 4, 0, 0]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rigctld_protocol() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            for reply in ["RPRT 0\n", "RPRT 0\n", "RPRT -9\n"] {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                tx.send(line.trim().to_string()).unwrap();
                w.write_all(reply.as_bytes()).unwrap();
            }
        });
        let mut p = open(&format!("rigctld:{addr}")).unwrap();
        p.set(true).unwrap();
        p.set(false).unwrap();
        assert!(p.set(true).is_err(), "an error from rigctld is reported");
        assert_eq!(rx.iter().take(3).collect::<Vec<_>>(), ["T 1", "T 0", "T 1"]);
    }

    #[test]
    fn bad_specs_are_refused() {
        assert!(open("bogus").is_err());
        assert!(open("cm108:/nonexistent/hidraw:9").is_err());
        assert_eq!(open("vox").unwrap().describe(), "VOX");
    }
}
