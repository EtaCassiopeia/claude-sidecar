use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Errors surfaced to HTTP clients. Each variant maps to a status code and is
/// rendered as `{"error": "..."}`.
#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("command not allowed: {0}")]
    NotAllowed(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("browser bridge failed: {0}")]
    Browser(String),

    #[error("command not found on PATH: {0}")]
    CommandNotFound(String),

    #[error("job not found: {0}")]
    JobNotFound(String),

    #[error("too many concurrent jobs (max {max})")]
    TooManyJobs { max: usize },

    #[error("process spawn failed: {0}")]
    Spawn(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("timeout after {secs}s")]
    Timeout { secs: u64 },

    #[error("internal error: {0}")]
    Internal(String),
}

impl SidecarError {
    fn status(&self) -> StatusCode {
        match self {
            SidecarError::NotAllowed(_) => StatusCode::FORBIDDEN,
            SidecarError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            SidecarError::Browser(_) => StatusCode::BAD_GATEWAY,
            SidecarError::CommandNotFound(_) | SidecarError::JobNotFound(_) => {
                StatusCode::NOT_FOUND
            }
            SidecarError::TooManyJobs { .. } => StatusCode::SERVICE_UNAVAILABLE,
            SidecarError::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            SidecarError::Spawn(_)
            | SidecarError::Io(_)
            | SidecarError::Json(_)
            | SidecarError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for SidecarError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Server-side faults are worth a log line; client faults (4xx) are not.
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}
