use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::AppState;

/// `GET /health` — liveness plus a snapshot of tracked job IDs.
pub async fn handle(State(state): State<AppState>) -> Json<Value> {
    let ids = state.registry.snapshot_ids();
    Json(json!({
        "status": "ok",
        "version": "3",
        "jobs": ids.len(),
        "job_ids": ids,
    }))
}
