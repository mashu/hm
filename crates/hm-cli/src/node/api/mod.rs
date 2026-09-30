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
//! | GET, POST | `/api/trust` | the stations whose keys this station knows; add one |
//! | DELETE | `/api/trust/{station}` | forget a station's key |
//! | GET, PATCH | `/api/settings` | settings now, and changing those that may change |

mod messages;
mod settings;
mod status;
mod trust;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use hm_store::Store;

use super::live::LiveConfig;
use super::{NodeConfig, Notify, Status};

use messages::{delete_conversation, delete_message, events, mark_read, message, messages, send};
use settings::{change_settings, get_settings};
use status::status;
use trust::{add_trust, list_trust, remove_trust};

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

const INDEX: &str = include_str!("../index.html");

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(INDEX),
    )
}
