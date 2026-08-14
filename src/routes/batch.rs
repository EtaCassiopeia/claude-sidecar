use std::time::Instant;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{
    error::SidecarError,
    logger,
    routes::exec::{self, RunSpec},
    AppState,
};

/// One command in a batch. `cwd`, `timeout_secs`, and `env` override the
/// batch-level defaults for this step only.
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

/// `POST /batch` request: an ordered list of commands to run in sequence.
#[derive(Debug, Deserialize)]
pub struct BatchRequest {
    pub steps: Vec<BatchStep>,
    /// Default working directory for steps that don't set their own.
    pub cwd: Option<String>,
    /// Default per-step timeout in seconds (default 60). Applies to each step
    /// individually — it is not a budget for the batch as a whole.
    pub timeout_secs: Option<u64>,
    /// Env applied to every step. A step's own `env` wins on key collisions.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// When false (default), stop at the first step that exits nonzero. When
    /// true, run every step regardless of exit codes.
    #[serde(default)]
    pub continue_on_error: bool,
}

#[derive(Debug, Serialize)]
pub struct BatchStepResult {
    pub cmd: String,
    pub args: Vec<String>,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    /// Results for the steps that actually ran, in order. Shorter than the
    /// request's `steps` if the batch stopped early on a failure.
    pub steps: Vec<BatchStepResult>,
    /// Index (into the request's `steps`) of the first step that exited
    /// nonzero, if any.
    pub failed_at: Option<usize>,
    /// True when every requested step ran and all exited zero.
    pub success: bool,
}

/// Upper bound on steps per batch — a sanity guard, not a security control.
const MAX_STEPS: usize = 100;

/// `POST /batch` — run an ordered sequence of allowlisted commands, stopping at
/// the first failure unless `continue_on_error` is set.
///
/// Every step is validated against the allowlist *before any step runs*, so a
/// disallowed command anywhere in the sequence rejects the whole batch (403)
/// without executing side effects. This is stricter than running until the bad
/// step is reached, and keeps a partially-applied batch from happening because
/// of a typo'd command name.
pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, SidecarError> {
    if req.steps.is_empty() {
        return Err(SidecarError::InvalidRequest("`steps` is empty".into()));
    }
    if req.steps.len() > MAX_STEPS {
        return Err(SidecarError::InvalidRequest(format!(
            "too many steps ({}); max {MAX_STEPS}",
            req.steps.len()
        )));
    }

    // Pre-validate every step so a bad command name fails the whole batch
    // before we run anything side-effecting.
    for step in &req.steps {
        exec::validate(&step.cmd, &step.args)?;
    }

    let default_timeout = req.timeout_secs.unwrap_or(60);
    logger::log_request(
        "POST",
        "/batch",
        &format!("{} steps", req.steps.len()),
        &[],
        req.cwd.as_deref(),
    );
    let started = Instant::now();

    let mut results = Vec::with_capacity(req.steps.len());
    let mut failed_at = None;

    for (idx, step) in req.steps.iter().enumerate() {
        let cwd = step.cwd.as_deref().or(req.cwd.as_deref());
        let timeout_secs = step.timeout_secs.unwrap_or(default_timeout);
        // Batch env first, then step env — Command applies later keys last, so
        // a step's own env overrides the batch default on collision.
        let mut env = req.env.clone();
        env.extend(step.env.iter().cloned());

        logger::log_request("POST", "/batch", &step.cmd, &step.args, cwd);

        let out = exec::run_command(RunSpec {
            cmd: &step.cmd,
            args: &step.args,
            cwd,
            env: &env,
            timeout_secs,
            verbose: state.config.verbose,
        })
        .await?;

        let exit_code = out.exit_code;
        results.push(BatchStepResult {
            cmd: step.cmd.clone(),
            args: step.args.clone(),
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code,
        });

        if exit_code != 0 && failed_at.is_none() {
            failed_at = Some(idx);
            if !req.continue_on_error {
                break;
            }
        }
    }

    let success = failed_at.is_none();
    logger::log_completion(
        "/batch",
        Some(if success { 0 } else { 1 }),
        started.elapsed().as_millis(),
    );

    Ok(Json(BatchResponse {
        steps: results,
        failed_at,
        success,
    }))
}
