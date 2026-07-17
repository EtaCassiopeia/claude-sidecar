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

/// `POST /exec` — run a short, allowlisted command and return its buffered
/// output. For long-running work use `POST /jobs` instead.
pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, SidecarError> {
    if !config::is_allowed(&req.cmd) {
        return Err(SidecarError::NotAllowed(req.cmd));
    }
    if let Err(reason) = config::check_args(&req.cmd, &req.args) {
        return Err(SidecarError::NotAllowed(reason));
    }
    let resolved =
        config::resolve(&req.cmd).ok_or_else(|| SidecarError::CommandNotFound(req.cmd.clone()))?;

    let timeout_secs = req.timeout_secs.unwrap_or(60);
    logger::log_request("POST", "/exec", &req.cmd, &req.args, req.cwd.as_deref());
    let started = Instant::now();

    let mut cmd = Command::new(&resolved);
    cmd.args(&req.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If we drop the child on timeout, make sure the OS process dies too.
        .kill_on_drop(true);
    if let Some(dir) = &req.cwd {
        cmd.current_dir(dir);
    }
    for (key, val) in &req.env {
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

    let verbose = state.config.verbose;
    // Drain both streams concurrently so a full pipe buffer can't deadlock us.
    let stdout_task = tokio::spawn(collect(stdout, verbose));
    let stderr_task = tokio::spawn(collect(stderr, verbose));

    let wait = async {
        let stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((stdout, stderr, status))
    };

    match timeout(Duration::from_secs(timeout_secs), wait).await {
        Ok(Ok((stdout, stderr, status))) => {
            let exit_code = status.code().unwrap_or(-1);
            logger::log_completion("/exec", Some(exit_code), started.elapsed().as_millis());
            Ok(Json(ExecResponse {
                stdout,
                stderr,
                exit_code,
            }))
        }
        Ok(Err(e)) => Err(SidecarError::Io(e)),
        Err(_) => {
            // `wait` (and with it `child`) is dropped here; `kill_on_drop` reaps
            // the process.
            logger::log_completion("/exec", None, started.elapsed().as_millis());
            Err(SidecarError::Timeout { secs: timeout_secs })
        }
    }
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
