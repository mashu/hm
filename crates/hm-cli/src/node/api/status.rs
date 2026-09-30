//! `GET /api/status`: who this station is, its bearers, and how likely a
//! handoff to each station it knows of is to complete now.

use crate::station::unix_now;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use super::AppState;

#[derive(Serialize)]
struct Estimate {
    station: String,
    bearer: &'static str,
    success: f64,
}

#[derive(Serialize)]
pub(super) struct StatusView {
    call: String,
    public_key: String,
    trust_line: String,
    /// Our grid locator, as sent in beacons.
    locator: Option<String>,
    /// `null` when the node has no radio; otherwise whether the TNC is connected.
    radio: Option<bool>,
    /// How the radio is reached, when the node has one.
    radio_via: Option<String>,
    internet_listen: Option<String>,
    internet_peers: Vec<String>,
    /// `null` without a modem; otherwise whether the modem program is reachable.
    modem: Option<bool>,
    /// How the modem is reached, when the node has one.
    modem_via: Option<String>,
    /// The station the modem is connected to right now.
    modem_peer: Option<String>,
    estimates: Vec<Estimate>,
    /// Stations heard on the radio lately, most recent first.
    heard: Vec<HeardView>,
}

/// A station heard on the radio. `key` and the other beacon fields are
/// `null` until a beacon from it has been heard.
#[derive(Serialize)]
struct HeardView {
    station: String,
    /// Seconds since any frame from it was heard.
    ago: u64,
    /// "trusted", "unknown" or "mismatch": its beacon's key against the trusted one.
    key: Option<&'static str>,
    offers: Option<Vec<&'static str>>,
    beacon_ago: Option<u64>,
    /// Its clock minus ours, seconds.
    clock_offset: Option<i64>,
    /// Stations its beacon says it has heard.
    hears: Option<Vec<String>>,
    /// The grid locator its beacon gives.
    locator: Option<String>,
    /// From our locator to its, centre to centre, when both are known.
    distance_km: Option<f64>,
    /// Initial great-circle bearing from us, degrees from north.
    bearing: Option<f64>,
}

pub(super) async fn status(State(s): State<AppState>) -> Json<StatusView> {
    let st = s.status.lock().expect("lock").clone();
    let mine = s.live.get().locator;
    Json(StatusView {
        locator: mine.map(|l| l.to_string()),
        call: s.cfg.me.to_string(),
        public_key: crate::hex::encode(&s.cfg.key.identity.public().0),
        trust_line: s.cfg.key.trust_line(),
        radio: st.radio,
        radio_via: st.radio_via.clone(),
        internet_listen: st.internet_listen.map(|a| a.to_string()),
        internet_peers: st.internet_peers.iter().map(|c| c.to_string()).collect(),
        modem: st.modem,
        modem_via: s.cfg.modem.as_ref().map(|m| m.describe()),
        modem_peer: st.modem_peer.map(|c| c.to_string()),
        estimates: st
            .estimates
            .into_iter()
            .map(|(c, b, p)| Estimate {
                station: c.to_string(),
                bearer: b,
                success: (p * 1000.0).round() / 1000.0,
            })
            .collect(),
        heard: {
            let now = unix_now();
            st.heard
                .iter()
                .map(|h| {
                    let b = h.beacon.as_ref();
                    let theirs = b.and_then(|b| b.locator);
                    let far = mine.zip(theirs).map(|(a, t)| crate::node::heard::distance(a, t));
                    HeardView {
                        locator: theirs.map(|l| l.to_string()),
                        distance_km: far.map(|(km, _)| km.round()),
                        bearing: far.map(|(_, deg)| deg.round()),
                        station: h.call.to_string(),
                        ago: now.saturating_sub(h.last),
                        key: b.map(|b| b.key.as_str()),
                        offers: b.map(|b| b.offers()),
                        beacon_ago: b.map(|b| now.saturating_sub(b.at)),
                        clock_offset: b.map(|b| b.clock_offset),
                        hears: b.map(|b| b.heard.iter().map(|x| x.call.to_string()).collect()),
                    }
                })
                .collect()
        },
    })
}
