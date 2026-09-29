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
//! | GET | `/api/messages?direction=in\|out\|all&peer=CALL&kind=chat\|mail\|bulletin&group=NAME&limit=n` | newest first |
//! | GET | `/api/messages/{id}` | decoded metadata plus the exact raw signed object as hex |
//! | GET | `/api/events` | server-sent events naming what changed: `message`, `status`, `settings` |
//! | POST | `/api/send` | `{"to", "text", "subject"?, "precedence"?, "kind"?: "bulletin", "group"?}` → `{"id"}` |
//! | DELETE | `/api/messages/{id}` | cancel queued outbound, otherwise delete inactive local history |
//! | DELETE | `/api/conversations/{peer}` | delete inactive local chat history, preserving active delivery |
//! | POST | `/api/read/{id}` | mark an inbound message read |

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use hm_bundle::{Address, Opened, Precedence};
use hm_store::{Direction, Record, State as MessageState, Store};
use hm_wire::{Callsign, Locator, ObjectId};
use serde::{Deserialize, Serialize};

use super::live::{Change, LiveConfig, RestartChange};
use super::{NodeConfig, Notify, Status};
use crate::config::RadioSettings;
use crate::files::Trust;
use crate::station::{build_bulletin, build_bundle, unix_now, MAX_BULLETINS_PER_HOUR, MAX_BULLETIN_BYTES};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub cfg: Arc<NodeConfig>,
    pub status: Arc<Mutex<Status>>,
    pub live: Arc<LiveConfig>,
    pub notify: Notify,
    /// Unix times of local bulletin publishes in the last hour (rate limit).
    pub bulletin_publishes: Arc<Mutex<VecDeque<u64>>>,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/status", get(status))
        .route("/api/messages", get(messages))
        .route("/api/messages/{id}", get(message).delete(delete_message))
        .route("/api/conversations/{peer}", delete(delete_conversation))
        .route("/api/events", get(events))
        .route("/api/send", post(send))
        .route("/api/read/{id}", post(mark_read))
        .route("/api/trust", get(list_trust).post(add_trust))
        .route("/api/settings", get(get_settings).patch(change_settings))
        .route("/api/trust/{station}", delete(remove_trust))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/", get(index))
        .merge(api)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
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

async fn security_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'; \
             form-action 'self'; connect-src 'self' https://tiles.openfreemap.org; \
             worker-src blob:; script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; \
             style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; \
             img-src 'self' data: blob: https://cdn.jsdelivr.net https://tiles.openfreemap.org",
        ),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("geolocation=(self), camera=(), microphone=()"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow"),
    );
    response
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

async fn status(State(s): State<AppState>) -> Json<StatusView> {
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
                    let far = mine.zip(theirs).map(|(a, t)| super::heard::distance(a, t));
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

#[derive(Deserialize)]
struct ListQuery {
    direction: Option<String>,
    /// Only messages to or from this station (a conversation).
    peer: Option<String>,
    /// "chat", "mail" or "bulletin".
    kind: Option<String>,
    /// Bulletin group name (matches `group:NAME` in `to`).
    group: Option<String>,
    limit: Option<usize>,
}

/// A message as the web page shows it. Every text field comes from the air
/// and must be displayed as text, never as HTML.
#[derive(Serialize, Debug)]
pub struct MessageView {
    id: String,
    /// Order of arrival or queuing in this store, across both directions.
    seq: u64,
    /// Sender-assigned conversation sequence when present (chat).
    #[serde(skip_serializing_if = "Option::is_none")]
    wire_seq: Option<u64>,
    /// Soft FIFO gap hint when a higher wire_seq arrived without predecessors.
    #[serde(skip_serializing_if = "Option::is_none")]
    seq_gap: Option<String>,
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

#[derive(Serialize, Debug)]
struct MessageDetail {
    #[serde(flatten)]
    message: MessageView,
    raw_bytes: usize,
    raw_hex: String,
}

fn view(r: Record, object: Option<Vec<u8>>) -> MessageView {
    let bundle = object.and_then(|o| Opened::decode(&o).ok()).map(|o| o.bundle);
    MessageView {
        id: r.id.to_string(),
        seq: r.seq,
        wire_seq: r.wire_seq.or_else(|| bundle.as_ref().and_then(|b| b.seq)),
        seq_gap: None,
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
            .and_then(|b| b.body.as_ref()?.as_text().ok().map(|text| text.into_owned())),
    }
}

fn annotate_seq_gaps(messages: &mut [MessageView]) {
    // Per directed stream (from → peer): note gaps in wire_seq for soft FIFO UI.
    let mut by_stream: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(wire) = message.wire_seq else {
            continue;
        };
        let _ = wire;
        let from = message.from.clone().unwrap_or_else(|| message.peer.clone());
        let to = message
            .to
            .first()
            .cloned()
            .unwrap_or_else(|| message.peer.clone());
        by_stream.entry((from, to)).or_default().push(index);
    }
    for indices in by_stream.values() {
        let mut seqs: Vec<(usize, u64)> = indices
            .iter()
            .filter_map(|&i| messages[i].wire_seq.map(|s| (i, s)))
            .collect();
        seqs.sort_by_key(|(_, s)| *s);
        let mut expected = None;
        for &(index, seq) in &seqs {
            if let Some(exp) = expected {
                if seq > exp {
                    messages[index].seq_gap = Some(format!("missing seq {exp}..{}", seq - 1));
                }
            }
            expected = Some(seq.saturating_add(1));
        }
    }
}

async fn messages(
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<MessageView>>, ApiError> {
    let directions = match q.direction.as_deref().unwrap_or("in") {
        "in" => vec![Direction::In],
        "out" => vec![Direction::Out],
        "all" => vec![Direction::In, Direction::Out],
        other => return Err(bad(format!("direction must be in, out or all, not {other:?}"))),
    };
    let peer = q
        .peer
        .as_deref()
        .map(|p| Callsign::parse(p.trim()).map_err(|e| bad(format!("peer: {e}"))))
        .transpose()?
        .map(|c| c.to_string());
    let kind = match q.kind.as_deref() {
        None => None,
        Some("chat") => Some("Chat"),
        Some("mail") => Some("Mail"),
        Some("bulletin") => Some("Bulletin"),
        Some(other) => return Err(bad(format!("kind must be chat, mail or bulletin, not {other:?}"))),
    };
    let group = q
        .group
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|g| format!("group:{g}"));
    let limit = q.limit.unwrap_or(100).min(1000);
    let store = s.store.clone();
    tokio::task::spawn_blocking(move || {
        // Filters apply to the newest 1000 of each direction.
        let scan = if peer.is_some() || kind.is_some() || group.is_some() {
            1000
        } else {
            limit
        };
        let mut out = Vec::new();
        for d in directions {
            for r in store.list(d, scan).map_err(internal)? {
                let obj = store.object(r.id).map_err(internal)?;
                let v = view(r, obj);
                let with = |p: &String| v.peer == *p || v.from.as_ref() == Some(p) || v.to.contains(p);
                if peer.as_ref().is_some_and(|p| !with(p))
                    || kind.is_some_and(|k| v.kind.as_deref() != Some(k))
                    || group
                        .as_ref()
                        .is_some_and(|g| !v.to.iter().any(|t| t == g || t.eq_ignore_ascii_case(g)))
                {
                    continue;
                }
                out.push(v);
            }
        }
        out.sort_by(|a, b| {
            // Soft FIFO: prefer wire_seq when both have it; else arrival time.
            match (a.wire_seq, b.wire_seq) {
                (Some(x), Some(y)) if x != y => x.cmp(&y).reverse(),
                _ => (a.at, a.seq).cmp(&(b.at, b.seq)).reverse(),
            }
        });
        out.truncate(limit);
        annotate_seq_gaps(&mut out);
        Ok(out)
    })
    .await
    .map_err(internal)?
    .map(Json)
}

async fn message(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<MessageDetail>, ApiError> {
    let id = ObjectId(crate::hex::decode_32(&id).map_err(bad)?);
    let store = s.store.clone();
    let found = tokio::task::spawn_blocking(move || {
        let Some(record) = store.record(id)? else {
            return Ok(None);
        };
        let object = store
            .object(id)?
            .ok_or_else(|| hm_store::Error::Corrupt("message has no object".into()))?;
        Ok::<_, hm_store::Error>(Some((record, object)))
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    let Some((record, object)) = found else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such message".into()));
    };
    let raw_bytes = object.len();
    let raw_hex = crate::hex::encode(&object);
    Ok(Json(MessageDetail {
        message: view(record, Some(object)),
        raw_bytes,
        raw_hex,
    }))
}

/// Server-sent events: one `data:` line per change, naming what changed. A
/// page fetches what it shows again when told. A comment line every 15 s
/// keeps idle connections open.
async fn events(State(s): State<AppState>) -> impl IntoResponse {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let rx = s.notify.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let what = match rx.recv().await {
            Ok(what) => what,
            // Missed some: the page fetches everything.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => "all",
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
        };
        Some((Ok::<_, std::convert::Infallible>(Event::default().data(what)), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

#[derive(Deserialize)]
struct SendRequest {
    to: String,
    text: String,
    subject: Option<String>,
    precedence: Option<String>,
    /// `"bulletin"` publishes a group bulletin (`to` is the group name).
    kind: Option<String>,
    /// Alternate to `to` when `kind` is bulletin.
    group: Option<String>,
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

fn admit_bulletin_publish(times: &mut VecDeque<u64>, now: u64) -> bool {
    let hour_ago = now.saturating_sub(3600);
    while times.front().is_some_and(|t| *t < hour_ago) {
        times.pop_front();
    }
    if times.len() >= MAX_BULLETINS_PER_HOUR {
        return false;
    }
    times.push_back(now);
    true
}

async fn send(
    State(s): State<AppState>,
    Json(req): Json<SendRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if req.text.is_empty() {
        return Err(bad("text is empty"));
    }
    let is_bulletin = matches!(req.kind.as_deref(), Some("bulletin")) || req.group.is_some();
    if is_bulletin {
        let group = req
            .group
            .as_deref()
            .or(Some(req.to.as_str()))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad("group name required"))?;
        if req.precedence.as_deref().is_some_and(|p| p != "routine") {
            return Err(bad("bulletins are routine precedence only"));
        }
        let now = unix_now();
        {
            let mut times = s.bulletin_publishes.lock().expect("lock");
            if !admit_bulletin_publish(&mut times, now) {
                return Err(ApiError(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("at most {MAX_BULLETINS_PER_HOUR} bulletins per hour"),
                ));
            }
        }
        let subject = req.subject.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let bundle = build_bulletin(&s.cfg.key, s.cfg.me, group, &req.text, subject)
            .map_err(|e| bad(e.to_string()))?;
        let (id, bytes) = (bundle.id(), bundle.to_vec());
        if bytes.len() > MAX_BULLETIN_BYTES {
            return Err(bad(format!(
                "bulletin too large ({} bytes; max {MAX_BULLETIN_BYTES})",
                bytes.len()
            )));
        }
        let peer = hm_xfer::broadcast_peer();
        let expires_at = Some(bundle.bundle().expires_at());
        let store = s.store.clone();
        tokio::task::spawn_blocking(move || {
            store.enqueue_with(
                id,
                &bytes,
                hm_store::EnqueueOpts {
                    to: peer,
                    precedence: 0,
                    now: unix_now(),
                    wire_seq: None,
                    expires_at,
                },
            )
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
        s.notify.send("message");
        return Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({ "id": id.to_string() })),
        ));
    }
    let to = Callsign::parse(req.to.trim()).map_err(|e| bad(format!("to: {e}")))?;
    let prec = parse_precedence(req.precedence.as_deref())?;
    let subject = req.subject.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let store = s.store.clone();
    let seq = if subject.is_none() {
        let peer = to;
        tokio::task::spawn_blocking(move || store.next_peer_seq(peer))
            .await
            .map_err(internal)?
            .map_err(internal)
            .map(Some)?
    } else {
        None
    };
    let bundle = build_bundle(&s.cfg.key, s.cfg.me, to, &req.text, subject, prec, seq)
        .map_err(|e| bad(e.to_string()))?;
    let (id, bytes) = (bundle.id(), bundle.to_vec());
    let wire_seq = bundle.bundle().seq;
    let expires_at = Some(bundle.bundle().expires_at());
    let store = s.store.clone();
    tokio::task::spawn_blocking(move || {
        store.enqueue_with(
            id,
            &bytes,
            hm_store::EnqueueOpts {
                to,
                precedence: prec.to_u8(),
                now: unix_now(),
                wire_seq,
                expires_at,
            },
        )
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    s.notify.send("message");
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
        Ok(()) => {
            s.notify.send("message");
            Ok(StatusCode::NO_CONTENT)
        }
        Err(hm_store::Error::NotFound) => Err(ApiError(StatusCode::NOT_FOUND, "no such message".into())),
        Err(e) => Err(internal(e)),
    }
}

async fn delete_message(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    let id = ObjectId(crate::hex::decode_32(&id).map_err(bad)?);
    let store = s.store.clone();
    let outcome = tokio::task::spawn_blocking(move || store.delete(id))
        .await
        .map_err(internal)?;
    match outcome {
        Ok(hm_store::DeleteOutcome::Cancelled | hm_store::DeleteOutcome::Deleted) => {
            s.notify.send("message");
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(hm_store::DeleteOutcome::Active) => Err(ApiError(
            StatusCode::CONFLICT,
            "message is still active; cancel queued outbound messages before deleting history".into(),
        )),
        Err(hm_store::Error::NotFound) => Err(ApiError(StatusCode::NOT_FOUND, "no such message".into())),
        Err(error) => Err(internal(error)),
    }
}

#[derive(Serialize)]
struct ConversationDelete {
    deleted: usize,
    active: usize,
}

async fn delete_conversation(
    State(s): State<AppState>,
    Path(peer): Path<String>,
) -> Result<Json<ConversationDelete>, ApiError> {
    let peer = Callsign::parse(peer.trim()).map_err(|error| bad(format!("peer: {error}")))?;
    let store = s.store.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let peer = peer.to_string();
        let mut deleted = 0;
        let mut active = 0;
        for direction in [Direction::In, Direction::Out] {
            for record in store.list(direction, usize::MAX)? {
                let id = record.id;
                let state = record.state;
                let object = store.object(id)?;
                let message = view(record, object);
                let with_peer = message.peer == peer
                    || message.from.as_ref() == Some(&peer)
                    || message.to.contains(&peer);
                if !with_peer || message.kind.as_deref() != Some("Chat") {
                    continue;
                }
                if matches!(state, MessageState::Queued | MessageState::InTransit) {
                    active += 1;
                    continue;
                }
                match store.delete(id)? {
                    hm_store::DeleteOutcome::Deleted => deleted += 1,
                    hm_store::DeleteOutcome::Cancelled | hm_store::DeleteOutcome::Active => active += 1,
                }
            }
        }
        Ok::<_, hm_store::Error>(ConversationDelete { deleted, active })
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    if outcome.deleted > 0 {
        s.notify.send("message");
    }
    Ok(Json(outcome))
}

#[derive(Serialize)]
struct TrustedView {
    station: String,
    key: String,
    note: Option<String>,
}

#[derive(Serialize)]
struct TrustList {
    /// The settings file changes are saved to; `null` when the node has none,
    /// and changes last until it restarts.
    file: Option<String>,
    stations: Vec<TrustedView>,
}

async fn list_trust(State(s): State<AppState>) -> Json<TrustList> {
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
struct TrustRequest {
    line: Option<String>,
    station: Option<String>,
    key: Option<String>,
    note: Option<String>,
}

async fn add_trust(
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
    super::log(format!("trusted {call} (through the API)"));
    Ok((
        StatusCode::CREATED,
        Json(TrustedView {
            station: call.to_string(),
            key: crate::hex::encode(&key.0),
            note: s.live.get().note(call).map(str::to_string),
        }),
    ))
}

async fn remove_trust(
    State(s): State<AppState>,
    Path(station): Path<String>,
) -> Result<StatusCode, ApiError> {
    let call = Callsign::parse(&station).map_err(|e| bad(format!("station: {e}")))?;
    match s.live.remove_trust(call).map_err(internal)? {
        true => {
            super::log(format!("no longer trusting {call} (through the API)"));
            Ok(StatusCode::NO_CONTENT)
        }
        false => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{call} has no entry of its own among the trusted stations"),
        )),
    }
}

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
            max_rounds
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
struct LiveView {
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
struct SettingsView {
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
                        super::arq::Kind::Vara => "vara".into(),
                        super::arq::Kind::Ardop => "ardop".into(),
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

async fn get_settings(State(s): State<AppState>) -> Json<SettingsView> {
    Json(settings_view(&s))
}

async fn change_settings(
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
        super::log("settings changed (through the API)");
    }
    Ok(Json(settings_view(&s)))
}
