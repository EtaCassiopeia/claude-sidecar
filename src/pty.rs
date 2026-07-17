//! Subprocess execution for `/jobs`.
//!
//! Runs a command under a pseudo-terminal so tools emit their interactive,
//! colorized output (progress bars, spinners), and falls back to plain pipes
//! when `openpty` is unavailable (e.g. a locked-down sandbox). Output is read in
//! buffered chunks and streamed line-by-line into the [`Job`]. A timeout kills
//! the whole process group so nothing is left running.

use std::{
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    task,
    time::timeout,
};

use crate::{
    job::{Job, Outcome},
    logger,
};

/// Everything the blocking runner needs to launch a process.
pub struct PtyOptions {
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub timeout: Duration,
    pub verbose: bool,
}

/// Shares the spawned child's PID with the async side so a timeout or a cancel
/// request can signal the whole process group. `0` means "not spawned yet".
#[derive(Clone, Default)]
pub struct KillHandle(Arc<AtomicI32>);

impl KillHandle {
    fn set(&self, pid: i32) {
        self.0.store(pid, Ordering::SeqCst);
    }

    /// SIGKILL the process group led by the child (created via `setsid`).
    pub fn kill_group(&self) {
        let pid = self.0.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: `kill(2)` is always safe to invoke. A negative PID targets
            // the process group, which the child leads because we call `setsid`
            // in `pre_exec`, so this reaps the entire process tree.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

/// Run `opts` to completion, streaming lines into `job`, and return the outcome.
///
/// The synchronous read loop runs on a blocking thread; this async wrapper
/// drains lines into the job buffer and enforces the overall timeout.
pub async fn run(job: Arc<Job>, opts: PtyOptions) -> Outcome {
    let timeout_dur = opts.timeout;
    let verbose = opts.verbose;
    let job_id = job.id.clone();

    let (line_tx, mut line_rx) = mpsc::channel::<String>(512);
    let (exit_tx, exit_rx) = oneshot::channel::<Result<i32, String>>();
    let kill = KillHandle::default();

    let runner_kill = kill.clone();
    let arm_kill = kill.clone();
    task::spawn_blocking(move || {
        let _ = exit_tx.send(blocking_loop(opts, &line_tx, &runner_kill));
    });

    // Arm the job with the kill handle once the PID is set (blocking_loop sets
    // it synchronously before any lines flow), so cancel() can SIGKILL the group.
    job.arm_kill(move || arm_kill.kill_group());

    let drain = async {
        while let Some(line) = line_rx.recv().await {
            if verbose {
                logger::log_line(&line);
            }
            job.push_line(line);
        }
        // Channel closed => process finished; collect the exit status.
        match exit_rx.await {
            Ok(Ok(code)) => Outcome::Completed { exit_code: code },
            Ok(Err(err)) => {
                tracing::error!(%job_id, "pty runner failed: {err}");
                Outcome::Failed
            }
            Err(_) => Outcome::Failed,
        }
    };

    match timeout(timeout_dur, drain).await {
        Ok(outcome) => outcome,
        Err(_) => {
            tracing::warn!(%job_id, "job timed out after {}s", timeout_dur.as_secs());
            kill.kill_group();
            Outcome::TimedOut
        }
    }
}

/// Try a PTY; fall back to pipes if the platform/sandbox refuses `openpty`.
fn blocking_loop(
    opts: PtyOptions,
    line_tx: &mpsc::Sender<String>,
    kill: &KillHandle,
) -> Result<i32, String> {
    use nix::pty::{openpty, Winsize};

    let winsize = Winsize {
        ws_row: opts.rows,
        ws_col: opts.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    match openpty(Some(&winsize), None) {
        Ok(pty) => run_with_pty(opts, line_tx, kill, pty),
        Err(_) => run_with_pipes(opts, line_tx, kill),
    }
}

fn run_with_pty(
    opts: PtyOptions,
    line_tx: &mpsc::Sender<String>,
    kill: &KillHandle,
    pty: nix::pty::OpenptyResult,
) -> Result<i32, String> {
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::process::CommandExt;

    let slave_fd = pty.slave.into_raw_fd();
    // SAFETY: `slave_fd` is a freshly opened, owned fd from `openpty`. Each
    // `Stdio` takes ownership of an fd and closes it after the child inherits
    // it; `dup` produces independent owned fds for stdout and stderr.
    let (stdin, stdout, stderr) = unsafe {
        (
            Stdio::from_raw_fd(slave_fd),
            Stdio::from_raw_fd(libc::dup(slave_fd)),
            Stdio::from_raw_fd(libc::dup(slave_fd)),
        )
    };

    let mut cmd = Command::new(&opts.cmd);
    cmd.args(&opts.args)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    apply_cwd_env(&mut cmd, &opts);

    // SAFETY: the closure runs in the child between `fork` and `exec` and calls
    // only async-signal-safe functions: `setsid` (new session/group so we can
    // group-kill) and `ioctl(TIOCSCTTY)` (adopt the PTY as controlling tty).
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0i32);
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    kill.set(child.id() as i32);

    let master_fd = pty.master.into_raw_fd();
    // SAFETY: `master_fd` is an owned fd from `openpty`; `File` takes ownership
    // and closes it on drop.
    let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    pump(master, line_tx);

    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    Ok(status.code().unwrap_or(-1))
}

fn run_with_pipes(
    opts: PtyOptions,
    line_tx: &mpsc::Sender<String>,
    kill: &KillHandle,
) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;

    tracing::debug!("openpty unavailable; falling back to pipes");

    let mut cmd = Command::new(&opts.cmd);
    cmd.args(&opts.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_cwd_env(&mut cmd, &opts);

    // SAFETY: `setsid` is async-signal-safe and runs in the child before `exec`;
    // it makes the child a process-group leader so a timeout can group-kill it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    kill.set(child.id() as i32);

    // Drain stdout and stderr concurrently. Reading them sequentially would
    // deadlock: a child that fills the stderr pipe buffer blocks forever while
    // we're still draining stdout.
    let stderr = child.stderr.take();
    let stderr_thread = stderr.map(|err| {
        let tx = line_tx.clone();
        std::thread::spawn(move || pump(err, &tx))
    });

    if let Some(out) = child.stdout.take() {
        pump(out, line_tx);
    }
    if let Some(handle) = stderr_thread {
        let _ = handle.join();
    }

    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    Ok(status.code().unwrap_or(-1))
}

fn apply_cwd_env(cmd: &mut Command, opts: &PtyOptions) {
    if let Some(dir) = &opts.cwd {
        cmd.current_dir(dir);
    }
    for (key, val) in &opts.env {
        cmd.env(key, val);
    }
}

/// Read `reader` in buffered chunks, splitting on `\n`/`\r`, and forward each
/// non-empty, ANSI-stripped line to `tx`. Stops on EOF, read error, or a closed
/// channel (the receiver went away).
fn pump<R: Read>(reader: R, tx: &mpsc::Sender<String>) {
    let mut buf = BufReader::with_capacity(8192, reader);
    let mut segment = Vec::with_capacity(256);
    while let Ok(more) = read_segment(&mut buf, &mut segment) {
        if !segment.is_empty() {
            let text = strip_ansi(&String::from_utf8_lossy(&segment));
            if !text.is_empty() && tx.blocking_send(text).is_err() {
                break;
            }
        }
        if !more {
            break;
        }
    }
}

/// Read bytes into `out` up to (excluding) the next `\n` or `\r`, consuming the
/// delimiter. Returns `Ok(true)` if a delimiter was found (more may follow) or
/// `Ok(false)` at EOF, leaving any trailing bytes in `out`. Buffered: it draws
/// from the `BufRead` fill buffer rather than one syscall per byte.
fn read_segment<R: BufRead>(reader: &mut R, out: &mut Vec<u8>) -> std::io::Result<bool> {
    out.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(false); // EOF
        }
        match available.iter().position(|&b| b == b'\n' || b == b'\r') {
            Some(idx) => {
                out.extend_from_slice(&available[..idx]);
                reader.consume(idx + 1);
                return Ok(true);
            }
            None => {
                let n = available.len();
                out.extend_from_slice(available);
                reader.consume(n);
            }
        }
    }
}

/// Strip ANSI CSI/escape sequences so stored lines are plain text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Consume until the final byte of the CSI sequence (a letter).
                for ch in chars.by_ref() {
                    if ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\x1b[31merror\x1b[0m"), "error");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[1;32mok\x1b[0m done"), "ok done");
    }

    #[test]
    fn read_segment_splits_on_lf_and_cr() {
        let mut cur = Cursor::new(b"one\ntwo\rthree".to_vec());
        let mut seg = Vec::new();

        assert!(read_segment(&mut cur, &mut seg).unwrap());
        assert_eq!(seg, b"one");
        assert!(read_segment(&mut cur, &mut seg).unwrap());
        assert_eq!(seg, b"two");
        // Final segment has no trailing delimiter -> EOF with data.
        assert!(!read_segment(&mut cur, &mut seg).unwrap());
        assert_eq!(seg, b"three");
    }

    #[test]
    fn pump_forwards_nonempty_lines() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let data = Cursor::new(b"\x1b[32mfirst\x1b[0m\n\nsecond\n".to_vec());
        // Blank line (between the two \n) is dropped.
        std::thread::spawn(move || pump(data, &tx));
        let mut got = Vec::new();
        while let Some(line) = rx.blocking_recv() {
            got.push(line);
        }
        assert_eq!(got, vec!["first".to_string(), "second".to_string()]);
    }
}
