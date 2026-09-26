//! KISS framing (K9NG/KA9Q), as used over TCP (Direwolf port 8001) and serial.
//!
//! A frame is `FEND type data FEND`; FEND and FESC inside are escaped. The
//! type byte holds the TNC port in its high nibble and the command in its low
//! nibble.

use alloc::vec::Vec;

pub const FEND: u8 = 0xC0;
pub const FESC: u8 = 0xDB;
pub const TFEND: u8 = 0xDC;
pub const TFESC: u8 = 0xDD;

/// Low-nibble command codes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Command {
    Data = 0,
    /// Key-up delay, in 10 ms units.
    TxDelay = 1,
    /// p-persistence, `p = (value + 1) / 256`.
    Persistence = 2,
    /// CSMA slot time, in 10 ms units.
    SlotTime = 3,
    /// Obsolete key-down tail, in 10 ms units.
    TxTail = 4,
    FullDuplex = 5,
    SetHardware = 6,
}

/// One decoded KISS frame.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KissFrame {
    pub port: u8,
    /// Low nibble of the type byte; 0 = data.
    pub command: u8,
    pub data: Vec<u8>,
}

impl KissFrame {
    pub fn is_data(&self) -> bool {
        self.command == Command::Data as u8
    }
}

fn push_escaped(out: &mut Vec<u8>, b: u8) {
    match b {
        FEND => out.extend_from_slice(&[FESC, TFEND]),
        FESC => out.extend_from_slice(&[FESC, TFESC]),
        _ => out.push(b),
    }
}

/// Append a complete KISS frame to `out`.
pub fn encode(port: u8, command: u8, data: &[u8], out: &mut Vec<u8>) {
    out.reserve(data.len() + 4);
    out.push(FEND);
    push_escaped(out, ((port & 0x0F) << 4) | (command & 0x0F));
    for &b in data {
        push_escaped(out, b);
    }
    out.push(FEND);
}

/// A data frame for TNC port `port`.
pub fn data_frame(port: u8, frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode(port, Command::Data as u8, frame, &mut out);
    out
}

/// A one-byte parameter command (TXDELAY, persistence, slot time, ...).
pub fn param_frame(port: u8, command: Command, value: u8) -> Vec<u8> {
    let mut out = Vec::new();
    encode(port, command as u8, &[value], &mut out);
    out
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    /// Before the first FEND: bytes are line noise and ignored.
    Hunt,
    InFrame,
    Escape,
}

/// Streaming decoder: feed any chunks of the byte stream, get whole frames.
///
/// Frames with an invalid escape or longer than the limit are dropped whole.
#[derive(Clone, Debug)]
pub struct Decoder {
    buf: Vec<u8>,
    state: State,
    dropping: bool,
    max_len: usize,
    dropped: u64,
}

impl Decoder {
    /// `max_len` bounds a frame's length after unescaping, type byte included.
    pub fn new(max_len: usize) -> Decoder {
        Decoder {
            buf: Vec::new(),
            state: State::Hunt,
            dropping: false,
            max_len: max_len.max(1),
            dropped: 0,
        }
    }

    /// Frames discarded so far because of bad escapes or excess length.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn push(&mut self, input: &[u8], out: &mut Vec<KissFrame>) {
        for &b in input {
            match (self.state, b) {
                (State::Hunt, FEND) => self.start(),
                (State::Hunt, _) => {}
                (State::InFrame, FEND) => {
                    self.finish(out);
                    self.start();
                }
                (State::InFrame, FESC) => self.state = State::Escape,
                (State::InFrame, _) => self.byte(b),
                (State::Escape, TFEND) => {
                    self.byte(FEND);
                    self.state = State::InFrame;
                }
                (State::Escape, TFESC) => {
                    self.byte(FESC);
                    self.state = State::InFrame;
                }
                (State::Escape, FEND) => {
                    // Broken escape at the end of a frame: drop it, resynchronise.
                    self.dropping = true;
                    self.finish(out);
                    self.start();
                }
                (State::Escape, _) => {
                    self.dropping = true;
                    self.state = State::InFrame;
                }
            }
        }
    }

    fn start(&mut self) {
        self.buf.clear();
        self.dropping = false;
        self.state = State::InFrame;
    }

    fn byte(&mut self, b: u8) {
        if self.dropping {
            return;
        }
        if self.buf.len() >= self.max_len {
            self.dropping = true;
            return;
        }
        self.buf.push(b);
    }

    fn finish(&mut self, out: &mut Vec<KissFrame>) {
        if self.dropping {
            self.dropped += 1;
        } else if let Some((&t, data)) = self.buf.split_first() {
            out.push(KissFrame {
                port: t >> 4,
                command: t & 0x0F,
                data: data.to_vec(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use proptest::prelude::*;

    #[test]
    fn escapes_on_the_wire() {
        let f = data_frame(0, &[0x01, FEND, 0x02, FESC, 0x03]);
        assert_eq!(
            f,
            vec![FEND, 0x00, 0x01, FESC, TFEND, 0x02, FESC, TFESC, 0x03, FEND]
        );
        // Port 12 with command 0 makes the type byte 0xC0, which must be escaped too.
        assert_eq!(data_frame(12, &[]), vec![FEND, FESC, TFEND, FEND]);
        assert_eq!(param_frame(1, Command::TxDelay, 30), vec![FEND, 0x11, 30, FEND]);
    }

    #[test]
    fn decodes_split_across_reads_and_skips_noise() {
        let mut wire = vec![0x55, 0xAA]; // noise before the first FEND
        wire.extend(data_frame(0, b"hello"));
        wire.extend(data_frame(2, &[FEND, FESC]));
        let mut d = Decoder::new(1024);
        let mut out = Vec::new();
        for chunk in wire.chunks(3) {
            d.push(chunk, &mut out);
        }
        assert_eq!(
            out,
            vec![
                KissFrame {
                    port: 0,
                    command: 0,
                    data: b"hello".to_vec()
                },
                KissFrame {
                    port: 2,
                    command: 0,
                    data: vec![FEND, FESC]
                },
            ]
        );
    }

    #[test]
    fn back_to_back_fends_and_empty_frames_are_ignored() {
        let mut d = Decoder::new(64);
        let mut out = Vec::new();
        d.push(&[FEND, FEND, FEND, 0x00, 0x41, FEND, FEND], &mut out);
        assert_eq!(
            out,
            vec![KissFrame {
                port: 0,
                command: 0,
                data: vec![0x41]
            }]
        );
    }

    #[test]
    fn bad_escapes_and_oversize_frames_are_dropped_whole() {
        let mut d = Decoder::new(4);
        let mut out = Vec::new();
        d.push(&[FEND, 0x00, 0x41, FESC, 0x42, 0x43, FEND], &mut out);
        d.push(&[FEND, 0x00, 1, 2, 3, 4, 5, FEND], &mut out);
        d.push(&[FEND, 0x00, 0x41, FESC, FEND], &mut out);
        d.push(&data_frame(0, b"ok"), &mut out);
        assert_eq!(
            out,
            vec![KissFrame {
                port: 0,
                command: 0,
                data: b"ok".to_vec()
            }]
        );
        assert_eq!(d.dropped(), 3);
    }

    proptest! {
        #[test]
        fn roundtrip_any_frames_any_chunking(
            frames in proptest::collection::vec((0u8..16, proptest::collection::vec(any::<u8>(), 0..300)), 0..8),
            chunk in 1usize..50,
        ) {
            let mut wire = Vec::new();
            for (port, data) in &frames {
                encode(*port, 0, data, &mut wire);
            }
            let mut d = Decoder::new(1024);
            let mut out = Vec::new();
            for c in wire.chunks(chunk) {
                d.push(c, &mut out);
            }
            let expected: Vec<KissFrame> =
                frames.iter().map(|(p, data)| KissFrame { port: *p, command: 0, data: data.clone() }).collect();
            prop_assert_eq!(out, expected);
        }

        #[test]
        fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..500)) {
            let mut d = Decoder::new(64);
            let mut out = Vec::new();
            d.push(&bytes, &mut out);
            for f in out {
                prop_assert!(f.data.len() < 64);
            }
        }
    }
}
