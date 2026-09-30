//! Messages: listing, detail, sending, marking read, deleting, and the
//! event stream that says what changed.

use std::collections::{BTreeMap, VecDeque};

use hm_bundle::{Address, Opened, Precedence};
use hm_store::{Direction, Record, State as MessageState};
use hm_wire::{Callsign, ObjectId};

use crate::station::{build_bulletin, build_bundle, unix_now, MAX_BULLETINS_PER_HOUR, MAX_BULLETIN_BYTES};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{bad, internal, ApiError, AppState};

#[derive(Deserialize)]
pub(super) struct ListQuery {
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
pub(super) struct MessageDetail {
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

pub(super) async fn messages(
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

pub(super) async fn message(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<MessageDetail>, ApiError> {
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
pub(super) async fn events(State(s): State<AppState>) -> impl IntoResponse {
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
pub(super) struct SendRequest {
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

pub(super) async fn send(
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

pub(super) async fn mark_read(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
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

pub(super) async fn delete_message(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
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
pub(super) struct ConversationDelete {
    deleted: usize,
    active: usize,
}

pub(super) async fn delete_conversation(
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
