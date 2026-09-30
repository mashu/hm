//! Settings: what can change while the station runs, what needs a
//! restart, and what is fixed by the configuration file.

use hm_wire::{Callsign, Locator};

use crate::config::RadioSettings;
use crate::node::live::{Change, RestartChange};

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{bad, internal, ApiError, AppState};

#[derive(Serialize, Deserialize)]
struct PeerView {
    station: String,
    address: String,
}

/// `[radio]`; `PATCH` takes any of the fields. A change opens a new radio link.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RadioView {
    enabled: Option<bool>,
    kiss: Option<String>,
    tnc_port: Option<u8>,
    /// The built-in modem's sound card; "" for a KISS TNC instead.
    audio: Option<String>,
    ptt: Option<String>,
    /// Built-in modem framing: "ax25", "il2p" or "auto".
    framing: Option<String>,
    persist: Option<u8>,
    slottime_ms: Option<u64>,
    bitrate: Option<u32>,
    txdelay_ms: Option<u64>,
    guard_ms: Option<u64>,
    max_rounds: Option<u32>,
    max_keyup_secs: Option<u64>,
}

impl RadioView {
    fn of(r: &RadioSettings) -> RadioView {
        RadioView {
            enabled: Some(r.enabled),
            kiss: Some(r.kiss.clone()),
            tnc_port: Some(r.tnc_port),
            audio: Some(r.audio.clone().unwrap_or_default()),
            ptt: Some(r.ptt.clone()),
            framing: Some(r.framing.clone()),
            persist: Some(r.persist),
            slottime_ms: Some(r.slottime_ms),
            bitrate: Some(r.bitrate),
            txdelay_ms: Some(r.txdelay_ms),
            guard_ms: Some(r.guard_ms),
            max_rounds: Some(r.max_rounds),
            max_keyup_secs: Some(r.max_keyup_secs),
        }
    }

    /// `r` with the fields given here changed.
    fn apply(self, r: &RadioSettings) -> RadioSettings {
        let mut r = r.clone();
        let audio = self
            .audio
            .map(|a| Some(a.trim().to_string()).filter(|a| !a.is_empty()));
        macro_rules! take {
            ($($f:ident),*) => { $(if let Some(v) = self.$f { r.$f = v; })* };
        }
        take!(
            enabled,
            kiss,
            tnc_port,
            ptt,
            framing,
            persist,
            slottime_ms,
            bitrate,
            txdelay_ms,
            guard_ms,
            max_rounds,
            max_keyup_secs
        );
        if let Some(a) = audio {
            r.audio = a;
        }
        r
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RelayView {
    enabled: Option<bool>,
    mailbox: Option<bool>,
    max_holdings: Option<usize>,
    max_bytes: Option<u64>,
    max_hops: Option<u8>,
    airtime_budget_secs: Option<u64>,
    control_airtime_fraction: Option<f64>,
}

impl RelayView {
    fn of(r: &crate::config::RelaySettings) -> RelayView {
        RelayView {
            enabled: Some(r.enabled),
            mailbox: Some(r.mailbox),
            max_holdings: Some(r.max_holdings),
            max_bytes: Some(r.max_bytes),
            max_hops: Some(r.max_hops),
            airtime_budget_secs: Some(r.airtime_budget_secs),
            control_airtime_fraction: Some(r.control_airtime_fraction),
        }
    }

    fn apply(self, r: &crate::config::RelaySettings) -> crate::config::RelaySettings {
        let mut r = r.clone();
        macro_rules! take {
            ($($f:ident),*) => { $(if let Some(v) = self.$f { r.$f = v; })* };
        }
        take!(
            enabled,
            mailbox,
            max_holdings,
            max_bytes,
            max_hops,
            airtime_budget_secs,
            control_airtime_fraction
        );
        r
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ModemView {
    enabled: Option<bool>,
    kind: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    bandwidth: Option<u32>,
    ptt: Option<String>,
}

impl ModemView {
    fn of(m: &crate::config::ModemSettings) -> ModemView {
        ModemView {
            enabled: Some(m.enabled),
            kind: Some(m.kind.clone()),
            host: Some(m.host.clone()),
            port: Some(m.port),
            bandwidth: Some(m.bandwidth),
            ptt: Some(m.ptt.clone()),
        }
    }

    fn apply(self, m: &crate::config::ModemSettings) -> crate::config::ModemSettings {
        let mut m = m.clone();
        macro_rules! take {
            ($($f:ident),*) => { $(if let Some(v) = self.$f { m.$f = v; })* };
        }
        take!(enabled, kind, host, port, bandwidth, ptt);
        m
    }
}

/// Settings that apply at once; `PATCH` takes any of them.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct LiveView {
    beacon_minutes: Option<u64>,
    radio_cost: Option<f64>,
    internet_cost: Option<f64>,
    modem_cost: Option<f64>,
    retry_first_secs: Option<u64>,
    retry_max_secs: Option<u64>,
    retry_attempts: Option<u32>,
    custody_grace_secs: Option<u64>,
    custody_suspect_secs: Option<u64>,
    receipt_retry_attempts: Option<u32>,
    relay: Option<RelayView>,
    peers: Option<Vec<PeerView>>,
    /// A grid locator; "" removes it.
    locator: Option<String>,
    radio: Option<RadioView>,
    /// Restart-bound settings: saved to the file, take effect on the next start.
    restart_to_change: Option<FixedView>,
}

/// Settings read at start-up; changing them in the file takes a restart.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FixedView {
    internet_listen: Option<String>,
    open_hub: Option<bool>,
    modem: Option<ModemView>,
    http: Option<String>,
    store: Option<String>,
}

#[derive(Serialize)]
pub(super) struct SettingsView {
    file: Option<String>,
    live: LiveView,
    /// Whether `[radio]` changes open a new link at once (otherwise they are
    /// saved and take a restart).
    radio_applies_now: bool,
    /// Settings the command line sets for this run; the file's values for
    /// these are saved but not used until the node runs without them.
    overridden: Vec<String>,
    restart_to_change: FixedView,
}

fn fixed_view(s: &AppState) -> FixedView {
    match s.live.file_config() {
        Some(Ok(c)) => FixedView {
            internet_listen: Some(c.internet.listen.clone().unwrap_or_default()),
            open_hub: Some(c.internet.open_hub),
            modem: Some(ModemView::of(&c.modem)),
            http: Some(c.station.http.clone()),
            store: Some(c.station.store.display().to_string()),
        },
        _ => {
            let modem = match &s.cfg.modem {
                Some(m) => ModemView::of(&crate::config::ModemSettings {
                    enabled: true,
                    kind: match m.kind {
                        crate::node::arq::Kind::Vara => "vara".into(),
                        crate::node::arq::Kind::Ardop => "ardop".into(),
                    },
                    host: m.host.clone(),
                    port: m.port,
                    bandwidth: m.bandwidth,
                    ..crate::config::ModemSettings::default()
                }),
                None => ModemView::of(&crate::config::ModemSettings::default()),
            };
            FixedView {
                internet_listen: Some(
                    s.status
                        .lock()
                        .expect("lock")
                        .internet_listen
                        .map(|a| a.to_string())
                        .unwrap_or_default(),
                ),
                open_hub: Some(s.cfg.internet.as_ref().is_some_and(|i| i.open_hub)),
                modem: Some(modem),
                http: Some(s.cfg.http.to_string()),
                store: Some(s.cfg.store.display().to_string()),
            }
        }
    }
}

fn settings_view(s: &AppState) -> SettingsView {
    let l = s.live.get();
    SettingsView {
        file: s.live.file().map(|p| p.display().to_string()),
        live: LiveView {
            beacon_minutes: Some(l.beacon_secs / 60),
            radio_cost: Some(l.costs.radio),
            internet_cost: Some(l.costs.internet),
            modem_cost: Some(l.costs.modem),
            retry_first_secs: Some(l.retry.first_delay_secs),
            retry_max_secs: Some(l.retry.max_delay_secs),
            retry_attempts: Some(l.retry.max_attempts),
            custody_grace_secs: Some(l.custody_grace_secs),
            custody_suspect_secs: Some(l.custody_suspect_secs),
            receipt_retry_attempts: Some(l.receipt_retry.max_attempts),
            relay: Some(RelayView::of(&l.relay)),
            peers: Some(
                l.peers
                    .iter()
                    .map(|(c, a)| PeerView {
                        station: c.to_string(),
                        address: a.clone(),
                    })
                    .collect(),
            ),
            locator: Some(l.locator.map(|g| g.to_string()).unwrap_or_default()),
            radio: Some(RadioView::of(&l.radio)),
            restart_to_change: None,
        },
        radio_applies_now: s.cfg.radio_builder.is_some(),
        overridden: s.cfg.overridden.clone(),
        restart_to_change: fixed_view(s),
    }
}

pub(super) async fn get_settings(State(s): State<AppState>) -> Json<SettingsView> {
    Json(settings_view(&s))
}

pub(super) async fn change_settings(
    State(s): State<AppState>,
    Json(req): Json<LiveView>,
) -> Result<Json<SettingsView>, ApiError> {
    let peers = match req.peers {
        None => None,
        Some(list) => Some(
            list.into_iter()
                .map(|p| {
                    let call = Callsign::parse(p.station.trim())
                        .map_err(|e| bad(format!("peer {}: {e}", p.station)))?;
                    let address = p.address.trim().to_string();
                    if !address.contains(':') {
                        return Err(bad(format!("peer {call}: address {address:?} needs a port")));
                    }
                    Ok((call, address))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };
    let locator = match req.locator.as_deref().map(str::trim) {
        None => None,
        Some("") => Some(None),
        Some(g) => Some(Some(
            Locator::parse(g).map_err(|e| bad(format!("locator {g:?}: {e}")))?,
        )),
    };
    let radio = req.radio.map(|r| r.apply(&s.live.get().radio));
    let relay = req.relay.map(|r| r.apply(&s.live.get().relay));
    let has_live = radio.is_some()
        || locator.is_some()
        || peers.is_some()
        || relay.is_some()
        || req.beacon_minutes.is_some()
        || req.radio_cost.is_some()
        || req.internet_cost.is_some()
        || req.modem_cost.is_some()
        || req.retry_first_secs.is_some()
        || req.retry_max_secs.is_some()
        || req.retry_attempts.is_some()
        || req.custody_grace_secs.is_some()
        || req.custody_suspect_secs.is_some()
        || req.receipt_retry_attempts.is_some();
    if has_live {
        s.live
            .change(Change {
                radio,
                locator,
                beacon_minutes: req.beacon_minutes,
                radio_cost: req.radio_cost,
                internet_cost: req.internet_cost,
                modem_cost: req.modem_cost,
                retry_first_secs: req.retry_first_secs,
                retry_max_secs: req.retry_max_secs,
                retry_attempts: req.retry_attempts,
                custody_grace_secs: req.custody_grace_secs,
                custody_suspect_secs: req.custody_suspect_secs,
                receipt_retry_attempts: req.receipt_retry_attempts,
                relay,
                peers,
            })
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::InvalidInput => bad(e.to_string()),
                _ => internal(e),
            })?;
    }
    let mut restarted = false;
    if let Some(restart) = req.restart_to_change {
        let base_modem = s
            .live
            .file_config()
            .and_then(|r| r.ok())
            .map(|c| c.modem)
            .unwrap_or_default();
        let modem = restart.modem.map(|m| m.apply(&base_modem));
        s.live
            .save_restart(RestartChange {
                internet_listen: restart.internet_listen,
                open_hub: restart.open_hub,
                modem,
                http: restart.http,
                store: restart.store,
            })
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::InvalidInput => bad(e.to_string()),
                _ => internal(e),
            })?;
        restarted = true;
    }
    if has_live || restarted {
        crate::node::log("settings changed (through the API)");
    }
    Ok(Json(settings_view(&s)))
}
