use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::{events, AppState};

/// `GET /health` — liveness plus a snapshot of tracked job IDs.
pub async fn handle(State(state): State<AppState>) -> Json<Value> {
    let ids = state.registry.snapshot_ids();
    Json(json!({
        "status": "ok",
        "version": "3",
        "jobs": ids.len(),
        "job_ids": ids,
        // So a client can tell there is something worth fetching from /logs
        // without polling the endpoint itself.
        "diagnostics": events::diagnostics().len(),
    }))
}

/// `GET /logs` — the sidecar's own retained warnings and errors.
///
/// These otherwise only reach stderr, which is unreachable for an autostarted
/// daemon whose launching terminal is long gone. This is the endpoint to check
/// when something failed and the reason has scrolled away.
pub async fn logs() -> Json<Value> {
    let diagnostics = events::diagnostics();
    Json(json!({
        "count": diagnostics.len(),
        "diagnostics": diagnostics,
    }))
}
