//! Where the node's log lines go, and small formatting helpers.

use std::cell::Cell;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use hm_wire::{Callsign, ObjectId};

type Sink = Box<dyn Fn(&str) + Send + Sync>;

static SINK: RwLock<Option<Sink>> = RwLock::new(None);

thread_local! {
    /// The station whose work this thread is doing, when several share it,
    /// and its time.
    static STATION: Cell<Option<(Callsign, u64)>> = const { Cell::new(None) };
}

/// Run `f` as `station`'s work at `now` (Unix seconds): its log lines name
/// the station and the time. For a simulation, where many stations share a
/// thread and time is simulated.
pub fn as_station<R>(station: Callsign, now: u64, f: impl FnOnce() -> R) -> R {
    let outer = STATION.replace(Some((station, now)));
    let result = f();
    STATION.set(outer);
    result
}

/// Send log lines to `sink` instead of standard error (a simulation of many
/// nodes over weeks may drop them).
pub fn set_log(sink: impl Fn(&str) + Send + Sync + 'static) {
    *SINK.write().expect("log sink") = Some(Box::new(sink));
}

/// One log line: to the sink set with [`set_log`], else to standard error
/// with the time.
pub fn log(msg: impl AsRef<str>) {
    let named;
    let msg = match STATION.get() {
        Some((station, now)) => {
            named = format!("{} {station}: {}", utc_clock(now), msg.as_ref());
            named.as_str()
        }
        None => msg.as_ref(),
    };
    match SINK.read().expect("log sink").as_ref() {
        Some(sink) => sink(msg),
        None => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            eprintln!("{} {}", utc_clock(now), msg);
        }
    }
}

/// `YYYY-MM-DD HH:MM:SSZ` for a Unix timestamp.
pub fn utc_clock(unix: u64) -> String {
    let secs = unix % 86_400;
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
}

/// Days since 1970-01-01 to a civil date. Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// The first twelve hex digits of an object id, for log lines.
pub fn short(id: &ObjectId) -> String {
    id.to_string()[..12].to_string()
}

/// Whether mail to `to` is for this node, run as `me` on a key for `key_call`:
/// its own callsign, or the bare callsign its key is bound to.
pub fn addressed_to_us(to: Callsign, me: Callsign, key_call: Callsign) -> bool {
    to == me || (to == key_call && key_call == key_call.base())
}

/// Tells whoever watches what changed: `"message"` (one arrived, was queued,
/// delivered or read), `"status"` (radio, links, stations heard) or
/// `"settings"`.
#[derive(Clone)]
pub struct Notify(std::sync::Arc<dyn Fn(&'static str) + Send + Sync>);

impl Notify {
    pub fn new(f: impl Fn(&'static str) + Send + Sync + 'static) -> Notify {
        Notify(std::sync::Arc::new(f))
    }

    /// Nobody watches.
    pub fn none() -> Notify {
        Notify::new(|_| {})
    }

    pub fn send(&self, what: &'static str) {
        (self.0)(what)
    }
}
