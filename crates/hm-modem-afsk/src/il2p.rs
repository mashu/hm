//! IL2P framing (the Improved Layer 2 Protocol of NinoTNC and Direwolf): a
//! 24-bit sync word, a Reed–Solomon protected header and payload blocks, no
//! bit stuffing and no NRZI. A frame survives errors that would have lost an
//! HDLC frame: the header corrects one bad byte, and each payload block half
//! as many bytes as it carries parity (8 with maximum FEC).
//!
//! Frames go in and come out as AX.25 frames (addresses, control, PID,
//! information; no frame check), the same as the HDLC path. A UI frame
//! between two stations with no digipeaters uses the compact type 1 header,
//! which carries the addresses; anything else travels whole in the payload
//! (type 0). On air, bytes go most significant bit first and a 1 bit is the
//! mark tone.

use alloc::vec;
use alloc::vec::Vec;

use crate::rs;

/// The sync word that starts every frame.
pub const SYNC_WORD: u32 = 0xF1_5E_48;
/// Preamble byte, sent while the transmitter keys up and before each frame.
pub const PREAMBLE: u8 = 0x55;
const HEADER_LEN: usize = 13;
const HEADER_PARITY: usize = 2;
/// Largest payload a frame carries.
pub const MAX_PAYLOAD: usize = 1023;

// ---- scrambling -------------------------------------------------------------

/// Undo IL2P scrambling (x^9 + x^4 + 1, restarted for each block).
fn descramble(block: &[u8]) -> Vec<u8> {
    let mut state: u32 = 0x1F0;
    block
        .iter()
        .map(|&byte| {
            let mut out = 0u8;
            for k in (0..8).rev() {
                let c = ((byte >> k) & 1) as u32;
                out |= (((c ^ state) & 1) as u8) << k;
                state = ((state >> 1) | (c << 8)) ^ (c << 3);
            }
            out
        })
        .collect()
}

/// Scramble a block: the exact inverse of [`descramble`].
fn scramble(block: &[u8]) -> Vec<u8> {
    let mut state: u32 = 0x1F0;
    block
        .iter()
        .map(|&byte| {
            let mut out = 0u8;
            for k in (0..8).rev() {
                let d = ((byte >> k) & 1) as u32;
                let c = d ^ (state & 1);
                out |= (c as u8) << k;
                state = ((state >> 1) | (c << 8)) ^ (c << 3);
            }
            out
        })
        .collect()
}

// ---- payload blocks ---------------------------------------------------------

/// How a payload is cut into Reed–Solomon blocks: large blocks (one byte
/// longer) first, then small ones, each followed by its parity.
struct Blocks {
    large: usize,
    large_len: usize,
    small: usize,
    small_len: usize,
    parity: usize,
}

impl Blocks {
    fn new(len: usize, max_fec: bool) -> Option<Blocks> {
        if len == 0 || len > MAX_PAYLOAD {
            return None;
        }
        let per = if max_fec { 239 } else { 247 };
        let count = len.div_ceil(per);
        let small_len = len / count;
        let large = len - count * small_len;
        let parity = if max_fec {
            16
        } else {
            match small_len {
                0..=61 => 2,
                62..=123 => 4,
                124..=185 => 6,
                _ => 8,
            }
        };
        Some(Blocks {
            large,
            large_len: small_len + 1,
            small: count - large,
            small_len,
            parity,
        })
    }

    fn sizes(&self) -> impl Iterator<Item = usize> + '_ {
        core::iter::repeat_n(self.large_len, self.large)
            .chain(core::iter::repeat_n(self.small_len, self.small))
    }

    /// Bytes on air for the whole payload.
    fn encoded_len(&self) -> usize {
        self.sizes().map(|n| n + self.parity).sum()
    }
}

fn encode_payload(payload: &[u8], max_fec: bool) -> Vec<u8> {
    let Some(b) = Blocks::new(payload.len(), max_fec) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(b.encoded_len());
    let mut at = 0;
    for n in b.sizes() {
        let s = scramble(&payload[at..at + n]);
        out.extend_from_slice(&s);
        out.extend(rs::encode(&s, b.parity));
        at += n;
    }
    out
}

fn decode_payload(encoded: &[u8], len: usize, max_fec: bool) -> Option<Vec<u8>> {
    let b = Blocks::new(len, max_fec)?;
    let mut out = Vec::with_capacity(len);
    let mut at = 0;
    for n in b.sizes() {
        let mut block = encoded.get(at..at + n + b.parity)?.to_vec();
        rs::decode(&mut block, b.parity)?;
        out.extend(descramble(&block[..n]));
        at += n + b.parity;
    }
    Some(out)
}

// ---- header -----------------------------------------------------------------

/// Header fields spread over bit 6 or bit 7 of header bytes, most significant
/// bit in the lowest byte.
fn set_field(hdr: &mut [u8; HEADER_LEN], bit: u8, first: usize, width: usize, value: u32) {
    for i in 0..width {
        if (value >> (width - 1 - i)) & 1 != 0 {
            hdr[first + i] |= 1 << bit;
        }
    }
}

fn get_field(hdr: &[u8; HEADER_LEN], bit: u8, first: usize, width: usize) -> u32 {
    (0..width).fold(0, |v, i| (v << 1) | ((hdr[first + i] >> bit) & 1) as u32)
}

/// AX.25 PIDs a type 1 header can carry, by their 4-bit IL2P code.
const PIDS: [(u8, u8); 10] = [
    (0x2, 0x20),
    (0x3, 0x01),
    (0x4, 0x06),
    (0x5, 0x07),
    (0x6, 0x08),
    (0xB, 0xCC),
    (0xC, 0xCD),
    (0xD, 0xCE),
    (0xE, 0xCF),
    (0xF, 0xF0),
];

fn pid_code(pid: u8) -> Option<u8> {
    if pid & 0x30 == 0x10 || pid & 0x30 == 0x20 {
        return Some(0x2); // AX.25 layer 3
    }
    PIDS.iter().find(|(_, p)| *p == pid).map(|(c, _)| *c)
}

/// A UI frame with exactly two addresses, as a type 1 header and its
/// payload; `None` unless decoding the header gives back exactly these bytes
/// (anything else goes whole, as type 0).
fn type1(frame: &[u8], max_fec: bool) -> Option<([u8; HEADER_LEN], &[u8])> {
    if frame.len() < 16 || frame[6] & 1 != 0 || frame[13] & 1 == 0 {
        return None; // not exactly two addresses
    }
    // Reserved address bits set, opposite C bits, clean callsign bytes.
    let (d, s) = (frame[6], frame[13]);
    if d & 0x60 != 0x60 || s & 0x60 != 0x60 || (d ^ s) & 0x80 == 0 {
        return None;
    }
    if frame[..6].iter().chain(&frame[7..13]).any(|b| b & 1 != 0) {
        return None;
    }
    let control = frame[14];
    if control & !0x10 != 0x03 {
        return None; // not UI
    }
    // Only PIDs that come back unchanged (the layer 3 group all decode as 0x20).
    let pid = pid_code(frame[15]).filter(|c| PIDS.contains(&(*c, frame[15])))?;
    let info = &frame[16..];
    if info.len() > MAX_PAYLOAD {
        return None;
    }
    let mut hdr = [0u8; HEADER_LEN];
    for (i, &b) in frame[..6].iter().chain(&frame[7..13]).enumerate() {
        let c = b >> 1;
        if !(0x20..=0x5F).contains(&c) {
            return None; // not DEC SIXBIT
        }
        hdr[i] = c - 0x20;
    }
    hdr[12] = (((frame[6] >> 1) & 0x0F) << 4) | ((frame[13] >> 1) & 0x0F);
    let pf = ((control >> 4) & 1) as u32;
    let command = (frame[6] >> 7) as u32;
    set_field(&mut hdr, 6, 0, 1, 1); // UI
    set_field(&mut hdr, 6, 1, 4, pid as u32);
    set_field(&mut hdr, 6, 5, 7, (pf << 6) | (5 << 3) | (command << 2));
    set_field(&mut hdr, 7, 0, 1, max_fec as u32);
    set_field(&mut hdr, 7, 1, 1, 1); // header type 1
    set_field(&mut hdr, 7, 2, 10, info.len() as u32);
    Some((hdr, info))
}

/// Rebuild the AX.25 UI frame a type 1 header describes, with `info`.
fn frame_from_type1(hdr: &[u8; HEADER_LEN], info: &[u8]) -> Option<Vec<u8>> {
    let control = get_field(hdr, 6, 5, 7);
    if get_field(hdr, 6, 0, 1) != 1 || (control >> 3) & 7 != 5 {
        return None; // only UI frames are carried here
    }
    let code = get_field(hdr, 6, 1, 4) as u8;
    let pid = PIDS.iter().find(|(c, _)| *c == code).map_or(0xF0, |(_, p)| *p);
    let command = (control >> 2) & 1 == 1;
    let pf = ((control >> 6) & 1) as u8;
    let mut f = Vec::with_capacity(16 + info.len());
    for (i, ssid) in [(0usize, hdr[12] >> 4), (6, hdr[12] & 0x0F)] {
        f.extend(hdr[i..i + 6].iter().map(|&c| ((c & 0x3F) + 0x20) << 1));
        let c_bit = (i == 0) == command;
        f.push(0x60 | (ssid << 1) | if c_bit { 0x80 } else { 0 } | (i == 6) as u8);
    }
    f.push(0x03 | (pf << 4));
    f.push(pid);
    f.extend_from_slice(info);
    Some(f)
}

// ---- frames -----------------------------------------------------------------

/// A frame as bytes on air after the preamble: sync word, header, payload.
/// `None` for frames longer than IL2P carries.
pub fn encode(frame: &[u8], max_fec: bool) -> Option<Vec<u8>> {
    let (hdr, payload) = match type1(frame, max_fec) {
        Some(h) => h,
        None => {
            if frame.is_empty() || frame.len() > MAX_PAYLOAD {
                return None;
            }
            let mut hdr = [0u8; HEADER_LEN];
            set_field(&mut hdr, 7, 0, 1, max_fec as u32);
            set_field(&mut hdr, 7, 2, 10, frame.len() as u32);
            (hdr, frame)
        }
    };
    let mut out = SYNC_WORD.to_be_bytes()[1..].to_vec();
    let h = scramble(&hdr);
    out.extend_from_slice(&h);
    out.extend(rs::encode(&h, HEADER_PARITY));
    out.extend(encode_payload(payload, max_fec));
    Some(out)
}

/// A received header, corrected and descrambled; with the payload length on air.
fn read_header(raw: &[u8]) -> Option<([u8; HEADER_LEN], usize)> {
    let mut block = raw.to_vec();
    rs::decode(&mut block, HEADER_PARITY)?;
    let hdr: [u8; HEADER_LEN] = descramble(&block[..HEADER_LEN]).try_into().ok()?;
    let len = get_field(&hdr, 7, 2, 10) as usize;
    let max_fec = get_field(&hdr, 7, 0, 1) == 1;
    let on_air = if len == 0 {
        0
    } else {
        Blocks::new(len, max_fec)?.encoded_len()
    };
    Some((hdr, on_air))
}

fn finish(hdr: &[u8; HEADER_LEN], payload: &[u8]) -> Option<Vec<u8>> {
    let len = get_field(hdr, 7, 2, 10) as usize;
    let max_fec = get_field(hdr, 7, 0, 1) == 1;
    let info = if len == 0 {
        Vec::new()
    } else {
        decode_payload(payload, len, max_fec)?
    };
    if get_field(hdr, 7, 1, 1) == 1 {
        frame_from_type1(hdr, &info)
    } else if info.len() >= 15 {
        Some(info) // type 0: the whole AX.25 frame
    } else {
        None
    }
}

#[derive(Clone, Debug)]
enum State {
    Searching,
    Header,
    Payload { hdr: [u8; HEADER_LEN], len: usize },
}

/// Bit-level IL2P receiver: raw bits in (1 = mark), AX.25 frames out. Either
/// polarity is accepted, and a sync word with one wrong bit.
#[derive(Clone, Debug)]
pub struct Receiver {
    acc: u32,
    state: State,
    inverted: bool,
    nbits: u8,
    buf: Vec<u8>,
}

impl Default for Receiver {
    fn default() -> Receiver {
        Receiver::new()
    }
}

impl Receiver {
    pub fn new() -> Receiver {
        Receiver {
            acc: 0,
            state: State::Searching,
            inverted: false,
            nbits: 0,
            buf: Vec::new(),
        }
    }

    /// Whether a frame is being collected (for carrier sensing).
    pub fn busy(&self) -> bool {
        !matches!(self.state, State::Searching)
    }

    pub fn push(&mut self, bit: u8) -> Option<Vec<u8>> {
        self.acc = ((self.acc << 1) | (bit & 1) as u32) & 0x00FF_FFFF;
        if let State::Searching = self.state {
            let normal = (self.acc ^ SYNC_WORD).count_ones() <= 1;
            let inverted = (!self.acc & 0x00FF_FFFF ^ SYNC_WORD).count_ones() <= 1;
            if normal || inverted {
                self.inverted = !normal;
                self.state = State::Header;
                self.nbits = 0;
                self.buf.clear();
            }
            return None;
        }
        self.nbits += 1;
        if self.nbits < 8 {
            return None;
        }
        self.nbits = 0;
        let byte = self.acc as u8;
        self.buf.push(if self.inverted { !byte } else { byte });
        match self.state.clone() {
            State::Searching => None,
            State::Header if self.buf.len() == HEADER_LEN + HEADER_PARITY => match read_header(&self.buf) {
                Some((hdr, 0)) => {
                    self.state = State::Searching;
                    finish(&hdr, &[])
                }
                Some((hdr, len)) => {
                    self.state = State::Payload { hdr, len };
                    self.buf.clear();
                    None
                }
                None => {
                    self.state = State::Searching;
                    None
                }
            },
            State::Header => None,
            State::Payload { hdr, len } if self.buf.len() == len => {
                self.state = State::Searching;
                let payload = core::mem::take(&mut self.buf);
                finish(&hdr, &payload)
            }
            State::Payload { .. } => None,
        }
    }
}

/// One transmission as raw bits (most significant bit first): `preamble`
/// preamble bytes, then each frame with its own preamble byte, then a
/// preamble byte to finish.
pub fn encode_bits(frames: &[&[u8]], preamble: usize, max_fec: bool) -> Vec<u8> {
    let mut bytes = vec![PREAMBLE; preamble.max(1)];
    for f in frames {
        if let Some(e) = encode(f, max_fec) {
            bytes.push(PREAMBLE);
            bytes.extend(e);
        }
    }
    bytes.push(PREAMBLE);
    bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |k| (b >> k) & 1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An AX.25 UI frame, SRC>DST with PID `pid`.
    fn ui(dst: &str, dst_ssid: u8, src: &str, src_ssid: u8, pid: u8, info: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        for (call, ssid, last, c) in [(dst, dst_ssid, false, true), (src, src_ssid, true, false)] {
            let mut padded = [b' '; 6];
            padded[..call.len()].copy_from_slice(call.as_bytes());
            f.extend(padded.iter().map(|c| c << 1));
            f.push(0x60 | (ssid << 1) | if c { 0x80 } else { 0 } | last as u8);
        }
        f.extend([0x03, pid]);
        f.extend_from_slice(info);
        f
    }

    fn receive(bits: &[u8]) -> Vec<Vec<u8>> {
        let mut r = Receiver::new();
        bits.iter().filter_map(|&b| r.push(b)).collect()
    }

    #[test]
    fn scrambling_is_undone() {
        let data: Vec<u8> = (0..300).map(|i| (i * 7 + 3) as u8).collect();
        assert_eq!(descramble(&scramble(&data)), data);
        assert_ne!(scramble(&data), data);
    }

    #[test]
    fn block_sizes_follow_the_payload_length() {
        let b = Blocks::new(100, true).unwrap();
        assert_eq!((b.large, b.small, b.small_len, b.parity), (0, 1, 100, 16));
        let b = Blocks::new(600, true).unwrap(); // 3 blocks: 200 each
        assert_eq!((b.large, b.small, b.small_len), (0, 3, 200));
        let b = Blocks::new(601, false).unwrap(); // 3 blocks of 247 max: 201, 200, 200
        assert_eq!(
            (b.large, b.large_len, b.small, b.small_len, b.parity),
            (1, 201, 2, 200, 8)
        );
        assert!(Blocks::new(1024, true).is_none());
    }

    #[test]
    fn type_1_and_type_0_frames_round_trip() {
        let hm = ui("HMNET", 0, "SA0KAM", 7, 0xF0, b"an hm frame");
        let odd_pid = ui("HMNET", 0, "SA0KAM", 0, 0x77, b"type 0");
        // A third address (a digipeater): only type 0 can carry it.
        let mut digi = ui("APRS", 0, "SO5KM", 1, 0xF0, b"via a digipeater");
        digi[13] &= !1;
        let wide: Vec<u8> = b"WIDE1 "
            .iter()
            .map(|c| c << 1)
            .chain([0x60 | (1 << 1) | 1])
            .collect();
        digi.splice(14..14, wide);
        for max_fec in [true, false] {
            let frames: [&[u8]; 3] = [&hm, &odd_pid, &digi];
            let got = receive(&encode_bits(&frames, 4, max_fec));
            assert_eq!(got, frames.iter().map(|f| f.to_vec()).collect::<Vec<_>>());
        }
        // The hm frame used the compact header: 15 bytes of header, then only the information.
        let e = encode(&hm, true).unwrap();
        assert_eq!(e.len(), 3 + 15 + 11 + 16);
    }

    #[test]
    fn errors_are_corrected_and_inverted_polarity_is_fine() {
        let f = ui("HMNET", 0, "SO5KM", 1, 0xF0, &[0xA5; 200]);
        let mut bits = encode_bits(&[&f], 2, true);
        // One bad bit in the sync word, a bad byte in the header, and eight
        // bad bytes in the payload block: all repaired.
        let sync = 8 * 3; // after two preamble bytes and the frame's own
        let payload = sync + 24 + 8 * 15;
        bits[sync + 5] ^= 1;
        for k in 0..8 {
            bits[sync + 24 + 8 * 3 + k] ^= 1;
        }
        for j in 0..8 {
            bits[payload + 8 * (20 * j + 1)] ^= 1;
        }
        assert_eq!(receive(&bits), vec![f.clone()]);
        let inverted: Vec<u8> = bits.iter().map(|b| b ^ 1).collect();
        assert_eq!(receive(&inverted), vec![f.clone()]);
        // Nine bad bytes in one block is too many: nothing comes out.
        bits[payload + 8 * 161] ^= 1;
        assert!(receive(&bits).is_empty());
    }

    #[test]
    fn any_frame_comes_back_byte_for_byte() {
        let mut x = 0x0123_4567_89AB_CDEFu64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for i in 0..3000 {
            let len = 16 + (next() % 300) as usize;
            let mut f: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if i % 2 == 0 {
                // Close to a UI frame, so both header types get exercised.
                let base = ui("HMNET", 0, "SA0KAM", (i % 16) as u8, 0xF0, &f[16..]);
                f = base;
                let k = (next() % 16) as usize;
                if i % 4 == 0 {
                    f[k] ^= 1 << (next() % 8); // one stray bit anywhere in the header
                }
            }
            let got = receive(&encode_bits(&[&f], 1, i % 3 != 0));
            assert_eq!(got, vec![f], "frame {i}");
        }
    }

    #[test]
    fn noise_never_panics() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut r = Receiver::new();
        for _ in 0..2_000_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let _ = r.push((x & 1) as u8);
        }
    }
}
