//! `GET /stats` and `POST /stats/query` — the durable metrics store, read back.
//!
//! The daemon holds the only handle to the Parquet archive it writes, so these
//! endpoints are how anything else reads it: the TUI's overlay fetches the
//! aggregates, and `claude-sidecar query` posts SQL.

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{error::SidecarError, AppState};

/// How many daily buckets the overlay chart shows.
const DEFAULT_DAYS: usize = 7;

/// Bound on an ad-hoc query's result text, so a `SELECT *` over a year of
/// history cannot buffer unboundedly into a response.
const MAX_QUERY_CHARS: usize = 200_000;

/// `GET /stats` — daily call counts and per-command aggregates.
pub async fn handle(State(state): State<AppState>) -> Result<Json<Value>, SidecarError> {
    let Some(metrics) = state.metrics.as_ref() else {
        return Err(SidecarError::Internal(
            "metrics are disabled: no writable metrics directory".into(),
        ));
    };
    let snapshot = metrics
        .snapshot(DEFAULT_DAYS)
        .await
        .map_err(|e| SidecarError::Internal(format!("stats: {e}")))?;
    Ok(Json(json!(snapshot)))
}

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub sql: String,
}

/// `POST /stats/query` — run SQL against the `calls` view.
///
/// Read-only by construction: DataFusion is pointed at the files but has no
/// registered catalog it can write to, and the statement runs against a view
/// rather than the underlying tables.
pub async fn query(
    State(state): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<Value>, SidecarError> {
    let Some(metrics) = state.metrics.as_ref() else {
        return Err(SidecarError::Internal(
            "metrics are disabled: no writable metrics directory".into(),
        ));
    };
    if req.sql.trim().is_empty() {
        return Err(SidecarError::InvalidRequest("sql must not be empty".into()));
    }
    // A failed query is usually the user's SQL being wrong, which is a 400
    // carrying the engine's own message — far more useful than a generic 500.
    let mut table = metrics
        .query(&req.sql)
        .await
        .map_err(|e| SidecarError::InvalidRequest(format!("{e}")))?;

    let truncated = table.chars().count() > MAX_QUERY_CHARS;
    if truncated {
        table = table.chars().take(MAX_QUERY_CHARS).collect();
    }
    Ok(Json(json!({
        "table": table,
        "truncated": truncated,
    })))
}
