use alloc::vec::Vec;

use crate::{Callsign, WireError};

pub const ROUTE_MAGIC: [u8; 4] = *b"HMR1";
pub const MAX_ROUTE_HOPS: u8 = 16;

const ROUTE_HEADER_LEN: usize = 10;
const CALLSIGN_LEN: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedBundle<'a> {
    pub hop_count: u8,
    pub visited: Vec<Callsign>,
    pub bundle: &'a [u8],
}

impl<'a> RoutedBundle<'a> {
    pub fn forwarded_by(&self, relay: Callsign, bundle_max_hops: u8) -> Result<Vec<u8>, WireError> {
        let limit = bundle_max_hops.min(MAX_ROUTE_HOPS);
        if self.hop_count >= limit || self.visited.contains(&relay) {
            return Err(WireError::OutOfRange);
        }
        let mut visited = self.visited.clone();
        visited.push(relay);
        wrap_routed(self.hop_count + 1, &visited, self.bundle)
    }
}

pub fn wrap_routed(hop_count: u8, visited: &[Callsign], bundle: &[u8]) -> Result<Vec<u8>, WireError> {
    if bundle.is_empty()
        || hop_count > MAX_ROUTE_HOPS
        || usize::from(hop_count) != visited.len()
        || has_duplicates(visited)
    {
        return Err(WireError::OutOfRange);
    }
    let inner_len = u32::try_from(bundle.len()).map_err(|_| WireError::OutOfRange)?;
    let mut out = Vec::with_capacity(ROUTE_HEADER_LEN + visited.len() * CALLSIGN_LEN + bundle.len());
    out.extend_from_slice(&ROUTE_MAGIC);
    out.push(hop_count);
    out.push(hop_count);
    out.extend_from_slice(&inner_len.to_be_bytes());
    for callsign in visited {
        out.extend_from_slice(&callsign.to_bytes());
    }
    out.extend_from_slice(bundle);
    Ok(out)
}

pub fn unwrap_routed(bytes: &[u8]) -> Result<Option<RoutedBundle<'_>>, WireError> {
    if !bytes.starts_with(&ROUTE_MAGIC) {
        return Ok(None);
    }
    if bytes.len() < ROUTE_HEADER_LEN {
        return Err(WireError::TooShort);
    }
    let hop_count = bytes[4];
    let visited_count = bytes[5];
    if hop_count > MAX_ROUTE_HOPS || hop_count != visited_count {
        return Err(WireError::OutOfRange);
    }
    let inner_len =
        usize::try_from(u32::from_be_bytes(copy_array(&bytes[6..10])?)).map_err(|_| WireError::OutOfRange)?;
    let path_len = usize::from(visited_count) * CALLSIGN_LEN;
    let bundle_start = ROUTE_HEADER_LEN
        .checked_add(path_len)
        .ok_or(WireError::OutOfRange)?;
    let expected_len = bundle_start.checked_add(inner_len).ok_or(WireError::OutOfRange)?;
    if inner_len == 0 || bytes.len() != expected_len {
        return Err(WireError::Trailing);
    }
    let visited = bytes[ROUTE_HEADER_LEN..bundle_start]
        .as_chunks::<CALLSIGN_LEN>()
        .0
        .iter()
        .map(|encoded| Callsign::from_bytes(*encoded))
        .collect::<Result<Vec<_>, _>>()?;
    if has_duplicates(&visited) {
        return Err(WireError::OutOfRange);
    }
    Ok(Some(RoutedBundle {
        hop_count,
        visited,
        bundle: &bytes[bundle_start..],
    }))
}

fn has_duplicates(callsigns: &[Callsign]) -> bool {
    callsigns
        .iter()
        .enumerate()
        .any(|(index, callsign)| callsigns[..index].contains(callsign))
}

fn copy_array<const N: usize>(bytes: &[u8]) -> Result<[u8; N], WireError> {
    bytes.try_into().map_err(|_| WireError::TooShort)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callsign(value: &str) -> Callsign {
        value.parse().unwrap()
    }

    #[test]
    fn routed_bundle_roundtrip() {
        let path = [callsign("M0AAA"), callsign("M0BBB")];
        let bytes = wrap_routed(2, &path, b"signed bundle").unwrap();
        let routed = unwrap_routed(&bytes).unwrap().unwrap();
        assert_eq!(routed.hop_count, 2);
        assert_eq!(routed.visited, path);
        assert_eq!(routed.bundle, b"signed bundle");
    }

    #[test]
    fn legacy_bundle_is_not_a_wrapper() {
        assert_eq!(unwrap_routed(b"legacy signed bundle").unwrap(), None);
    }

    #[test]
    fn forwarding_rejects_loops_and_limit() {
        let first = callsign("M0AAA");
        let routed = RoutedBundle {
            hop_count: 1,
            visited: vec![first],
            bundle: b"signed bundle",
        };
        assert_eq!(routed.forwarded_by(first, 8), Err(WireError::OutOfRange));
        assert_eq!(
            routed.forwarded_by(callsign("M0BBB"), 1),
            Err(WireError::OutOfRange)
        );
    }

    #[test]
    fn rejects_duplicate_path() {
        let repeated = callsign("M0AAA");
        assert_eq!(
            wrap_routed(2, &[repeated, repeated], b"signed bundle"),
            Err(WireError::OutOfRange)
        );
    }
}
