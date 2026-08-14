pub mod batch;
pub mod browser;
pub mod exec;
pub mod health;
pub mod jobs;

use axum::{
    routing::{delete, get, post},
    Router,
};

use crate::AppState;

/// Build the application router. Constructed once at startup and shared across
/// all connections — routes are compiled a single time, not per request.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health::handle))
        .route("/exec", post(exec::handle))
        .route("/batch", post(batch::handle))
        .route("/jobs", get(jobs::list).post(jobs::create))
        .route("/jobs/:id/lines", get(jobs::lines))
        .route("/jobs/:id/status", get(jobs::status))
        .route("/jobs/:id/stream", get(jobs::stream))
        .route("/jobs/:id", delete(jobs::cancel))
        .route("/browser/fetch", post(browser::fetch))
        .route("/browser/tab", get(browser::tab))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use tower::ServiceExt; // for `oneshot`

    use super::*;
    use crate::{config::Config, job::JobRegistry};

    fn test_state() -> AppState {
        let config = Arc::new(Config::default());
        let registry = JobRegistry::new(
            config.max_jobs,
            config.max_lines_per_job,
            config.spill_to_disk,
            config.job_ttl_secs,
        );
        AppState { config, registry }
    }

    async fn send(req: Request<Body>) -> StatusCode {
        router(test_state())
            .oneshot(req)
            .await
            .expect("router is infallible")
            .status()
    }

    /// Send a request and return both the status and the parsed JSON body.
    async fn send_json(req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = router(test_state())
            .oneshot(req)
            .await
            .expect("router is infallible");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body readable");
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    fn json_post(uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .expect("valid request")
    }

    #[tokio::test]
    async fn health_is_ok() {
        let req = Request::get("/health").body(Body::empty()).unwrap();
        assert_eq!(send(req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let req = Request::get("/nope").body(Body::empty()).unwrap();
        assert_eq!(send(req).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn interpreter_inline_exec_is_403() {
        let req = json_post(
            "/exec",
            r#"{"cmd":"python3","args":["-c","import os; os.system('id')"]}"#,
        );
        assert_eq!(send(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn disallowed_command_is_403() {
        // `sudo` is not allowlisted; `-n true` is also harmless if it ever runs.
        let req = json_post("/exec", r#"{"cmd":"sudo","args":["-n","true"]}"#);
        assert_eq!(send(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn malformed_json_is_400() {
        let req = json_post("/exec", "not json");
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn browser_fetch_non_http_url_is_400() {
        // URL validation runs before any osascript spawn, so this is safe in CI.
        let req = json_post("/browser/fetch", r#"{"url":"file:///etc/passwd"}"#);
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn browser_fetch_missing_url_is_422() {
        let req = json_post("/browser/fetch", r#"{"no_url":true}"#);
        assert_eq!(send(req).await, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn status_of_missing_job_is_404() {
        let req = Request::get("/jobs/does-not-exist/status")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(req).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn batch_empty_steps_is_400() {
        let req = json_post("/batch", r#"{"steps":[]}"#);
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn batch_disallowed_command_rejects_whole_batch() {
        // `sudo` in step 2 must reject the batch up front (403) — the allowlisted
        // `echo` in step 1 must not run first.
        let req = json_post(
            "/batch",
            r#"{"steps":[{"cmd":"echo","args":["hi"]},{"cmd":"sudo","args":["-n","true"]}]}"#,
        );
        assert_eq!(send(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn batch_interpreter_inline_exec_is_403() {
        let req = json_post(
            "/batch",
            r#"{"steps":[{"cmd":"python3","args":["-c","import os"]}]}"#,
        );
        assert_eq!(send(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn batch_happy_path_runs_all_steps() {
        let req = json_post(
            "/batch",
            r#"{"steps":[{"cmd":"echo","args":["a"]},{"cmd":"echo","args":["b"]}]}"#,
        );
        let (status, body) = send_json(req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert!(body["failed_at"].is_null());
        assert_eq!(body["steps"].as_array().unwrap().len(), 2);
        assert_eq!(body["steps"][0]["stdout"], "a\n");
    }

    #[tokio::test]
    async fn batch_stops_at_first_failure() {
        // `ls` of a missing path exits nonzero; the third step must be skipped.
        let req = json_post(
            "/batch",
            r#"{"steps":[
                {"cmd":"echo","args":["one"]},
                {"cmd":"ls","args":["/definitely/does/not/exist/xyz"]},
                {"cmd":"echo","args":["three"]}
            ]}"#,
        );
        let (status, body) = send_json(req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], false);
        assert_eq!(body["failed_at"], 1);
        // Only the first two steps ran; the third was skipped.
        assert_eq!(body["steps"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn batch_continue_on_error_runs_every_step() {
        let req = json_post(
            "/batch",
            r#"{"continue_on_error":true,"steps":[
                {"cmd":"echo","args":["one"]},
                {"cmd":"ls","args":["/definitely/does/not/exist/xyz"]},
                {"cmd":"echo","args":["three"]}
            ]}"#,
        );
        let (status, body) = send_json(req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], false);
        assert_eq!(body["failed_at"], 1);
        // All three ran despite the middle failure.
        assert_eq!(body["steps"].as_array().unwrap().len(), 3);
        assert_eq!(body["steps"][2]["stdout"], "three\n");
    }
}
