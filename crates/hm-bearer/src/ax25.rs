//! AX.25 UI encapsulation for KISS TNCs.
//!
//! ```text
//! dest   7 B  "HMNET" SSID 0, command bit set
//! src    7 B  station callsign (≤ 6 characters + SSID 0–15), last-address bit set
//! ctrl   1 B  0x03 (UI)
//! pid    1 B  0xF0 (no layer 3)
//! info   n B  the hm frame
//! ```
//!
//! The AX.25 source address is the legal station identification on this
//! path, so callsigns AX.25 cannot express (more than 6 characters before the
//! SSID, or a `/` suffix) cannot use a KISS TNC.

use alloc::vec::Vec;

use hm_wire::{Callsign, CALLSIGN_MAX_LEN};

/// Destination address used by every hm frame on the KISS path.
pub const HM_DEST: [u8; 6] = *b"HMNET ";
pub const CONTROL_UI: u8 = 0x03;
pub const PID_NO_L3: u8 = 0xF0;
/// Bytes the UI wrapper adds to each frame (two addresses, control, PID).
pub const UI_OVERHEAD: usize = 16;
const MAX_DIGIPEATERS: usize = 8;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Ax25Error {
    /// Callsign longer than 6 characters, containing `/` or `.`, or SSID above 15.
    NotRepresentable,
    TooShort,
    BadAddress,
    NotUi,
    TooManyDigipeaters,
}

/// An AX.25 address: 6 space-padded characters and a 4-bit SSID.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Address {
    pub call: [u8; 6],
    pub ssid: u8,
}

impl Address {
    pub fn from_callsign(c: Callsign) -> Result<Address, Ax25Error> {
        let mut buf = [0u8; CALLSIGN_MAX_LEN];
        let n = c.write_chars(&mut buf);
        let text = &buf[..n];
        let (base, ssid) = match text.iter().position(|&b| b == b'-') {
            Some(i) => (&text[..i], parse_ssid(&text[i + 1..])?),
            None => (text, 0),
        };
        if base.is_empty() || base.len() > 6 || !base.iter().all(|b| b.is_ascii_alphanumeric()) {
            return Err(Ax25Error::NotRepresentable);
        }
        let mut call = [b' '; 6];
        call[..base.len()].copy_from_slice(base);
        Ok(Address { call, ssid })
    }

    fn encode(&self, command_bit: bool, last: bool, out: &mut Vec<u8>) {
        for &c in &self.call {
            out.push(c << 1);
        }
        out.push(((command_bit as u8) << 7) | 0x60 | ((self.ssid & 0x0F) << 1) | last as u8);
    }

    /// Decode 7 address bytes; returns the address and the last-address bit.
    fn decode(b: &[u8]) -> Result<(Address, bool), Ax25Error> {
        let mut call = [0u8; 6];
        let mut seen_space = false;
        for (i, &raw) in b[..6].iter().enumerate() {
            if raw & 1 != 0 {
                return Err(Ax25Error::BadAddress);
            }
            let c = raw >> 1;
            match c {
                b'A'..=b'Z' | b'0'..=b'9' if !seen_space => call[i] = c,
                b' ' if i > 0 => {
                    seen_space = true;
                    call[i] = c;
                }
                _ => return Err(Ax25Error::BadAddress),
            }
        }
        Ok((
            Address {
                call,
                ssid: (b[6] >> 1) & 0x0F,
            },
            b[6] & 1 == 1,
        ))
    }
}

fn parse_ssid(digits: &[u8]) -> Result<u8, Ax25Error> {
    if digits.is_empty() || digits.len() > 2 || !digits.iter().all(u8::is_ascii_digit) {
        return Err(Ax25Error::NotRepresentable);
    }
    let v = digits.iter().fold(0u8, |a, d| a * 10 + (d - b'0'));
    if v > 15 {
        return Err(Ax25Error::NotRepresentable);
    }
    Ok(v)
}

/// Wrap an hm frame in a UI frame from `src` to `HMNET`.
pub fn wrap(src: Callsign, info: &[u8]) -> Result<Vec<u8>, Ax25Error> {
    let src = Address::from_callsign(src)?;
    let mut out = Vec::with_capacity(UI_OVERHEAD + info.len());
    Address {
        call: HM_DEST,
        ssid: 0,
    }
    .encode(true, false, &mut out);
    src.encode(false, true, &mut out);
    out.push(CONTROL_UI);
    out.push(PID_NO_L3);
    out.extend_from_slice(info);
    Ok(out)
}

/// A parsed UI frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiFrame<'a> {
    pub dest: Address,
    pub src: Address,
    pub digipeaters: usize,
    pub pid: u8,
    pub info: &'a [u8],
}

/// Parse any AX.25 UI frame (digipeater path allowed, skipped).
pub fn parse_ui(frame: &[u8]) -> Result<UiFrame<'_>, Ax25Error> {
    if frame.len() < UI_OVERHEAD {
        return Err(Ax25Error::TooShort);
    }
    let (dest, dest_last) = Address::decode(&frame[0..7])?;
    if dest_last {
        return Err(Ax25Error::BadAddress);
    }
    let (src, mut last) = Address::decode(&frame[7..14])?;
    let mut at = 14;
    let mut digipeaters = 0;
    while !last {
        if digipeaters == MAX_DIGIPEATERS {
            return Err(Ax25Error::TooManyDigipeaters);
        }
        if frame.len() < at + 7 + 2 {
            return Err(Ax25Error::TooShort);
        }
        let (_, l) = Address::decode(&frame[at..at + 7])?;
        last = l;
        at += 7;
        digipeaters += 1;
    }
    if frame.len() < at + 2 {
        return Err(Ax25Error::TooShort);
    }
    // UI with or without the poll/final bit.
    if frame[at] & !0x10 != CONTROL_UI {
        return Err(Ax25Error::NotUi);
    }
    Ok(UiFrame {
        dest,
        src,
        digipeaters,
        pid: frame[at + 1],
        info: &frame[at + 2..],
    })
}

/// The hm frame inside a UI frame addressed to `HMNET` with PID 0xF0, if it is one.
pub fn unwrap(frame: &[u8]) -> Option<&[u8]> {
    let ui = parse_ui(frame).ok()?;
    (ui.dest.call == HM_DEST && ui.pid == PID_NO_L3).then_some(ui.info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use proptest::prelude::*;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    #[test]
    fn wire_layout_matches_ax25() {
        let f = wrap(call("SA0KAM"), b"hi").unwrap();
        // "HMNET " shifted left, SSID byte with command bit: 0x80 | 0x60.
        assert_eq!(&f[0..7], &[0x90, 0x9A, 0x9C, 0x8A, 0xA8, 0x40, 0xE0]);
        // "SA0KAM" shifted left, SSID 0, last-address bit set.
        assert_eq!(&f[7..14], &[0xA6, 0x82, 0x60, 0x96, 0x82, 0x9A, 0x61]);
        assert_eq!(&f[14..], &[0x03, 0xF0, b'h', b'i']);
        assert_eq!(f.len(), UI_OVERHEAD + 2);
    }

    #[test]
    fn ssid_and_roundtrip() {
        let f = wrap(call("SO5KM-7"), b"x").unwrap();
        assert_eq!(f[13], 0x60 | (7 << 1) | 1);
        let ui = parse_ui(&f).unwrap();
        assert_eq!(
            ui.src,
            Address {
                call: *b"SO5KM ",
                ssid: 7
            }
        );
        assert_eq!(unwrap(&f), Some(&b"x"[..]));
    }

    #[test]
    fn unrepresentable_callsigns() {
        assert_eq!(wrap(call("SP5ABC/P"), b""), Err(Ax25Error::NotRepresentable));
        assert_eq!(wrap(call("DL1ABCDE"), b""), Err(Ax25Error::NotRepresentable));
        assert_eq!(wrap(call("SA0KAM-16"), b""), Err(Ax25Error::NotRepresentable));
        assert_eq!(wrap(call("SA0KAM-X"), b""), Err(Ax25Error::NotRepresentable));
    }

    #[test]
    fn digipeated_frames_still_unwrap() {
        let mut f = wrap(call("SA0KAM"), b"via").unwrap();
        f[13] &= !1; // source is no longer the last address
        let digi: Vec<u8> = b"WIDE1 "
            .iter()
            .map(|c| c << 1)
            .chain([0x60 | (1 << 1) | 1])
            .collect();
        f.splice(14..14, digi);
        let ui = parse_ui(&f).unwrap();
        assert_eq!((ui.digipeaters, ui.info), (1, &b"via"[..]));
    }

    #[test]
    fn other_traffic_is_not_ours() {
        let mut aprs = wrap(call("SA0KAM"), b"!5920.00N/01756.00E-").unwrap();
        let apdw: Vec<u8> = b"APDW18".iter().map(|c| c << 1).collect();
        aprs[..6].copy_from_slice(&apdw);
        assert!(parse_ui(&aprs).is_ok());
        assert_eq!(unwrap(&aprs), None);
        let mut iframe = wrap(call("SA0KAM"), b"").unwrap();
        iframe[14] = 0x00;
        assert_eq!(parse_ui(&iframe), Err(Ax25Error::NotUi));
    }

    proptest! {
        #[test]
        fn parse_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..120)) {
            let _ = parse_ui(&bytes);
        }

        #[test]
        fn wrap_unwrap_roundtrip(base in "[A-Z0-9]{1,6}", ssid in 0u8..16, info in proptest::collection::vec(any::<u8>(), 0..300)) {
            let c = if ssid == 0 { call(&base) } else { call(&alloc::format!("{base}-{ssid}")) };
            let f = wrap(c, &info).unwrap();
            prop_assert_eq!(unwrap(&f), Some(&info[..]));
        }
    }

    #[test]
    fn roundtrip_through_kiss() {
        let hm = vec![0u8; 222];
        let wire = crate::kiss::data_frame(0, &wrap(call("SA0KAM"), &hm).unwrap());
        let mut d = crate::kiss::Decoder::new(2048);
        let mut out = Vec::new();
        d.push(&wire, &mut out);
        assert_eq!(unwrap(&out[0].data), Some(&hm[..]));
    }
}
