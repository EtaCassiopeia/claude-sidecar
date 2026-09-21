use std::time::Instant;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{error::SidecarError, events::ActivityKind, logger, routes::exec, AppState};

/// One command in a batch. Mirrors the `/exec` request shape (no shell, args as
/// an explicit vector), with an optional per-step `cwd` that overrides the
/// batch-level default.
#[derive(Debug, Deserialize)]
pub struct BatchStep {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

/// `POST /batch` — an ordered list of commands run in sequence in one call.
#[derive(Debug, Deserialize)]
pub struct BatchRequest {
    pub steps: Vec<BatchStep>,
    /// Working directory applied to every step that does not set its own `cwd`.
    pub cwd: Option<String>,
    /// When true, keep running after a step fails (non-zero exit or spawn/timeout
    /// error). Default: stop at the first failure.
    #[serde(default)]
    pub continue_on_error: bool,
    /// Claude Code session on whose behalf the batch runs. Applied to every step,
    /// since a batch is one caller's request. See [`super::exec::ExecRequest`].
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchStepResult {
    pub cmd: String,
    pub args: Vec<String>,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// Present only when the step failed to run at all (timeout, spawn error)
    /// rather than exiting with a code. `exit_code` is `-1` in that case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub steps: Vec<BatchStepResult>,
    /// True when execution stopped early because a step failed and
    /// `continue_on_error` was not set. The `steps` array then holds only the
    /// steps that actually ran.
    pub aborted: bool,
}

/// `POST /batch` — validate every step against the allowlist up front, then run
/// them in order, stopping at the first failure unless `continue_on_error`.
///
/// There is deliberately no value substitution between steps and no shell: each
/// step goes through exactly the same allowlist + argument gate as `/exec`. If
/// step 2 needs a value produced by step 1, read it from the response and build
/// step 2 explicitly in a second call.
pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, SidecarError> {
    if req.steps.is_empty() {
        return Err(SidecarError::InvalidRequest(
            "batch requires at least one step".into(),
        ));
    }

    // Pre-flight: validate the whole batch before running anything, so a
    // disallowed step never executes after side-effecting steps have already
    // run. A single bad step fails the entire request with 403.
    for step in &req.steps {
        exec::validate(&step.cmd, &step.args)?;
    }

    let verbose = state.config.verbose;
    let mut results = Vec::with_capacity(req.steps.len());
    let mut aborted = false;
    // Blank is treated as absent, as on `/exec`.
    let session = req
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    for step in &req.steps {
        let cwd = step.cwd.as_deref().or(req.cwd.as_deref());
        let timeout_secs = step.timeout_secs.unwrap_or(60);
        logger::log_request("POST", "/batch", &step.cmd, &step.args, cwd);
        let started = Instant::now();
        // Per step, not per batch: a step is what runs and what fails, and a
        // batch-level record would hide which one is currently executing.
        let activity = state.events.start_for_session(
            ActivityKind::Batch,
            &step.cmd,
            &step.args,
            cwd,
            session,
        );

        let outcome =
            exec::run_command(&step.cmd, &step.args, cwd, timeout_secs, &step.env, verbose).await;

        let result = match outcome {
            Ok(r) => {
                logger::log_completion("/batch", Some(r.exit_code), started.elapsed().as_millis());
                state.events.finish(activity, Some(r.exit_code), None);
                BatchStepResult {
                    cmd: step.cmd.clone(),
                    args: step.args.clone(),
                    stdout: r.stdout,
                    stderr: r.stderr,
                    exit_code: r.exit_code,
                    error: None,
                }
            }
            Err(e) => {
                logger::log_completion("/batch", None, started.elapsed().as_millis());
                state.events.finish(activity, None, Some(e.to_string()));
                BatchStepResult {
                    cmd: step.cmd.clone(),
                    args: step.args.clone(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: -1,
                    error: Some(e.to_string()),
                }
            }
        };

        let failed = result.exit_code != 0 || result.error.is_some();
        results.push(result);
        if failed && !req.continue_on_error {
            aborted = true;
            break;
        }
    }

    Ok(Json(BatchResponse {
        steps: results,
        aborted,
    }))
}
