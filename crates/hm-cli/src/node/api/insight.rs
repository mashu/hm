//! `GET /api/insight`: what the node knows now (see [`hm_node::insight`]):
//! every station heard of, what is believed of each path and custodian, the
//! channel, how the chances given have come true, the latest evidence, and
//! why each message goes or waits. Asked of the coordinator, which owns the
//! node, and computed when asked.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use hm_node::insight::Insight;
use tokio::sync::oneshot;

use super::{ApiError, AppState};
use crate::node::coordinator::Query;

pub(super) async fn insight(State(s): State<AppState>) -> Result<Json<Insight>, ApiError> {
    let gone = || ApiError(StatusCode::SERVICE_UNAVAILABLE, "the node is not running".into());
    let (answer, answered) = oneshot::channel();
    s.node.send(Query::Insight(answer)).await.map_err(|_| gone())?;
    answered.await.map(Json).map_err(|_| gone())
}
