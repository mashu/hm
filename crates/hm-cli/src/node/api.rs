//! JSON API and the built-in web page.
//!
//! Every `/api` request needs `Authorization: Bearer <token>`; the token is
//! created on first start next to the store. The page at `/` holds no data and
//! is public; it asks for the token and keeps it in the browser.
//!
//! | Method | Path | |
//! | --- | --- | --- |
//! | GET | `/` | web page |
//! | GET | `/api/status` | callsign, key, bearers and their estimated success per station |
//! | GET | `/api/messages?direction=in\|out&limit=n` | newest first |
//! | POST | `/api/send` | `{"to", "text", "subject"?, "precedence"?}` → `{"id"}` |
//! | POST | `/api/read/{id}` | mark an inbound message read |

use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hm_bundle::{Address, Opened, Precedence};
use hm_store::{Direction, Record, Store};
use hm_wire::{Callsign, ObjectId};
use serde::{Deserialize, Serialize};

use super::{NodeConfig, Status};
use crate::station::{build_bundle, unix_now};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub cfg: Arc<NodeConfig>,
    pub status: Arc<Mutex<Status>>,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/status", get(status))
        .route("/api/messages", get(messages))
        .route("/api/send", post(send))
        .route("/api/read/{id}", post(mark_read))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new().route("/", get(index)).merge(api).with_state(state)
}

/// Compare in time independent of where the inputs differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_token(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if same(given.as_bytes(), s.cfg.token.as_bytes()) {
        next.run(req).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "missing or wrong access token".into()).into_response()
    }
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

const INDEX: &str = include_str!("index.html");

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(INDEX),
    )
}

#[derive(Serialize)]
struct Estimate {
    station: String,
    bearer: &'static str,
    success: f64,
}

#[derive(Serialize)]
struct StatusView {
    call: String,
    public_key: String,
    trust_line: String,
    /// `null` when the node has no radio; otherwise whether the TNC is connected.
    radio: Option<bool>,
    /// How the radio is reached, when the node has one.
    radio_via: Option<String>,
    internet_listen: Option<String>,
    internet_peers: Vec<String>,
    estimates: Vec<Estimate>,
}

async fn status(State(s): State<AppState>) -> Json<StatusView> {
    let st = s.status.lock().expect("lock").clone();
    Json(StatusView {
        call: s.cfg.me.to_string(),
        public_key: crate::hex::encode(&s.cfg.key.identity.public().0),
        trust_line: s.cfg.key.trust_line(),
        radio: st.radio,
        radio_via: s.cfg.radio.as_ref().map(|r| r.describe()),
        internet_listen: st.internet_listen.map(|a| a.to_string()),
        internet_peers: st.internet_peers.iter().map(|c| c.to_string()).collect(),
        estimates: st
            .estimates
            .into_iter()
            .map(|(c, b, p)| Estimate {
                station: c.to_string(),
                bearer: b,
                success: (p * 1000.0).round() / 1000.0,
            })
            .collect(),
    })
}

#[derive(Deserialize)]
struct ListQuery {
    direction: Option<String>,
    limit: Option<usize>,
}

/// A message as the web page shows it. Every text field comes from the air
/// and must be displayed as text, never as HTML.
#[derive(Serialize, Debug)]
pub struct MessageView {
    id: String,
    direction: &'static str,
    peer: String,
    at: u64,
    state: String,
    precedence: u8,
    attempts: u32,
    next_attempt: u64,
    verified: bool,
    note: Option<String>,
    delivered_by: Option<String>,
    from: Option<String>,
    to: Vec<String>,
    kind: Option<String>,
    subject: Option<String>,
    text: Option<String>,
}

fn view(r: Record, object: Option<Vec<u8>>) -> MessageView {
    let bundle = object.and_then(|o| Opened::decode(&o).ok()).map(|o| o.bundle);
    MessageView {
        id: r.id.to_string(),
        direction: if r.direction == Direction::In { "in" } else { "out" },
        peer: r.peer.to_string(),
        at: r.at,
        state: format!("{:?}", r.state),
        precedence: r.precedence,
        attempts: r.attempts,
        next_attempt: r.next_attempt,
        verified: r.verified,
        note: r.note,
        delivered_by: r.by,
        from: bundle.as_ref().map(|b| b.from.to_string()),
        to: bundle
            .as_ref()
            .map(|b| {
                b.to.iter()
                    .map(|a| match a {
                        Address::Station(c) => c.to_string(),
                        Address::Group(g) => format!("group:{g}"),
                        Address::Tactical(t) => format!("tactical:{t}"),
                        Address::Email(e) => e.clone(),
                        Address::Other { tag, .. } => format!("other:{tag}"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        kind: bundle.as_ref().map(|b| format!("{:?}", b.kind)),
        subject: bundle.as_ref().and_then(|b| b.subject.clone()),
        text: bundle
            .as_ref()
            .and_then(|b| b.body.as_ref()?.as_text().ok().map(str::to_string)),
    }
}

async fn messages(
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<MessageView>>, ApiError> {
    let direction = match q.direction.as_deref().unwrap_or("in") {
        "in" => Direction::In,
        "out" => Direction::Out,
        other => return Err(bad(format!("direction must be in or out, not {other:?}"))),
    };
    let limit = q.limit.unwrap_or(100).min(1000);
    let store = s.store.clone();
    tokio::task::spawn_blocking(move || {
        let records = store.list(direction, limit).map_err(internal)?;
        records
            .into_iter()
            .map(|r| {
                let obj = store.object(r.id).map_err(internal)?;
                Ok(view(r, obj))
            })
            .collect::<Result<Vec<_>, ApiError>>()
    })
    .await
    .map_err(internal)?
    .map(Json)
}

#[derive(Deserialize)]
struct SendRequest {
    to: String,
    text: String,
    subject: Option<String>,
    precedence: Option<String>,
}

fn parse_precedence(p: Option<&str>) -> Result<Precedence, ApiError> {
    Ok(match p.unwrap_or("routine") {
        "routine" => Precedence::Routine,
        "priority" => Precedence::Priority,
        "immediate" => Precedence::Immediate,
        "flash" => Precedence::Flash,
        other => return Err(bad(format!("unknown precedence {other:?}"))),
    })
}

async fn send(
    State(s): State<AppState>,
    Json(req): Json<SendRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let to = Callsign::parse(req.to.trim()).map_err(|e| bad(format!("to: {e}")))?;
    if req.text.is_empty() {
        return Err(bad("text is empty"));
    }
    let prec = parse_precedence(req.precedence.as_deref())?;
    let subject = req.subject.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let bundle =
        build_bundle(&s.cfg.key, s.cfg.me, to, &req.text, subject, prec).map_err(|e| bad(e.to_string()))?;
    let (id, bytes) = (bundle.id(), bundle.to_vec());
    let store = s.store.clone();
    tokio::task::spawn_blocking(move || store.enqueue(id, &bytes, to, prec.to_u8(), unix_now()))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id.to_string() })),
    ))
}

async fn mark_read(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    let id = ObjectId(crate::hex::decode_32(&id).map_err(bad)?);
    let store = s.store.clone();
    match tokio::task::spawn_blocking(move || store.mark_read(id))
        .await
        .map_err(internal)?
    {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(hm_store::Error::NotFound) => Err(ApiError(StatusCode::NOT_FOUND, "no such message".into())),
        Err(e) => Err(internal(e)),
    }
}
