//! HDLC framing at the bit level: flags, bit stuffing, frame check.

use alloc::vec::Vec;

use crate::crc::{crc16_x25, fcs_ok};

pub const FLAG: u8 = 0x7E;
/// Longest frame accepted (AX.25 information field of 2048 bytes plus headers).
pub const MAX_FRAME: usize = 2200;
/// Shortest frame accepted, frame check included.
pub const MIN_FRAME: usize = 4;

fn push_byte_bits(out: &mut Vec<u8>, b: u8) {
    for i in 0..8 {
        out.push((b >> i) & 1);
    }
}

/// One transmission as bits (LSB first): `preamble` flags, the frames each
/// followed by a flag, then `postamble - 1` more flags.
pub fn encode(frames: &[&[u8]], preamble: usize, postamble: usize) -> Vec<u8> {
    let mut bits = Vec::new();
    for _ in 0..preamble.max(1) {
        push_byte_bits(&mut bits, FLAG);
    }
    for f in frames {
        let mut ones = 0;
        let fcs = crc16_x25(f).to_le_bytes();
        for &b in f.iter().chain(fcs.iter()) {
            for i in 0..8 {
                let bit = (b >> i) & 1;
                bits.push(bit);
                if bit == 1 {
                    ones += 1;
                    if ones == 5 {
                        bits.push(0);
                        ones = 0;
                    }
                } else {
                    ones = 0;
                }
            }
        }
        push_byte_bits(&mut bits, FLAG);
    }
    for _ in 1..postamble.max(1) {
        push_byte_bits(&mut bits, FLAG);
    }
    bits
}

/// Bit-level HDLC receiver.
#[derive(Clone, Debug, Default)]
pub struct Deframer {
    pattern: u8,
    ones: u32,
    in_frame: bool,
    bits: Vec<u8>,
}

impl Deframer {
    pub fn new() -> Deframer {
        Deframer::default()
    }

    /// Feed one decoded bit; returns a verified frame (without FCS) when one ends.
    pub fn push(&mut self, bit: u8) -> Option<Vec<u8>> {
        self.pattern = (self.pattern >> 1) | (bit << 7);
        if self.pattern == FLAG {
            let mut result = None;
            if self.in_frame && self.bits.len() >= 7 {
                // The flag's first seven bits were taken as data; drop them.
                self.bits.truncate(self.bits.len() - 7);
                let n = self.bits.len();
                if n.is_multiple_of(8) && n / 8 >= MIN_FRAME {
                    let bytes: Vec<u8> = self
                        .bits
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| c.iter().enumerate().fold(0u8, |a, (i, b)| a | (b << i)))
                        .collect();
                    if fcs_ok(&bytes) {
                        result = Some(bytes[..bytes.len() - 2].to_vec());
                    }
                }
            }
            self.bits.clear();
            self.in_frame = true;
            self.ones = 0;
            return result;
        }
        if bit == 1 {
            self.ones += 1;
            if self.ones >= 7 {
                // Abort, or idle line: wait for the next flag.
                self.in_frame = false;
                self.bits.clear();
                return None;
            }
        } else {
            if self.ones == 5 {
                self.ones = 0;
                return None; // stuffed zero
            }
            self.ones = 0;
        }
        if self.in_frame {
            if self.bits.len() >= MAX_FRAME * 8 {
                self.in_frame = false;
                self.bits.clear();
            } else {
                self.bits.push(bit);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn stuffing_and_roundtrip() {
        let frames: [&[u8]; 3] = [b"\xFF\xFF\xFF\xFF", b"\x7E\x7E\x7E\x7E hdlc", b"\x00\x01\x02\x03"];
        let bits = encode(&frames, 3, 2);
        // No run of six ones except inside flags: check by deframing.
        let mut d = Deframer::new();
        let got: Vec<Vec<u8>> = bits.iter().filter_map(|&b| d.push(b)).collect();
        assert_eq!(got, frames.iter().map(|f| f.to_vec()).collect::<Vec<_>>());
    }

    #[test]
    fn corrupted_frames_are_dropped() {
        let mut bits = encode(&[b"abcdef"], 2, 2);
        bits[40] ^= 1;
        let mut d = Deframer::new();
        assert!(bits.iter().all(|&b| d.push(b).is_none()));
        let _ = vec![0u8];
    }
}
