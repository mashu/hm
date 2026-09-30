//! Stations heard on the radio: from any frame's source callsign, and from
//! beacons, whose keys are compared with the trusted keys (never adopted).

use std::collections::BTreeMap;

use hm_wire::{Callsign, Heard, Locator, FLAG_INTERNET, FLAG_MAILBOX, FLAG_RELAY, MAX_HEARD};
use hm_xfer::beacon::HeardBeacon;

use crate::Trust;

/// How a beacon's key compares with the trusted key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyCheck {
    /// The key trusted for the station.
    Trusted,
    /// The station is not trusted.
    Unknown,
    /// Another key is trusted for it: an impostor, or a station with a new key.
    Mismatch,
}

impl KeyCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyCheck::Trusted => "trusted",
            KeyCheck::Unknown => "unknown",
            KeyCheck::Mismatch => "mismatch",
        }
    }
}

/// One station as last heard.
#[derive(Clone, Debug, PartialEq)]
pub struct Station {
    pub call: Callsign,
    /// Unix seconds when any frame from it was last heard.
    pub last: u64,
    /// From its latest beacon, if any.
    pub beacon: Option<BeaconSeen>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BeaconSeen {
    pub at: u64,
    pub key: KeyCheck,
    pub flags: u8,
    /// Its clock minus ours when the beacon arrived, in seconds.
    pub clock_offset: i64,
    /// Stations it says it has heard.
    pub heard: Vec<Heard>,
    /// Where it says it is.
    pub locator: Option<Locator>,
}

impl BeaconSeen {
    pub fn offers(&self) -> Vec<&'static str> {
        [
            (FLAG_MAILBOX, "mailbox"),
            (FLAG_RELAY, "relay"),
            (FLAG_INTERNET, "internet"),
        ]
        .into_iter()
        .filter(|(f, _)| self.flags & f != 0)
        .map(|(_, n)| n)
        .collect()
    }
}

/// Stations heard lately, by callsign (SSIDs apart: they are different stations on air).
#[derive(Default)]
pub struct HeardTable {
    stations: BTreeMap<Callsign, Station>,
}

/// Stations not heard for this long are forgotten.
const FORGET_AFTER: u64 = 24 * 3600;
/// A beacon lists stations heard within this long.
const BEACON_WINDOW: u64 = 3600;

impl HeardTable {
    /// A frame from `from` at `now`; true when the station is new.
    pub fn frame(&mut self, now: u64, from: Callsign) -> bool {
        let mut new = false;
        self.stations
            .entry(from)
            .and_modify(|s| s.last = s.last.max(now))
            .or_insert_with(|| {
                new = true;
                Station {
                    call: from,
                    last: now,
                    beacon: None,
                }
            });
        new
    }

    /// A beacon heard; returns how its key compares with `trust`, or `None`
    /// when it names the trusted key and does not verify with it (forged or
    /// damaged), and is dropped. A station with no trusted key cannot be
    /// checked: its beacon is listed, and believed in nothing.
    pub fn beacon(&mut self, now: u64, b: &HeardBeacon, trust: &Trust) -> Option<KeyCheck> {
        let key = match trust.key_for(b.from) {
            None => KeyCheck::Unknown,
            Some(k) if b.signed_by(&k) => KeyCheck::Trusted,
            Some(k) if k.id() != b.beacon.key_id => KeyCheck::Mismatch,
            Some(_) => return None,
        };
        self.frame(now, b.from);
        let s = self.stations.get_mut(&b.from).expect("inserted above");
        s.beacon = Some(BeaconSeen {
            at: now,
            key,
            flags: b.beacon.flags,
            clock_offset: b.beacon.time as i64 - now as i64,
            heard: b.beacon.heard.clone(),
            locator: b.beacon.locator,
        });
        Some(key)
    }

    /// Drop stations not heard for a day.
    pub fn expire(&mut self, now: u64) {
        self.stations.retain(|_, s| s.last + FORGET_AFTER > now);
    }

    /// Everything heard, most recent first.
    pub fn list(&self) -> Vec<Station> {
        let mut v: Vec<Station> = self.stations.values().cloned().collect();
        v.sort_by(|a, b| b.last.cmp(&a.last).then(a.call.cmp(&b.call)));
        v
    }

    /// Stations heard within `within` seconds: those sharing the channel with us.
    pub fn active(&self, now: u64, within: u64) -> usize {
        self.stations
            .values()
            .filter(|s| now.saturating_sub(s.last) < within)
            .count()
    }

    /// For our own beacon: up to 16 stations heard within the hour (or within
    /// `window`, if longer: beacons are further apart on a busy channel), most
    /// recent first.
    pub fn for_beacon(&self, now: u64, window: u64) -> Vec<Heard> {
        let window = window.max(BEACON_WINDOW);
        self.list()
            .into_iter()
            .filter(|s| now.saturating_sub(s.last) < window)
            .take(MAX_HEARD)
            .map(|s| Heard {
                call: s.call,
                minutes: (now.saturating_sub(s.last) / 60).min(255) as u8,
            })
            .collect()
    }
}

/// Great-circle distance in km and initial bearing in degrees (0 = north)
/// from the centre of one grid square to the centre of another.
pub fn distance(from: Locator, to: Locator) -> (f64, f64) {
    const EARTH_KM: f64 = 6371.0;
    let ((la1, lo1), (la2, lo2)) = (from.centre(), to.centre());
    let (p1, p2) = (la1.to_radians(), la2.to_radians());
    let dl = (lo2 - lo1).to_radians();
    let a = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    let km = 2.0 * EARTH_KM * a.sqrt().min(1.0).asin();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    (km, (y.atan2(x).to_degrees() + 360.0) % 360.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_ident::Identity;
    use hm_xfer::beacon::{beacon_frame, read_beacon};

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    fn beacon(secret: u8, from: &str, time: u32) -> HeardBeacon {
        let f = beacon_frame(
            &Identity::from_secret([secret; 32]),
            call(from),
            FLAG_MAILBOX,
            time,
            Locator::parse("KO02").ok(),
            vec![],
        )
        .unwrap();
        read_beacon(&f).unwrap()
    }

    #[test]
    fn keys_are_compared_with_the_trust_file_not_learned() {
        let mut trust = Trust::default();
        trust.insert(call("SA0KAM"), Identity::from_secret([1; 32]).public());
        let mut t = HeardTable::default();
        assert_eq!(
            t.beacon(1000, &beacon(1, "SA0KAM-10", 1005), &trust),
            Some(KeyCheck::Trusted)
        );
        assert_eq!(
            t.beacon(1000, &beacon(2, "SA0KAM", 1000), &trust),
            Some(KeyCheck::Mismatch)
        );
        assert_eq!(
            t.beacon(1000, &beacon(3, "SO5KM", 990), &trust),
            Some(KeyCheck::Unknown)
        );
        // Hearing an unknown station's beacon twice does not make it trusted.
        assert_eq!(
            t.beacon(1100, &beacon(3, "SO5KM", 1100), &trust),
            Some(KeyCheck::Unknown)
        );
        // Naming the trusted key without its signature: dropped.
        let mut forged = beacon(2, "SA0KAM", 1200);
        forged.beacon.key_id = Identity::from_secret([1; 32]).public().id();
        assert_eq!(t.beacon(1200, &forged, &trust), None);
        let list = t.list();
        let kam10 = list.iter().find(|s| s.call == call("SA0KAM-10")).unwrap();
        let seen = kam10.beacon.as_ref().unwrap();
        assert_eq!((seen.clock_offset, seen.offers()), (5, vec!["mailbox"]));
        assert_eq!(seen.locator.unwrap().to_string(), "KO02");
    }

    #[test]
    fn our_beacon_lists_the_last_hour_most_recent_first() {
        let mut t = HeardTable::default();
        assert!(t.frame(0, call("SP5AAA")));
        assert!(!t.frame(10, call("SP5AAA")));
        t.frame(3000, call("SO5KM-1"));
        t.frame(3500, call("SA0KAM"));
        let b = t.for_beacon(3700, 0);
        let got: Vec<(Callsign, u8)> = b.iter().map(|h| (h.call, h.minutes)).collect();
        // SP5AAA was last heard at 10 s: over an hour before 3700 s.
        assert_eq!(got, vec![(call("SA0KAM"), 3), (call("SO5KM-1"), 11)]);
        for i in 0..20u32 {
            t.frame(3600, call(&format!("SQ{i}XX")));
        }
        assert_eq!(t.for_beacon(3700, 0).len(), MAX_HEARD);
        t.expire(10 + FORGET_AFTER);
        assert!(t.list().iter().all(|s| s.call != call("SP5AAA")));
    }

    #[test]
    fn distance_and_bearing_between_squares() {
        let l = |s| Locator::parse(s).unwrap();
        // Stockholm to Warsaw: about 810 km, south-south-east.
        let (km, deg) = distance(l("JO89xi"), l("KO02md"));
        assert!((km - 810.0).abs() < 25.0, "{km}");
        assert!((150.0..170.0).contains(&deg), "{deg}");
        assert_eq!(distance(l("JO89"), l("JO89")).0, 0.0);
    }
}
