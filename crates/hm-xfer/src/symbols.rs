//! Symbols: their size, and how many an object needs.

use crate::send::Outgoing;
use crate::{Xfer, MAX_SOURCE_SYMBOLS, SYMBOL_ALIGNMENT};
use alloc::vec::Vec;
use hm_wire::MAX_OBJECT_LEN;
use raptorq::ObjectTransmissionInformation;

/// The smallest symbol size, a multiple of 8 and at most `max`, that keeps an
/// object of `len` bytes in as few symbols as `max` would. Every symbol,
/// repair symbols included, is sent whole, so the padding after the object is
/// airtime: a 119-byte chat bundle goes out as one 120-byte symbol, not a
/// 200-byte one.
pub fn fit_symbol(len: u32, max: u16) -> u16 {
    let align = u32::from(SYMBOL_ALIGNMENT);
    let max = u32::from(max.max(SYMBOL_ALIGNMENT)) / align * align;
    if len == 0 {
        return max as u16;
    }
    let k = len.div_ceil(max);
    (len.div_ceil(k).div_ceil(align) * align).min(max) as u16
}

/// Only called with parameters that passed [`Xfer::check_params`].
pub(crate) fn oti(len: u32, symbol_size: u16) -> ObjectTransmissionInformation {
    ObjectTransmissionInformation::new(len as u64, symbol_size, 1, 1, SYMBOL_ALIGNMENT as u8)
}

impl Xfer {
    pub(crate) fn check_params(&self, len: u32, symbol_size: usize) -> Option<(u16, u32)> {
        if len == 0 || len > self.cfg.max_object_len || len > MAX_OBJECT_LEN {
            return None;
        }
        let t = u16::try_from(symbol_size).ok()?;
        if t == 0 || !t.is_multiple_of(SYMBOL_ALIGNMENT) {
            return None;
        }
        let k = len.div_ceil(t as u32);
        (k <= MAX_SOURCE_SYMBOLS).then_some((t, k))
    }

    pub(crate) fn symbol(o: &Outgoing, esi: u32) -> Vec<u8> {
        if esi < o.k {
            o.source[esi as usize].data().to_vec()
        } else {
            o.encoder.repair_packets(esi - o.k, 1).remove(0).split().1
        }
    }
}
