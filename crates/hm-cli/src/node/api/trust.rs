//! The trust list: the stations whose keys this station knows.

use hm_wire::Callsign;

use crate::files::Trust;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{bad, internal, ApiError, AppState};

#[derive(Serialize)]
pub(super) struct TrustedView {
    station: String,
    key: String,
    note: Option<String>,
}

#[derive(Serialize)]
pub(super) struct TrustList {
    /// The settings file changes are saved to; `null` when the node has none,
    /// and changes last until it restarts.
    file: Option<String>,
    stations: Vec<TrustedView>,
}

pub(super) async fn list_trust(State(s): State<AppState>) -> Json<TrustList> {
    let live = s.live.get();
    Json(TrustList {
        file: s.live.file().map(|p| p.display().to_string()),
        stations: live
            .trust
            .iter()
            .map(|(c, k)| TrustedView {
                station: c.to_string(),
                key: crate::hex::encode(&k.0),
                note: live.note(c).map(str::to_string),
            })
            .collect(),
    })
}

/// A line as `hm whoami` prints it (`CALL KEY`), or the two parts apart, and
/// an optional note.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TrustRequest {
    line: Option<String>,
    station: Option<String>,
    key: Option<String>,
    note: Option<String>,
}

pub(super) async fn add_trust(
    State(s): State<AppState>,
    Json(req): Json<TrustRequest>,
) -> Result<(StatusCode, Json<TrustedView>), ApiError> {
    let line = match (req.line, req.station, req.key) {
        (Some(l), None, None) => l,
        (None, Some(c), Some(k)) => format!("{c} {k}"),
        _ => return Err(bad("give `line`, or `station` and `key`")),
    };
    let (call, key) = Trust::parse_line(&line)
        .map_err(bad)?
        .ok_or_else(|| bad("empty line"))?;
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    s.live.add_trust(call, key, note).map_err(internal)?;
    crate::node::log(format!("trusted {call} (through the API)"));
    Ok((
        StatusCode::CREATED,
        Json(TrustedView {
            station: call.to_string(),
            key: crate::hex::encode(&key.0),
            note: s.live.get().note(call).map(str::to_string),
        }),
    ))
}

pub(super) async fn remove_trust(
    State(s): State<AppState>,
    Path(station): Path<String>,
) -> Result<StatusCode, ApiError> {
    let call = Callsign::parse(&station).map_err(|e| bad(format!("station: {e}")))?;
    match s.live.remove_trust(call).map_err(internal)? {
        true => {
            crate::node::log(format!("no longer trusting {call} (through the API)"));
            Ok(StatusCode::NO_CONTENT)
        }
        false => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{call} has no entry of its own among the trusted stations"),
        )),
    }
}
