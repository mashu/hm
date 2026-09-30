//! What the latest observations did to the beliefs: a bounded record, so
//! that a person can see evidence move the chances.
//!
//! Every observation the beliefs take in is recorded with the chance the
//! belief gave before and after it: for a link, that a handoff over it now
//! completes; for a custodian, that it takes custody and does its part; for
//! a calibration record, the chance that comes true when the one the update
//! was about is given. The record keeps the last [`JOURNAL_LEN`]
//! observations of each kind of subject (a ring buffer each): beacons due
//! and missed come by the hundred a day, and would push the rarer handoffs
//! and receipts out of one shared record. It is not saved: it explains the
//! beliefs, it is not part of them.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use hm_wire::Callsign;

use crate::custodian::CustodianObservation;
use crate::link::LinkObservation;
use crate::{Bearer, LinkKey};

/// Observations the journal keeps of each kind of subject.
pub const JOURNAL_LEN: usize = 128;

/// What a belief is about.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subject {
    /// A path, by [its key](LinkKey::path).
    Link(LinkKey),
    Custodian(Callsign),
    /// A bearer's calibration, for paths seen open (`true`) or not.
    Calibration(Bearer, bool),
}

impl Subject {
    fn kind(&self) -> usize {
        match self {
            Subject::Link(_) => 0,
            Subject::Custodian(_) => 1,
            Subject::Calibration(..) => 2,
        }
    }
}

/// What was observed.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Observed {
    Link(LinkObservation),
    Custodian(CustodianObservation),
    /// A handoff given `chance` ended, the link carrying it or not.
    Outcome {
        chance: f64,
        carried: bool,
    },
}

/// One observation and what it did.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Update {
    pub at: u64,
    pub subject: Subject,
    pub observed: Observed,
    /// The belief's chance before the observation.
    pub before: f64,
    /// And after it.
    pub after: f64,
}

/// The last [`JOURNAL_LEN`] updates of each kind, numbered as taken in.
#[derive(Clone, Debug, Default)]
pub(super) struct Journal {
    kinds: [VecDeque<(u64, Update)>; 3],
    taken: u64,
}

impl Journal {
    pub(super) fn record(&mut self, update: Update) {
        let ring = &mut self.kinds[update.subject.kind()];
        if ring.len() == JOURNAL_LEN {
            ring.pop_front();
        }
        ring.push_back((self.taken, update));
        self.taken += 1;
    }

    /// Every update kept, in the order taken in.
    pub(super) fn updates(&self) -> impl DoubleEndedIterator<Item = &Update> {
        let mut all: Vec<&(u64, Update)> = self.kinds.iter().flatten().collect();
        all.sort_unstable_by_key(|(taken, _)| *taken);
        all.into_iter().map(|(_, update)| update)
    }
}
