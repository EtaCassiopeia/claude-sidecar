use std::{
    process::Stdio,
    time::{Duration, Instant},
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    time::timeout,
};

use crate::{config, error::SidecarError, logger, AppState};

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Serialize)]
pub struct ExecResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Everything needed to run one allowlisted command. Borrowed so both `/exec`
/// and `/batch` can build a spec from their own request shapes without cloning.
pub(crate) struct RunSpec<'a> {
    pub cmd: &'a str,
    pub args: &'a [String],
    pub cwd: Option<&'a str>,
    pub env: &'a [(String, String)],
    pub timeout_secs: u64,
    pub verbose: bool,
}

/// Validate a command against the allowlist and its argument rules.
///
/// Separated from execution so a multi-step caller (`/batch`) can reject an
/// entire request up front — before running any side-effecting step — if any
/// command is disallowed.
pub(crate) fn validate(cmd: &str, args: &[String]) -> Result<(), SidecarError> {
    if !config::is_allowed(cmd) {
        return Err(SidecarError::NotAllowed(cmd.to_string()));
    }
    if let Err(reason) = config::check_args(cmd, args) {
        return Err(SidecarError::NotAllowed(reason));
    }
    Ok(())
}

/// Run one allowlisted command to completion (or timeout) and return its
/// buffered output. Validates first, so it is safe to call directly.
pub(crate) async fn run_command(spec: RunSpec<'_>) -> Result<ExecResponse, SidecarError> {
    validate(spec.cmd, spec.args)?;
    let resolved =
        config::resolve(spec.cmd).ok_or_else(|| SidecarError::CommandNotFound(spec.cmd.into()))?;

    let mut cmd = Command::new(&resolved);
    cmd.args(spec.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If we drop the child on timeout, make sure the OS process dies too.
        .kill_on_drop(true);
    if let Some(dir) = spec.cwd {
        cmd.current_dir(dir);
    }
    for (key, val) in spec.env {
        cmd.env(key, val);
    }

    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SidecarError::Internal("stdout pipe missing".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SidecarError::Internal("stderr pipe missing".into()))?;

    // Drain both streams concurrently so a full pipe buffer can't deadlock us.
    let stdout_task = tokio::spawn(collect(stdout, spec.verbose));
    let stderr_task = tokio::spawn(collect(stderr, spec.verbose));

    let wait = async {
        let stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((stdout, stderr, status))
    };

    match timeout(Duration::from_secs(spec.timeout_secs), wait).await {
        Ok(Ok((stdout, stderr, status))) => Ok(ExecResponse {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => Err(SidecarError::Io(e)),
        // `wait` (and with it `child`) is dropped here; `kill_on_drop` reaps it.
        Err(_) => Err(SidecarError::Timeout {
            secs: spec.timeout_secs,
        }),
    }
}

/// `POST /exec` — run a short, allowlisted command and return its buffered
/// output. For long-running work use `POST /jobs` instead.
pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, SidecarError> {
    let timeout_secs = req.timeout_secs.unwrap_or(60);
    logger::log_request("POST", "/exec", &req.cmd, &req.args, req.cwd.as_deref());
    let started = Instant::now();

    let result = run_command(RunSpec {
        cmd: &req.cmd,
        args: &req.args,
        cwd: req.cwd.as_deref(),
        env: &req.env,
        timeout_secs,
        verbose: state.config.verbose,
    })
    .await;

    match &result {
        Ok(resp) => {
            logger::log_completion("/exec", Some(resp.exit_code), started.elapsed().as_millis())
        }
        Err(_) => logger::log_completion("/exec", None, started.elapsed().as_millis()),
    }
    result.map(Json)
}

/// Read a stream to end-of-file, returning its full text and optionally echoing
/// each line to the server log.
async fn collect<R: AsyncRead + Unpin>(reader: R, verbose: bool) -> String {
    let mut lines = BufReader::new(reader).lines();
    let mut buf = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if verbose {
            logger::log_line(&line);
        }
        buf.push_str(&line);
        buf.push('\n');
    }
    buf
}
