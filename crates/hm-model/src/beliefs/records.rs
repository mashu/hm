//! Beliefs saved as records, one per link and custodian and one per
//! calibration, and restored at start.

use alloc::vec::Vec;

use hm_wire::Callsign;

use super::{Beliefs, Subject};
use crate::calibration::{Calibration, Record};
use crate::custodian::CustodianModel;
use crate::link::LinkModel;
use crate::{Bearer, LinkKey};

/// Version byte in front of every encoded record.
const RECORD_VERSION: u8 = 1;
const LINK_RECORD: u8 = 1;
const CUSTODIAN_RECORD: u8 = 2;
const CALIBRATION_RECORD: u8 = 3;

/// A record that could not be restored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreError(pub &'static str);

impl core::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl Beliefs {
    /// Records changed since the last call, to save: `(key, Some(value))` to
    /// write, `(key, None)` to delete.
    pub fn take_changed(&mut self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        for subject in core::mem::take(&mut self.changed) {
            let value = match subject {
                Subject::Link(key) => self.links.get(&key).map(encode_value),
                Subject::Custodian(station) => self.custodians.get(&station).map(encode_value),
                Subject::Calibration(bearer, seen) => Some(encode_value(
                    self.calibration[bearer.index()][usize::from(seen)].record(),
                )),
            };
            if let Some(value) = value {
                out.push((record_key(subject), Some(value)));
            }
        }
        for subject in core::mem::take(&mut self.removed) {
            out.push((record_key(subject), None));
        }
        out
    }

    /// Take back a record saved from [`Beliefs::take_changed`].
    pub fn restore(&mut self, key: &[u8], value: &[u8]) -> Result<(), RestoreError> {
        let (&version, body) = value.split_first().ok_or(RestoreError("empty record"))?;
        if version != RECORD_VERSION {
            return Err(RestoreError("unknown record version"));
        }
        match parse_key(key)? {
            Subject::Link(link) => {
                let model: LinkModel = minicbor::decode(body).map_err(|_| RestoreError("link record"))?;
                let at = model.observed_at();
                // Saved by a version that kept the two directions apart:
                // keep the direction observed last.
                let path = link.path();
                if path != link {
                    self.removed.insert(Subject::Link(link));
                    self.changed.insert(Subject::Link(path));
                }
                if self.links.get(&path).is_none_or(|kept| kept.observed_at() <= at) {
                    self.links.insert(path, model);
                }
                self.refit_link_prior(link.bearer, at);
            }
            Subject::Custodian(station) => {
                let model: CustodianModel =
                    minicbor::decode(body).map_err(|_| RestoreError("custodian record"))?;
                let at = model.observed_at();
                self.custodians.insert(station, model);
                self.refit_custodian_prior(at);
            }
            Subject::Calibration(bearer, seen) => {
                let record: Record =
                    minicbor::decode(body).map_err(|_| RestoreError("calibration record"))?;
                self.calibration[bearer.index()][usize::from(seen)] = Calibration::from_record(record);
            }
        }
        Ok(())
    }
}

fn encode_value<T: minicbor::Encode<()>>(model: &T) -> Vec<u8> {
    let mut out = alloc::vec![RECORD_VERSION];
    out.extend(minicbor::to_vec(model).expect("encoding to a vector cannot fail"));
    out
}

fn record_key(subject: Subject) -> Vec<u8> {
    match subject {
        Subject::Link(key) => {
            let mut out = alloc::vec![LINK_RECORD];
            out.extend_from_slice(&key.from.to_bytes());
            out.extend_from_slice(&key.to.to_bytes());
            out.push(key.bearer.index() as u8);
            out
        }
        Subject::Custodian(station) => {
            let mut out = alloc::vec![CUSTODIAN_RECORD];
            out.extend_from_slice(&station.to_bytes());
            out
        }
        Subject::Calibration(bearer, seen) => {
            alloc::vec![CALIBRATION_RECORD, bearer.index() as u8, u8::from(seen)]
        }
    }
}

fn parse_key(key: &[u8]) -> Result<Subject, RestoreError> {
    let call = |bytes: &[u8]| -> Result<Callsign, RestoreError> {
        let bytes: [u8; 6] = bytes.try_into().map_err(|_| RestoreError("callsign"))?;
        Callsign::from_bytes(bytes).map_err(|_| RestoreError("callsign"))
    };
    match key {
        [LINK_RECORD, rest @ ..] if rest.len() == 13 => Ok(Subject::Link(LinkKey {
            from: call(&rest[..6])?,
            to: call(&rest[6..12])?,
            bearer: Bearer::from_index(rest[12]).ok_or(RestoreError("bearer"))?,
        })),
        [CUSTODIAN_RECORD, rest @ ..] if rest.len() == 6 => Ok(Subject::Custodian(call(rest)?)),
        [CALIBRATION_RECORD, bearer, seen @ (0 | 1)] => Ok(Subject::Calibration(
            Bearer::from_index(*bearer).ok_or(RestoreError("bearer"))?,
            *seen == 1,
        )),
        _ => Err(RestoreError("record key")),
    }
}
