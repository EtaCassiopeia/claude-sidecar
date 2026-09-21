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

use crate::{config, error::SidecarError, events::ActivityKind, logger, AppState};

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Claude Code session on whose behalf this runs. One sidecar serves every
    /// session on the machine, so without it the log is one undifferentiated
    /// stream. Optional: a hand-written `curl` has no session to report.
    pub session_id: Option<String>,
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
    // Announced before validation, so a monitor sees denied calls too — a command
    // rejected by policy is exactly the sort of thing worth surfacing, and it is
    // invisible if the record only starts after the gate.
    //
    // A blank session id is treated as absent, matching `/jobs`: a shell that
    // interpolates an unset variable sends `""`, and storing that would make
    // "unattributed" look like a distinct session.
    let session = req
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let activity = state.events.start_for_session(
        ActivityKind::Exec,
        &req.cmd,
        &req.args,
        req.cwd.as_deref(),
        session,
    );

    if let Err(e) = validate(&req.cmd, &req.args) {
        state.events.finish(activity, None, Some(e.to_string()));
        return Err(e);
    }

    let timeout_secs = req.timeout_secs.unwrap_or(60);
    logger::log_request("POST", "/exec", &req.cmd, &req.args, req.cwd.as_deref());
    let started = Instant::now();

    let result = run_command(
        &req.cmd,
        &req.args,
        req.cwd.as_deref(),
        timeout_secs,
        &req.env,
        state.config.verbose,
    )
    .await;

    match &result {
        Ok(r) => {
            logger::log_completion("/exec", Some(r.exit_code), started.elapsed().as_millis());
            state.events.finish(activity, Some(r.exit_code), None);
        }
        Err(e) => {
            logger::log_completion("/exec", None, started.elapsed().as_millis());
            state.events.finish(activity, None, Some(e.to_string()));
        }
    }
    result.map(Json)
}

/// Validate that a command may be run: it must be permitted by the active
/// policy (deny-by-exception — see [`crate::config::Policy`]).
///
/// This is the single gate `/exec`, `/batch`, and `/jobs` pass through, so there
/// is no way to reach `run_command` or the job runner without it.
///
/// Arguments are deliberately not inspected. The previous allowlist paired with
/// an interpreter-flag check to stop `python3 -c "os.system(…)"` from reaching
/// binaries the allowlist excluded; with no allowlist there is nothing for such
/// a check to protect, and it only blocked legitimate invocations.
pub(crate) fn validate(cmd: &str, _args: &[String]) -> Result<(), SidecarError> {
    if !config::is_allowed(cmd) {
        return Err(SidecarError::NotAllowed(cmd.to_string()));
    }
    Ok(())
}

/// Run a single allowlisted command to completion, buffering stdout/stderr.
///
/// Callers MUST have already passed `(cmd, args)` through [`validate`]. Does no
/// logging of its own — the caller owns request/completion logging so the log
/// path (`/exec` vs `/batch`) stays accurate.
pub(crate) async fn run_command(
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_secs: u64,
    env: &[(String, String)],
    verbose: bool,
) -> Result<ExecResponse, SidecarError> {
    let resolved =
        config::resolve(cmd).ok_or_else(|| SidecarError::CommandNotFound(cmd.to_string()))?;

    let mut command = Command::new(&resolved);
    command
        .args(args)
        // No stdin: a command that prompts must see EOF and take its default or
        // fail. Inheriting the sidecar's stdin makes it block on a descriptor
        // nobody will ever write to, so the whole request waits out the timeout.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If we drop the child on timeout, make sure the OS process dies too.
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    // Overlay a fresh login-shell environment (rotating registry token, resolver creds,
    // …) on top of the sidecar's frozen launch-time env, then let per-request env win.
    for (key, val) in crate::env_refresh::fresh_env().iter() {
        command.env(key, val);
    }
    for (key, val) in env {
        command.env(key, val);
    }

    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SidecarError::Internal("stdout pipe missing".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SidecarError::Internal("stderr pipe missing".into()))?;

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
        Ok(Ok((stdout, stderr, status))) => Ok(ExecResponse {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => Err(SidecarError::Io(e)),
        // `wait` (and with it `child`) is dropped here; `kill_on_drop` reaps
        // the process.
        Err(_) => Err(SidecarError::Timeout { secs: timeout_secs }),
    }
}

/// Cap on the bytes retained per stream for a buffered `/exec` response.
///
/// The whole response is held in memory and serialized into one JSON body, so an
/// unbounded read means a command that prints gigabytes puts gigabytes in the
/// response. Past the cap the tail is dropped and a marker is appended, since
/// truncating loudly beats either an OOM or a silently short body. Use `/jobs`
/// for output that legitimately gets this large.
const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// Read a stream to end-of-file, returning its text (bounded by
/// [`MAX_STREAM_BYTES`]) and optionally echoing each line to the server log.
async fn collect<R: AsyncRead + Unpin>(reader: R, verbose: bool) -> String {
    let mut lines = BufReader::new(reader).lines();
    let mut buf = String::new();
    let mut truncated_bytes = 0usize;
    while let Ok(Some(line)) = lines.next_line().await {
        if verbose {
            logger::log_line(&line);
        }
        // Keep draining after the cap: stopping early would leave the child
        // blocked on a full pipe. We just stop retaining.
        if buf.len() + line.len() + 1 > MAX_STREAM_BYTES {
            truncated_bytes += line.len() + 1;
            continue;
        }
        buf.push_str(&line);
        buf.push('\n');
    }
    if truncated_bytes > 0 {
        buf.push_str(&format!(
            "\n[sidecar: {truncated_bytes} further bytes dropped — output exceeded \
             {MAX_STREAM_BYTES} bytes; use POST /jobs for large output]\n"
        ));
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn collect_retains_output_under_the_cap() {
        let data = std::io::Cursor::new(b"one\ntwo\n".to_vec());
        assert_eq!(collect(data, false).await, "one\ntwo\n");
    }

    #[tokio::test]
    async fn collect_truncates_and_says_so() {
        // A line long enough to blow the cap on its own.
        let big = "x".repeat(MAX_STREAM_BYTES + 1024);
        let data = std::io::Cursor::new(format!("keep\n{big}\n").into_bytes());
        let out = collect(data, false).await;
        assert!(out.starts_with("keep\n"), "earlier output must be kept");
        assert!(
            out.contains("further bytes dropped"),
            "truncation must be announced, not silent"
        );
        assert!(
            out.len() < MAX_STREAM_BYTES + 512,
            "retained {} bytes, cap is {MAX_STREAM_BYTES}",
            out.len()
        );
    }

    /// A prompting command used to inherit the sidecar's stdin and block on a
    /// descriptor nobody would ever write to, burning the whole 60s timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_reads_stdin_gets_eof_instead_of_hanging() {
        let args = vec![
            "-c".to_string(),
            "read a; printf 'done=%s' \"${a:-empty}\"".to_string(),
        ];
        let res = run_command("sh", &args, None, 10, &[], false)
            .await
            .expect("should run");
        assert!(res.stdout.contains("done=empty"), "{res:?}");
    }
}
