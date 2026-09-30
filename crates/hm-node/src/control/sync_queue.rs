//! SYNC frames waiting for the radio control budget: a newer frame about
//! the same thing replaces an older one, and what has expired is dropped.

use std::collections::VecDeque;

use hm_wire::{Dest, SyncMessage};

use super::holdings::PAIRWISE_STATE_SECS;
use super::trickle::{newer_serial, AdvertKey};

/// Frames waiting for the radio control budget.
pub const SYNC_QUEUE_FRAMES: usize = 64;

/// A SYNC frame waiting for the radio's control budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedSync {
    pub to: Dest,
    pub payload: Vec<u8>,
    /// Unix seconds after which what it says is no longer worth sending.
    pub expires_at: u64,
    /// Frames with the same slot say the same kind of thing about the same
    /// subject; a newer one replaces an older one still waiting.
    slot: Option<[u8; 32]>,
}

/// SYNC frames waiting for the radio's control budget. The budget releases a
/// frame every minute or so on a busy channel, so a plain queue would send
/// what was true an hour ago: a newer frame about the same contact, or to
/// the same station with the same kind of filter, takes the older one's
/// place, and a frame whose content has expired is dropped instead of sent.
pub struct SyncQueue {
    items: VecDeque<QueuedSync>,
    capacity: usize,
    dropped: u64,
}

impl SyncQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity,
            dropped: 0,
        }
    }

    /// Queue `payload` for `to`, at `now` (Unix seconds).
    pub fn push(&mut self, to: Dest, payload: Vec<u8>, now: u64) {
        let (slot, expires_at) = match SyncMessage::decode(&payload) {
            Ok(SyncMessage::Contact(advert)) => {
                (Some(AdvertKey::from(&advert).slot()), u64::from(advert.end))
            }
            Ok(SyncMessage::Filter(filter)) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(b"hm/sync-slot/filter");
                match to {
                    Dest::Station(call) => hasher.update(&call.to_bytes()),
                    Dest::Broadcast => hasher.update(&[0xff; 6]),
                };
                hasher.update(&[filter.scope]);
                (
                    Some(*hasher.finalize().as_bytes()),
                    now.saturating_add(PAIRWISE_STATE_SECS),
                )
            }
            // Offers and wants answer one exchange; the peer forgets it after this.
            _ => (None, now.saturating_add(PAIRWISE_STATE_SECS)),
        };
        let item = QueuedSync {
            to,
            payload,
            expires_at,
            slot,
        };
        if let Some(slot) = slot {
            if let Some(waiting) = self.items.iter_mut().find(|waiting| waiting.slot == Some(slot)) {
                *waiting = item;
                return;
            }
        }
        if self.items.len() >= self.capacity {
            self.dropped += 1;
            return;
        }
        self.items.push_back(item);
    }

    /// A SYNC frame heard on the air: a queued copy of the same contact, no
    /// newer than what was heard, need not go out again (Trickle suppression,
    /// carried through to frames already waiting for the budget).
    pub fn heard(&mut self, payload: &[u8]) {
        let Ok(SyncMessage::Contact(heard)) = SyncMessage::decode(payload) else {
            return;
        };
        let slot = AdvertKey::from(&heard).slot();
        let before = self.items.len();
        self.items.retain(|item| {
            item.slot != Some(slot)
                || match SyncMessage::decode(&item.payload) {
                    Ok(SyncMessage::Contact(queued)) => newer_serial(queued.sequence, heard.sequence),
                    _ => true,
                }
        });
        self.dropped += (before - self.items.len()) as u64;
    }

    /// The next frame worth sending at `now`, dropping expired ones on the way.
    pub fn front(&mut self, now: u64) -> Option<&QueuedSync> {
        while self.items.front().is_some_and(|item| item.expires_at <= now) {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.front()
    }

    pub fn pop(&mut self) -> Option<QueuedSync> {
        self.items.pop_front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Frames dropped: the queue was full, or they expired waiting.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
