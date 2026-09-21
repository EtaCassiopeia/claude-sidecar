pub mod batch;
pub mod browser;
pub mod events;
pub mod exec;
pub mod gdocs;
pub mod gdocs_read;
pub mod health;
pub mod jobs;
pub mod stats;

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
        .route("/logs", get(health::logs))
        .route("/events", get(events::stream))
        .route("/stats", get(stats::handle))
        .route("/stats/query", post(stats::query))
        .route("/exec", post(exec::handle))
        .route("/batch", post(batch::handle))
        .route("/jobs", get(jobs::list).post(jobs::create))
        .route("/jobs/:id/lines", get(jobs::lines))
        .route("/jobs/:id/status", get(jobs::status))
        .route("/jobs/:id/stream", get(jobs::stream))
        .route("/jobs/:id", delete(jobs::cancel))
        .route("/browser/fetch", post(browser::fetch))
        .route("/browser/tab", get(browser::tab))
        .route("/gdocs/clipboard", post(gdocs::handle))
        .route("/gdocs/read", post(gdocs_read::read))
        .route("/gdocs/list", get(gdocs_read::list))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use tower::ServiceExt; // for `oneshot`

    use super::*;
    use crate::{config::Config, events::EventBus, job::JobRegistry};

    fn test_state() -> AppState {
        let config = Arc::new(Config::default());
        let registry = JobRegistry::new(
            config.max_jobs,
            config.max_lines_per_job,
            config.spill_to_disk,
            config.job_ttl_secs,
        );
        AppState {
            config,
            registry,
            events: EventBus::new(),
            // These tests assert routing and policy, not storage. A recorder
            // would write real files into the test's home directory.
            metrics: None,
        }
    }

    async fn send(req: Request<Body>) -> StatusCode {
        router(test_state())
            .oneshot(req)
            .await
            .expect("router is infallible")
            .status()
    }

    /// Send a request and return `(status, parsed JSON body)`.
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

    /// Send a request against a *specific* state, so a test can create a job and
    /// then poll it (`send_json` builds a fresh registry per call).
    async fn send_json_with(
        state: AppState,
        req: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let resp = router(state)
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

    /// The endpoint that answers "why did that fail" after the reason has
    /// scrolled off stderr.
    #[tokio::test]
    async fn logs_serves_retained_diagnostics() {
        crate::events::record_diagnostic("ERROR", "test", "a spill write failed".into());
        let (status, body) = send_json(get("/logs")).await;
        assert_eq!(status, StatusCode::OK);
        let found = body["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|d| d["message"] == "a spill write failed");
        assert!(found, "the recorded diagnostic must be served: {body}");
        assert!(body["count"].as_u64().is_some_and(|c| c > 0));
    }

    /// So a client knows there is something worth fetching without polling
    /// `/logs` itself.
    #[tokio::test]
    async fn health_reports_the_diagnostic_count() {
        crate::events::record_diagnostic("WARN", "test", "something odd".into());
        let (_, body) = send_json(get("/health")).await;
        assert!(
            body["diagnostics"].as_u64().is_some_and(|c| c > 0),
            "{body}"
        );
    }

    #[tokio::test]
    async fn denied_command_is_403() {
        // `sudo` is the one default denial; `-n true` is harmless if it ever runs.
        let req = json_post("/exec", r#"{"cmd":"sudo","args":["-n","true"]}"#);
        assert_eq!(send(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn denial_is_enforced_on_the_job_path_too() {
        // `/jobs` must share the same gate as `/exec`, not its own copy.
        let req = json_post("/jobs", r#"{"cmd":"sudo","args":["-n","true"]}"#);
        assert_eq!(send(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn interpreter_inline_exec_is_permitted() {
        // Deliberate reversal: with no allowlist there is nothing for an
        // interpreter-flag check to protect, so `-c` is a normal invocation.
        let req = json_post("/exec", r#"{"cmd":"python3","args":["-c","print(1)"]}"#);
        assert_eq!(send(req).await, StatusCode::OK);
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

    // Every /gdocs/clipboard test passes `set_clipboard: false`, so validation
    // and rendering are exercised without spawning osascript — the same
    // reject-before-spawn discipline the browser tests follow.

    #[tokio::test]
    async fn gdocs_requires_exactly_one_input_is_400() {
        let neither = json_post("/gdocs/clipboard", r#"{"set_clipboard":false}"#);
        assert_eq!(send(neither).await, StatusCode::BAD_REQUEST);

        let both = json_post(
            "/gdocs/clipboard",
            r##"{"path":"/tmp/a.md","markdown":"# x","set_clipboard":false}"##,
        );
        assert_eq!(send(both).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_relative_path_is_400() {
        let req = json_post(
            "/gdocs/clipboard",
            r#"{"path":"relative.md","set_clipboard":false}"#,
        );
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_missing_markdown_file_is_400() {
        let req = json_post(
            "/gdocs/clipboard",
            r#"{"path":"/nonexistent/nope.md","set_clipboard":false}"#,
        );
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_non_html_out_path_is_400() {
        let req = json_post(
            "/gdocs/clipboard",
            r##"{"markdown":"# x","out_path":"/tmp/out.txt","set_clipboard":false}"##,
        );
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_empty_markdown_is_400() {
        let req = json_post(
            "/gdocs/clipboard",
            r#"{"markdown":"   \n ","set_clipboard":false}"#,
        );
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_no_input_fields_is_400() {
        // `path`/`markdown` are both Option, so an unrelated body deserializes
        // fine and fails the "exactly one input" check instead of at serde.
        let req = json_post("/gdocs/clipboard", r#"{"nonsense":true}"#);
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_unknown_theme_is_422() {
        let req = json_post(
            "/gdocs/clipboard",
            r##"{"markdown":"# x","theme":"midnight","set_clipboard":false}"##,
        );
        assert_eq!(send(req).await, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn gdocs_inline_markdown_renders_without_clipboard() {
        let req = json_post(
            "/gdocs/clipboard",
            r##"{"markdown":"# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```scala\ncase class A(b: Int)\n```\n\n```gherkin\nGiven x\n```\n","set_clipboard":false}"##,
        );
        let (status, body) = send_json(req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["clipboard_set"], false);
        assert_eq!(body["clipboard_flavor"], serde_json::Value::Null);
        assert_eq!(body["code_blocks"], 2);
        assert_eq!(body["tables"], 1);
        assert_eq!(body["languages"], serde_json::json!(["Scala"]));
        assert_eq!(
            body["unhighlighted_languages"],
            serde_json::json!(["gherkin"])
        );

        // Verify by reading the artifact, not by trusting the summary — the
        // previous implementation of this converter reported success while
        // emitting broken attributes.
        let path = body["html_path"].as_str().expect("html_path present");
        let html = std::fs::read_to_string(path).expect("html file written");
        assert!(html.contains("font-size:20pt"), "heading style inlined");
        assert!(
            html.contains("border-collapse:collapse"),
            "table style inlined"
        );
        assert!(html.contains("color:#"), "scala tokens colorized");
        assert!(
            !html.contains("aria-hidden"),
            "no per-line anchor artifacts"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn gdocs_writes_to_a_caller_specified_out_path() {
        let out = std::env::temp_dir().join("gdocs-route-out.html");
        let body = format!(
            r##"{{"markdown":"# x","out_path":"{}","set_clipboard":false}}"##,
            out.display()
        );
        let (status, value) = send_json(json_post("/gdocs/clipboard", &body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["html_path"], out.to_string_lossy().as_ref());
        assert!(out.exists());
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn gdocs_mermaid_fence_warns_instead_of_dropping_the_diagram() {
        let req = json_post(
            "/gdocs/clipboard",
            r##"{"markdown":"```mermaid\ngraph TD\n  A-->B\n```\n","set_clipboard":false}"##,
        );
        let (status, body) = send_json(req).await;
        assert_eq!(status, StatusCode::OK);
        let warnings = body["warnings"].as_array().expect("warnings array");
        assert_eq!(warnings.len(), 1, "the degradation must be reported");
        let path = body["html_path"].as_str().unwrap();
        let html = std::fs::read_to_string(path).unwrap();
        assert!(html.contains("graph TD"), "diagram source must survive");
        let _ = std::fs::remove_file(path);
    }

    // Every /gdocs/read test below names a document that is rejected during
    // validation, so no osascript is ever spawned — the same
    // reject-before-spawn discipline the browser tests follow.

    #[tokio::test]
    async fn gdocs_read_requires_a_selector_is_400() {
        let req = json_post("/gdocs/read", r#"{}"#);
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_read_multiple_selectors_is_400() {
        let req = json_post(
            "/gdocs/read",
            r##"{"doc_id":"1HDS9lW9I81gXKUnTOunAkQt4KVCNx044","title":"Design"}"##,
        );
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    /// An id that could break out of the generated JavaScript must be refused
    /// before Chrome is involved at all.
    #[tokio::test]
    async fn gdocs_read_injection_shaped_doc_id_is_400() {
        let req = json_post(
            "/gdocs/read",
            r##"{"doc_id":"1AAAA'+alert(1)+'AAAAAAAAAA"}"##,
        );
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_read_non_docs_url_is_400() {
        let req = json_post(
            "/gdocs/read",
            r##"{"url":"https://example.com/document/d/1AAAAAAAAAAAAAAAAAAAA/edit"}"##,
        );
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    /// Slides has no Markdown export; asking for it is caught as a bad request
    /// rather than becoming a confusing 404 from Google.
    #[tokio::test]
    async fn gdocs_read_unsupported_format_for_kind_is_400() {
        let req = json_post(
            "/gdocs/read",
            r##"{"url":"https://docs.google.com/presentation/d/1AAAAAAAAAAAAAAAAAAAA/edit","format":"md"}"##,
        );
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gdocs_read_unknown_format_is_422() {
        let req = json_post(
            "/gdocs/read",
            r##"{"doc_id":"1HDS9lW9I81gXKUnTOunAkQt4KVCNx044","format":"pdf"}"##,
        );
        assert_eq!(send(req).await, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn status_of_missing_job_is_404() {
        let req = Request::get("/jobs/does-not-exist/status")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(req).await, StatusCode::NOT_FOUND);
    }

    // ─── /events ──────────────────────────────────────────────────────────────

    /// Read the SSE body until `limit` bytes or the stream ends, so a test can
    /// assert on the replay without hanging on a stream that never closes.
    async fn read_sse(state: AppState, limit: usize) -> String {
        use http_body_util::BodyExt;

        let resp = router(state)
            .oneshot(get("/events"))
            .await
            .expect("router is infallible");
        assert_eq!(resp.status(), StatusCode::OK);

        let mut body = resp.into_body();
        let mut out = String::new();
        while out.len() < limit {
            match tokio::time::timeout(Duration::from_secs(2), body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        out.push_str(&String::from_utf8_lossy(data));
                    }
                }
                // Stream ended, errored, or went quiet — the replay is complete.
                _ => break,
            }
        }
        out
    }

    /// The replay is what lets a watcher reach current state without the poll it
    /// is meant to replace.
    #[tokio::test]
    async fn events_replays_current_jobs_then_signals_ready() {
        let (state, job) = state_with_job();
        job.push_line("compiling".into());
        let body = read_sse(state, 4096).await;

        assert!(body.contains(&job.id), "the tracked job must be replayed");
        assert!(body.contains("job_created"), "{body}");
        assert!(body.contains("job_progress"), "{body}");
        assert!(
            body.contains(r#""type":"ready""#),
            "the end of replay must be marked: {body}"
        );
    }

    /// The gap this endpoint exists to close: `/exec` traffic touches no job, so
    /// nothing else would ever tell a watcher it happened.
    #[tokio::test]
    async fn events_reports_one_shot_calls_that_touch_no_job() {
        let state = test_state();
        let id = state.events.start(
            crate::events::ActivityKind::Exec,
            "git",
            &["status".into()],
            None,
        );
        state.events.finish(id, Some(0), None);

        let body = read_sse(state, 4096).await;
        assert!(body.contains("activity_finished"), "{body}");
        assert!(body.contains("git"), "{body}");
    }

    /// A denied command is exactly the sort of thing a monitor should surface, so
    /// the record must be created before the policy gate, not after it.
    #[tokio::test]
    async fn a_denied_exec_is_still_recorded_as_activity() {
        let state = test_state();
        let resp = router(state.clone())
            .oneshot(json_post("/exec", r#"{"cmd":"sudo","args":["-n","true"]}"#))
            .await
            .expect("router is infallible");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let body = read_sse(state, 4096).await;
        assert!(
            body.contains("sudo"),
            "the denied call must be visible: {body}"
        );
        assert!(body.contains("activity_finished"), "{body}");
    }

    /// Every step is its own record: a step is what runs and what fails, and a
    /// batch-level record would hide which one is executing.
    #[tokio::test]
    async fn batch_records_each_step_separately() {
        let state = test_state();
        let resp = router(state.clone())
            .oneshot(json_post(
                "/batch",
                r#"{"steps":[{"cmd":"echo","args":["one"]},{"cmd":"echo","args":["two"]}]}"#,
            ))
            .await
            .expect("router is infallible");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = read_sse(state, 8192).await;
        assert!(body.contains("one"), "{body}");
        assert!(body.contains("two"), "{body}");
    }

    #[tokio::test]
    async fn batch_empty_steps_is_400() {
        let req = json_post("/batch", r#"{"steps":[]}"#);
        assert_eq!(send(req).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn batch_denied_step_fails_whole_batch_403() {
        // Pre-flight validation rejects the batch before any step runs, so the
        // permitted `echo` must NOT execute.
        let req = json_post(
            "/batch",
            r#"{"steps":[{"cmd":"echo","args":["hi"]},{"cmd":"sudo","args":["-n","true"]}]}"#,
        );
        assert_eq!(send(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn batch_runs_in_order_and_stops_on_failure() {
        // Step 2 (`ls` of a nonexistent path) exits non-zero, so step 3 must not run.
        let req = json_post(
            "/batch",
            r#"{"steps":[
                {"cmd":"echo","args":["one"]},
                {"cmd":"ls","args":["/no/such/path/xyzzy"]},
                {"cmd":"echo","args":["three"]}
            ]}"#,
        );
        let (status, body) = send_json(req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["aborted"], true);
        let steps = body["steps"].as_array().expect("steps array");
        assert_eq!(steps.len(), 2, "third step must not run after failure");
        assert_eq!(steps[0]["exit_code"], 0);
        assert_ne!(steps[1]["exit_code"], 0);
    }

    #[tokio::test]
    async fn batch_continue_on_error_runs_all_steps() {
        let req = json_post(
            "/batch",
            r#"{"continue_on_error":true,"steps":[
                {"cmd":"ls","args":["/no/such/path/xyzzy"]},
                {"cmd":"echo","args":["still runs"]}
            ]}"#,
        );
        let (status, body) = send_json(req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["aborted"], false);
        let steps = body["steps"].as_array().expect("steps array");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1]["exit_code"], 0);
    }

    // ─── Option 1: long-poll and cancel honesty ───────────────────────────────

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri)
            .body(Body::empty())
            .expect("valid request")
    }

    /// Create a job directly in the registry and return `(state, job)`, so a test
    /// can drive `/lines` against a job whose output it controls.
    fn state_with_job() -> (AppState, std::sync::Arc<crate::job::Job>) {
        let state = test_state();
        let job = state
            .registry
            .create("echo".into(), vec![], None)
            .expect("registry has capacity");
        (state, job)
    }

    #[tokio::test]
    async fn lines_without_wait_returns_immediately() {
        // Backward compatibility: no `wait_ms` must not block on a running job
        // that has produced nothing.
        let (state, _job) = state_with_job();
        let id = state.registry.snapshot_ids().pop().expect("one job");
        let started = std::time::Instant::now();
        let (status, body) = send_json_with(state, get(&format!("/jobs/{id}/lines?from=0"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["running"], true);
        assert!(body["lines"].as_array().expect("array").is_empty());
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "should not have blocked"
        );
    }

    #[tokio::test]
    async fn lines_wait_returns_early_when_a_line_arrives() {
        let (state, job) = state_with_job();
        let id = job.id.clone();
        let writer = std::sync::Arc::clone(&job);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            writer.push_line("hello".into());
        });

        let started = std::time::Instant::now();
        let (status, body) = send_json_with(
            state,
            get(&format!("/jobs/{id}/lines?from=0&wait_ms=10000")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["lines"][0]["text"], "hello");
        // Woken by the line, not by the 10s budget.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?}, should have woken on the line",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn lines_wait_returns_early_when_the_job_finishes() {
        // The silent-completion case: a job that ends without emitting anything
        // must still wake the waiter, or the client burns the whole budget.
        let (state, job) = state_with_job();
        let id = job.id.clone();
        let finisher = std::sync::Arc::clone(&job);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            finisher.finish(crate::job::Outcome::Completed { exit_code: 0 });
        });

        let started = std::time::Instant::now();
        let (status, body) = send_json_with(
            state,
            get(&format!("/jobs/{id}/lines?from=0&wait_ms=10000")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["running"], false);
        assert_eq!(body["exit_code"], 0);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?}, should have woken on Finished",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn lines_wait_gives_up_after_the_budget() {
        // Timing out with zero lines is a valid response; the cursor must not move.
        let (state, job) = state_with_job();
        let id = job.id.clone();
        let (status, body) =
            send_json_with(state, get(&format!("/jobs/{id}/lines?from=0&wait_ms=150"))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["lines"].as_array().expect("array").is_empty());
        assert_eq!(body["next_from"], 0);
        assert_eq!(body["running"], true);
    }

    #[tokio::test]
    async fn lines_wait_skipped_when_output_already_available() {
        let (state, job) = state_with_job();
        let id = job.id.clone();
        job.push_line("already here".into());
        let started = std::time::Instant::now();
        let (_, body) = send_json_with(
            state,
            get(&format!("/jobs/{id}/lines?from=0&wait_ms=10000")),
        )
        .await;
        assert_eq!(body["lines"][0]["text"], "already here");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn cancel_reports_not_signalled_before_the_child_spawns() {
        // A job with no kill callback armed yet stands in for the spawn window:
        // the cancel is recorded, but nothing was signalled — and the response
        // must say so rather than claiming success.
        let (state, job) = state_with_job();
        let id = job.id.clone();
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/jobs/{id}"))
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = send_json_with(state, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["canceled"], true);
        assert_eq!(body["signalled"], false);
    }

    #[tokio::test]
    async fn cancel_reports_signalled_once_armed() {
        let (state, job) = state_with_job();
        let id = job.id.clone();
        job.arm_kill(|| true);
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/jobs/{id}"))
            .body(Body::empty())
            .expect("valid request");
        let (_, body) = send_json_with(state, req).await;
        assert_eq!(body["canceled"], true);
        assert_eq!(body["signalled"], true);
    }

    #[tokio::test]
    async fn cancel_of_finished_job_is_neither_canceled_nor_signalled() {
        let (state, job) = state_with_job();
        let id = job.id.clone();
        job.finish(crate::job::Outcome::Completed { exit_code: 0 });
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/jobs/{id}"))
            .body(Body::empty())
            .expect("valid request");
        let (_, body) = send_json_with(state, req).await;
        assert_eq!(body["canceled"], false);
        assert_eq!(body["signalled"], false);
    }
}
