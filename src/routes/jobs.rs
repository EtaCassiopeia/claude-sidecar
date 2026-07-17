use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{Path, Query, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::{
    config,
    error::SidecarError,
    job::{now_ms, JobEvent, JobLine, Outcome},
    logger,
    pty::{self, PtyOptions},
    AppState,
};

/// Cap the number of lines returned by a single poll so a huge backlog can't
/// produce a multi-megabyte response. Clients page forward via `next_from`.
const MAX_LINES_PER_POLL: usize = 500;

const DEFAULT_JOB_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_COLS: u16 = 220;
const DEFAULT_ROWS: u16 = 50;

// ─── GET /jobs ────────────────────────────────────────────────────────────────

/// Return a summary of every tracked job (running and recently finished).
pub async fn list(State(state): State<AppState>) -> Json<Vec<StatusResponse>> {
    let jobs = state.registry.snapshot();
    let mut summaries = Vec::with_capacity(jobs.len());
    for job in jobs {
        let job_state = job.state();
        summaries.push(StatusResponse {
            job_id: job.id.clone(),
            cmd: job.cmd.clone(),
            args: job.args.clone(),
            running: job_state.is_running(),
            exit_code: job_state.outcome().and_then(Outcome::exit_code),
            line_count: job.line_count(),
            elapsed_ms: job.elapsed_ms(),
        });
    }
    Json(summaries)
}

// ─── POST /jobs ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Serialize)]
pub struct CreateJobResponse {
    pub job_id: String,
}

/// Spawn a long-running command and return its job ID immediately. Output is
/// collected in the background; clients poll `/lines` or watch `/stream`.
pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateJobRequest>,
) -> Result<Json<CreateJobResponse>, SidecarError> {
    if !config::is_allowed(&req.cmd) {
        return Err(SidecarError::NotAllowed(req.cmd));
    }
    if let Err(reason) = config::check_args(&req.cmd, &req.args) {
        return Err(SidecarError::NotAllowed(reason));
    }
    let resolved =
        config::resolve(&req.cmd).ok_or_else(|| SidecarError::CommandNotFound(req.cmd.clone()))?;

    logger::log_request("POST", "/jobs", &req.cmd, &req.args, req.cwd.as_deref());

    let job = state.registry.create(req.cmd.clone(), req.args.clone())?;

    let opts = PtyOptions {
        cmd: resolved.to_string_lossy().into_owned(),
        args: req.args,
        cwd: req.cwd.map(PathBuf::from),
        env: req.env,
        cols: req.cols.unwrap_or(DEFAULT_COLS),
        rows: req.rows.unwrap_or(DEFAULT_ROWS),
        timeout: Duration::from_secs(req.timeout_secs.unwrap_or(DEFAULT_JOB_TIMEOUT_SECS)),
        verbose: state.config.verbose,
    };

    let job_id = job.id.clone();
    let log_path = format!("/jobs/{job_id}");
    let runner = Arc::clone(&job);
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let outcome = pty::run(Arc::clone(&runner), opts).await;
        // If the job was canceled via DELETE /jobs/:id, report Canceled regardless
        // of what the underlying process returned (likely SIGKILL -> Failed/Completed(-1)).
        let outcome = if runner.was_canceled() {
            Outcome::Canceled
        } else {
            outcome
        };
        runner.finish(outcome);
        logger::log_completion(
            &log_path,
            outcome.exit_code(),
            started.elapsed().as_millis(),
        );
    });

    Ok(Json(CreateJobResponse { job_id }))
}

// ─── GET /jobs/{id}/lines ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LinesQuery {
    #[serde(default)]
    pub from: usize,
}

#[derive(Debug, Serialize)]
pub struct LinesResponse {
    pub lines: Vec<Arc<JobLine>>,
    pub next_from: usize,
    /// Lines evicted before this window because the per-job buffer cap was hit.
    /// Non-zero means the client fell behind a very chatty job and some output
    /// is gone; `next_from` will have jumped past the gap.
    pub dropped: usize,
    pub running: bool,
    pub exit_code: Option<i32>,
}

/// Return buffered lines starting at `?from=N`, plus the cursor to poll next.
pub async fn lines(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LinesQuery>,
) -> Result<Json<LinesResponse>, SidecarError> {
    let job = state.registry.get(&id)?;
    let (lines, next_from, dropped) = job.read_window(query.from, MAX_LINES_PER_POLL).await;
    let state = job.state();
    Ok(Json(LinesResponse {
        lines,
        next_from,
        dropped,
        running: state.is_running(),
        exit_code: state.outcome().and_then(Outcome::exit_code),
    }))
}

// ─── GET /jobs/{id}/status ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub job_id: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub line_count: usize,
    pub elapsed_ms: u64,
}

/// A compact snapshot of a job's progress.
pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, SidecarError> {
    let job = state.registry.get(&id)?;
    let job_state = job.state();
    Ok(Json(StatusResponse {
        job_id: job.id.clone(),
        cmd: job.cmd.clone(),
        args: job.args.clone(),
        running: job_state.is_running(),
        exit_code: job_state.outcome().and_then(Outcome::exit_code),
        line_count: job.line_count(),
        elapsed_ms: job.elapsed_ms(),
    }))
}

// ─── GET /jobs/{id}/stream (SSE) ──────────────────────────────────────────────

/// Page size for replaying a job's backlog to a newly-attached SSE subscriber.
/// Larger than a poll page since it's a one-shot catch-up — this bounds the
/// number of disk round-trips when replaying a spilled history.
const SSE_REPLAY_CHUNK: usize = 1000;

/// Server-Sent Events stream of a job's output, for humans watching live.
///
/// Correctness of replay + live handoff: we subscribe *before* reading the
/// boundary (the next logical index), so no line can slip through the gap. The
/// full history `[0, boundary)` is replayed first — paged through the line
/// buffer, which transparently reads spilled lines back from disk — then live
/// `Line` events at or past the boundary are forwarded (earlier duplicates are
/// filtered). A terminal `Finished` event, or an already-terminal state at
/// subscribe time, emits an `exit` event and closes the stream promptly.
pub async fn stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, SidecarError> {
    let job = state.registry.get(&id)?;

    let mut rx = job.subscribe();
    let boundary = job.next_index();
    let start_state = job.state();

    let stream = async_stream::stream! {
        // Replay history [0, boundary): spilled lines from disk (if any) plus the
        // in-memory tail. In no-spill mode this yields just the retained tail,
        // since older lines were dropped.
        let mut cursor = 0;
        while cursor < boundary {
            let want = (boundary - cursor).min(SSE_REPLAY_CHUNK);
            let (lines, next_from, _dropped) = job.read_window(cursor, want).await;
            for line in lines {
                yield Ok::<Event, Infallible>(line_event(&line));
            }
            if next_from <= cursor {
                break; // no forward progress (e.g. a disk read error) — stop replay
            }
            cursor = next_from;
        }

        // Already finished before we subscribed: emit exit and stop, since no
        // further broadcast will arrive.
        if let Some(outcome) = start_state.outcome() {
            yield Ok(exit_event(outcome));
            return;
        }

        loop {
            match rx.recv().await {
                Ok(JobEvent::Line(line)) => {
                    if line.index >= boundary {
                        yield Ok(line_event(&line));
                    }
                }
                Ok(JobEvent::Finished(outcome)) => {
                    yield Ok(exit_event(outcome));
                    break;
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(job_id = %id, "sse subscriber lagged by {n} events");
                }
                Err(RecvError::Closed) => break,
            }
        }
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn line_event(line: &JobLine) -> Event {
    // JobLine always serializes; fall back to an empty object on the impossible
    // error rather than panicking.
    Event::default()
        .json_data(line)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

fn exit_event(outcome: Outcome) -> Event {
    let payload = json!({
        "type": "exit",
        "outcome": outcome,
        "exit_code": outcome.exit_code(),
        "ts": now_ms(),
    });
    Event::default().data(payload.to_string())
}

// ─── DELETE /jobs/:id ─────────────────────────────────────────────────────────

/// Cancel a running job by sending SIGKILL to its process group.
///
/// Returns 200 with `{"canceled": true}` if the job was running and the signal
/// was sent, or 200 with `{"canceled": false}` if the job had already finished.
/// Returns 404 if the job ID is not found.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, SidecarError> {
    let job = state.registry.get(&id)?;
    let was_running = job.state().is_running();
    if was_running {
        job.cancel();
    }
    Ok(Json(json!({ "canceled": was_running, "job_id": id })))
}
