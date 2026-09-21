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
    logger, metrics,
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
    Json(
        state
            .registry
            .snapshot()
            .iter()
            .map(|j| summarize(j))
            .collect(),
    )
}

// ─── POST /jobs ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
    /// Kill the job if it emits nothing for this long. Off unless set — a quiet
    /// job (long link step, silent test run) is not necessarily a wedged one.
    pub idle_timeout_secs: Option<u64>,
    /// Keystrokes to feed the command's stdin up front, for tools that stop on a
    /// confirmation prompt (`"Y\n"`). Omitted means stdin is closed.
    pub input: Option<String>,
    /// Identifies the calling client so a monitor can group jobs by origin. The
    /// sidecar never interprets it — an opaque tag, typically a Claude Code
    /// session id supplied by a hook.
    pub session_id: Option<String>,
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
    super::exec::validate(&req.cmd, &req.args)?;
    let resolved =
        config::resolve(&req.cmd).ok_or_else(|| SidecarError::CommandNotFound(req.cmd.clone()))?;

    logger::log_request("POST", "/jobs", &req.cmd, &req.args, req.cwd.as_deref());

    let job = state.registry.create(
        req.cmd.clone(),
        req.args.clone(),
        req.session_id.filter(|s| !s.trim().is_empty()),
    )?;

    // Captured before `req.args` is moved into `opts` below.
    let metrics_args = req.args.clone();

    let opts = PtyOptions {
        cmd: resolved.to_string_lossy().into_owned(),
        args: req.args,
        cwd: req.cwd.map(PathBuf::from),
        env: req.env,
        cols: req.cols.unwrap_or(DEFAULT_COLS),
        rows: req.rows.unwrap_or(DEFAULT_ROWS),
        timeout: Duration::from_secs(req.timeout_secs.unwrap_or(DEFAULT_JOB_TIMEOUT_SECS)),
        kill_grace: Duration::from_secs(state.config.kill_grace_secs),
        idle_timeout: req
            .idle_timeout_secs
            .filter(|s| *s > 0)
            .map(Duration::from_secs),
        input: req.input,
        verbose: state.config.verbose,
    };

    let job_id = job.id.clone();
    let log_path = format!("/jobs/{job_id}");
    let runner = Arc::clone(&job);
    // Jobs never pass through `EventBus::finish`, so without this the durable
    // metrics would omit exactly the slow work most worth measuring — a build
    // that takes minutes rather than an `/exec` that takes milliseconds.
    let metrics = state.metrics.clone();
    let metrics_cmd = req.cmd.clone();
    let metrics_sub = metrics_args
        .first()
        .and_then(|a| metrics::safe_subcommand(a));
    let metrics_nargs = metrics_args.len() as u32;
    // Read back off the job rather than off `req`: the registry already applied
    // the blank-is-absent rule, so this cannot disagree with what the TUI shows.
    let metrics_session = job.session_id.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        // Wall-clock start, for the metrics row. `Instant` is monotonic and has
        // no epoch, so it cannot answer "which day did this run on" — and a long
        // build started before midnight must be attributed to the day it began,
        // not the day it finished.
        let started_ms = now_ms() as i64;

        // Run the pty inside a task we can join, so a panic in it cannot leave
        // the job Running forever — `finish` is only reachable from here, and a
        // job stuck in Running is never evicted and holds a slot for good.
        let pty_job = Arc::clone(&runner);
        let outcome = match tokio::spawn(pty::run(pty_job, opts)).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::error!(job_id = %runner.id, "job runner died: {e}");
                Outcome::Failed
            }
        };

        // `pty::run` already reports Canceled (with whether the shutdown had to
        // escalate), so nothing to reinterpret here.
        runner.finish(outcome);
        let elapsed = started.elapsed().as_millis();
        logger::log_completion(&log_path, outcome.exit_code(), elapsed);

        if let Some(recorder) = metrics {
            recorder.record(&metrics::CallRecord {
                date: metrics::local_date(started_ms),
                ts: started_ms,
                kind: "job".into(),
                cmd: metrics_cmd,
                sub: metrics_sub,
                nargs: metrics_nargs,
                ms: Some(elapsed as i64),
                exit: outcome.exit_code(),
                // A job that was killed or timed out has no exit code, so the
                // outcome is the only truthful thing to record about it. Named
                // explicitly rather than via `Debug`, whose struct-variant form
                // ("canceled { escalated: false }") would leak Rust syntax into
                // a data column that SQL has to group on.
                error: match outcome {
                    Outcome::Completed { .. } => None,
                    Outcome::Canceled { .. } => Some("canceled".into()),
                    Outcome::TimedOut { .. } => Some("timed_out".into()),
                    Outcome::IdleTimedOut { .. } => Some("idle_timed_out".into()),
                    Outcome::Failed => Some("failed".into()),
                },
                session: metrics_session,
            });
        }
    });

    Ok(Json(CreateJobResponse { job_id }))
}

// ─── GET /jobs/{id}/lines ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LinesQuery {
    #[serde(default)]
    pub from: usize,
    /// Wait up to this many milliseconds for output rather than returning an
    /// empty window immediately. Capped at [`MAX_WAIT_MS`]; `0` (the default)
    /// preserves the original non-blocking behavior.
    #[serde(default)]
    pub wait_ms: u64,
}

/// Ceiling on `wait_ms`. Long enough to make polling cheap, short enough to stay
/// under a client's default HTTP timeout.
const MAX_WAIT_MS: u64 = 30_000;

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
///
/// With `?wait_ms=N` the request blocks until output arrives or the job ends,
/// instead of returning an empty window that the client must re-poll after a
/// sleep. This is what lets callers drop the fixed `sleep` from their poll loop:
/// the wait happens here, where it can end the instant something happens.
pub async fn lines(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LinesQuery>,
) -> Result<Json<LinesResponse>, SidecarError> {
    let job = state.registry.get(&id)?;

    // Subscribe before the first read, so a line landing between the read and
    // the wait cannot be missed. Same ordering discipline as `stream`.
    let mut rx = job.subscribe();
    let mut snapshot = job.read_window(query.from, MAX_LINES_PER_POLL).await;

    let wait = Duration::from_millis(query.wait_ms.min(MAX_WAIT_MS));
    if snapshot.0.is_empty() && !wait.is_zero() && job.state().is_running() {
        // Wake on any event: a new line, or the job finishing. Lag is not an
        // error here — the line buffer is authoritative, so a lagged receiver
        // just means "something happened, go look".
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(JobEvent::Line(_)) | Err(RecvError::Lagged(_))) => {
                    snapshot = job.read_window(query.from, MAX_LINES_PER_POLL).await;
                    if !snapshot.0.is_empty() {
                        break;
                    }
                }
                // Finished, channel closed, or budget spent: re-read once so the
                // response carries any final lines alongside the terminal state.
                Ok(Ok(JobEvent::Finished(_)) | Err(RecvError::Closed)) | Err(_) => {
                    snapshot = job.read_window(query.from, MAX_LINES_PER_POLL).await;
                    break;
                }
            }
        }
    }

    let (lines, next_from, dropped) = snapshot;
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
    /// The client that created the job, when it identified itself. Lets a monitor
    /// group jobs by Claude Code session — the pid cannot, since sessions share
    /// one sidecar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub line_count: usize,
    pub elapsed_ms: u64,
    /// Milliseconds since the job last produced output. A large and growing
    /// value on a running job is the signature of a wedged process — without it,
    /// a job blocked on an unreachable host looks identical to a slow compile.
    pub idle_ms: u64,
    /// Child PID while running, so a wedged job can be inspected with `ps`/`lsof`.
    pub pid: Option<i32>,
    /// How the job ended: `completed`, `timed_out`, `idle_timed_out`, `canceled`,
    /// or `failed`. Absent while running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

fn summarize(job: &crate::job::Job) -> StatusResponse {
    let job_state = job.state();
    StatusResponse {
        job_id: job.id.clone(),
        cmd: job.cmd.clone(),
        args: job.args.clone(),
        session_id: job.session_id.clone(),
        running: job_state.is_running(),
        exit_code: job_state.outcome().and_then(Outcome::exit_code),
        line_count: job.line_count(),
        elapsed_ms: job.elapsed_ms(),
        idle_ms: job.idle_ms(),
        pid: job.pid(),
        outcome: job_state.outcome(),
    }
}

/// A compact snapshot of a job's progress.
pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, SidecarError> {
    let job = state.registry.get(&id)?;
    Ok(Json(summarize(&job)))
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
///
/// Two kinds of loss are reported rather than hidden. Lines evicted before the
/// replay window emit a `gap` event; a subscriber that falls behind the
/// broadcast channel re-reads from the line buffer (which is authoritative)
/// instead of silently skipping whatever it missed.
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
        let mut announced_gap = false;
        while cursor < boundary {
            let want = (boundary - cursor).min(SSE_REPLAY_CHUNK);
            let (lines, next_from, dropped) = job.read_window(cursor, want).await;
            // Lines evicted before this window are unrecoverable. Say so, once,
            // before the first surviving line — otherwise a watcher cannot tell
            // a truncated history from a job that simply started here.
            if dropped > 0 && !announced_gap {
                announced_gap = true;
                yield Ok(gap_event(dropped, next_from.saturating_sub(lines.len())));
            }
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

        // Highest index forwarded so far; also the resync cursor after a lag.
        let mut next = boundary;
        loop {
            match rx.recv().await {
                Ok(JobEvent::Line(line)) => {
                    if line.index >= next {
                        next = line.index + 1;
                        yield Ok(line_event(&line));
                    }
                }
                Ok(JobEvent::Finished(outcome)) => {
                    yield Ok(exit_event(outcome));
                    break;
                }
                Err(RecvError::Lagged(n)) => {
                    // We missed `n` events, but the lines themselves are still in
                    // the line buffer. Re-read from `next` rather than resuming
                    // blind, which would drop them from the stream silently.
                    tracing::warn!(job_id = %id, "sse subscriber lagged by {n} events; resyncing");
                    let target = job.next_index();
                    while next < target {
                        let want = (target - next).min(SSE_REPLAY_CHUNK);
                        let (lines, next_from, dropped) = job.read_window(next, want).await;
                        if dropped > 0 {
                            yield Ok(gap_event(dropped, next_from.saturating_sub(lines.len())));
                        }
                        for line in lines {
                            yield Ok(line_event(&line));
                        }
                        if next_from <= next {
                            break; // no progress; give up on the resync
                        }
                        next = next_from;
                    }
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

/// Announce unrecoverable loss: `dropped` lines before `resume_index` were
/// evicted from the buffer and cannot be replayed.
fn gap_event(dropped: usize, resume_index: usize) -> Event {
    let payload = json!({
        "type": "gap",
        "dropped": dropped,
        "resume_index": resume_index,
        "ts": now_ms(),
    });
    Event::default().data(payload.to_string())
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
/// `canceled` reports that the job was running and a cancel was requested;
/// `signalled` reports whether a signal actually reached the process. They can
/// differ: a cancel arriving before the child has been spawned sets the intent
/// but has nothing to signal yet. Returns 404 if the job ID is not found.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, SidecarError> {
    let job = state.registry.get(&id)?;
    let was_running = job.state().is_running();
    let signalled = if was_running { job.cancel() } else { false };
    Ok(Json(
        json!({ "canceled": was_running, "signalled": signalled, "job_id": id }),
    ))
}
