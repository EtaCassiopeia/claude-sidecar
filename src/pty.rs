//! Subprocess execution for `/jobs`.
//!
//! Runs a command under a pseudo-terminal so tools emit their interactive,
//! colorized output (progress bars, spinners), and falls back to plain pipes
//! when `openpty` is unavailable (e.g. a locked-down sandbox). Output is read in
//! buffered chunks and streamed line-by-line into the [`Job`]. A timeout kills
//! the whole process group so nothing is left running.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
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
    /// How long a `SIGTERM`'d process group gets to exit before `SIGKILL`.
    /// Zero means skip straight to `SIGKILL`.
    pub kill_grace: Duration,
    /// Kill the job if it produces no output for this long. `None` disables the
    /// check — a legitimately quiet job (a long link step, a silent test run)
    /// must not be killed by default.
    pub idle_timeout: Option<Duration>,
    /// Keystrokes to feed the process's stdin up front, for commands that stop on
    /// a confirmation prompt (`"Y\n"`). `None` closes stdin immediately, which
    /// makes a prompting command take its default or fail instead of hanging
    /// forever on a terminal nobody is typing into.
    pub input: Option<String>,
    pub verbose: bool,
}

/// Shares the spawned child's PID with the async side so a timeout or a cancel
/// request can signal the whole process group.
///
/// Also records a cancel that arrives *before* the child exists: without that,
/// a request landing in the window between job creation and `spawn` would find
/// no PID, signal nothing, and be silently lost while the API reported success.
/// `set` replays it, so the ordering cannot matter.
#[derive(Clone, Default)]
pub struct KillHandle(Arc<KillState>);

#[derive(Default)]
struct KillState {
    /// Child PID, or `0` before spawn.
    pid: AtomicI32,
    /// A signal was requested before the PID was known; `set` delivers it.
    pending: AtomicBool,
}

impl KillHandle {
    /// Publish the child's PID, delivering any cancel that arrived first.
    fn set(&self, pid: i32) {
        self.0.pid.store(pid, Ordering::SeqCst);
        if self.0.pending.swap(false, Ordering::SeqCst) {
            // Cancel already requested: honour it now rather than letting the
            // child run on. Terminal rung — the requester is not waiting on a
            // grace period that started before the process existed.
            signal_group(pid, libc::SIGKILL);
        }
    }

    /// Ask the process group to terminate, giving it a chance to clean up.
    ///
    /// Returns whether the request was recorded — `true` even before spawn,
    /// because `set` will replay it. Only `false` if the process already exited.
    pub fn terminate_group(&self) -> bool {
        match self.0.pid.load(Ordering::SeqCst) {
            0 => {
                self.0.pending.store(true, Ordering::SeqCst);
                true
            }
            pid => signal_group(pid, libc::SIGTERM),
        }
    }

    /// SIGKILL the process group. Used when a `SIGTERM` grace period expires.
    pub fn kill_group(&self) -> bool {
        match self.0.pid.load(Ordering::SeqCst) {
            0 => {
                self.0.pending.store(true, Ordering::SeqCst);
                true
            }
            pid => signal_group(pid, libc::SIGKILL),
        }
    }

    /// Has the child exited? Used to end a grace period early.
    fn is_alive(&self) -> bool {
        match self.0.pid.load(Ordering::SeqCst) {
            0 => true, // not spawned yet — treat as pending, not gone
            // Signal 0 probes for existence without delivering anything.
            pid => signal_group(pid, 0),
        }
    }
}

/// Send `sig` to the process group led by `pid`, returning whether it landed.
///
/// The negative PID is essential: it targets the whole group, which the child
/// leads because of the `setsid` in `pre_exec`. Signalling `pid` alone would
/// leave grandchildren running — and `sbt`/`cargo` both spawn them.
pub(crate) fn signal_group(pid: i32, sig: libc::c_int) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill(2)` is always safe to invoke; a negative PID addresses the
    // process group. Signal 0 checks for existence without delivering.
    unsafe { libc::kill(-pid, sig) == 0 }
}

/// What the blocking runner reports to the async side over one channel.
enum Chunk {
    /// A completed output line.
    Line(String),
    /// The child produced output that did not complete a line — an in-place
    /// redraw. Carries no text, but proves the job is alive for idle detection.
    Activity,
    /// The child's PID, sent once, as soon as it is known.
    Pid(i32),
}

/// Run `opts` to completion, streaming lines into `job`, and return the outcome.
///
/// The synchronous read loop runs on a blocking thread; this async wrapper
/// drains lines into the job buffer and enforces the overall timeout.
pub async fn run(job: Arc<Job>, opts: PtyOptions) -> Outcome {
    let timeout_dur = opts.timeout;
    let grace = opts.kill_grace;
    let idle_limit = opts.idle_timeout;
    let verbose = opts.verbose;
    let job_id = job.id.clone();

    let (line_tx, mut line_rx) = mpsc::channel::<Chunk>(512);
    let (exit_tx, exit_rx) = oneshot::channel::<Result<i32, String>>();
    let kill = KillHandle::default();

    // Arm before spawning, so a cancel arriving in the spawn window is recorded
    // by the handle and replayed once the PID is known, rather than lost. The
    // callback only requests termination; escalation is enforced below, so the
    // grace period is applied from one place for both cancel and timeout.
    let arm_kill = kill.clone();
    let canceled_tx = Arc::new(tokio::sync::Notify::new());
    let canceled_rx = Arc::clone(&canceled_tx);
    job.arm_kill(move || {
        let sent = arm_kill.terminate_group();
        canceled_tx.notify_waiters();
        sent
    });

    let runner_kill = kill.clone();
    task::spawn_blocking(move || {
        let _ = exit_tx.send(blocking_loop(opts, &line_tx, &runner_kill));
    });

    // Set by whichever path has to escalate to `SIGKILL`. Shared because cancel,
    // wall-clock timeout, and idle timeout can each get there, and the caller
    // needs the answer whichever one did.
    let escalated = Arc::new(AtomicBool::new(false));

    let drain_job = Arc::clone(&job);
    let drain_id = job_id.clone();
    let drain = async move {
        while let Some(chunk) = line_rx.recv().await {
            match chunk {
                Chunk::Line(line) => {
                    if verbose {
                        logger::log_line(&line);
                    }
                    drain_job.push_line(line);
                }
                Chunk::Activity => drain_job.mark_output(),
                Chunk::Pid(pid) => drain_job.set_pid(pid),
            }
        }
        // Channel closed => process finished; collect the exit status.
        match exit_rx.await {
            Ok(Ok(code)) => Outcome::Completed { exit_code: code },
            Ok(Err(err)) => {
                tracing::error!(%drain_id, "pty runner failed: {err}");
                Outcome::Failed
            }
            Err(_) => Outcome::Failed,
        }
    };

    // Escalate a cancel that the child ignores. Without this a SIGTERM-deaf
    // process would hang until the (possibly hour-long) job timeout.
    let escalate_on_cancel = {
        let kill = kill.clone();
        let escalated = Arc::clone(&escalated);
        async move {
            canceled_rx.notified().await;
            enforce_grace(&kill, grace, &escalated).await;
        }
    };

    tokio::pin!(drain);
    tokio::pin!(escalate_on_cancel);

    // The deadline is a `select!` arm rather than a `timeout()` wrapper: wrapping
    // would drop `drain` the moment it fired, discarding exactly the cleanup
    // output the SIGTERM grace period exists to capture.
    let deadline = tokio::time::sleep(timeout_dur);
    tokio::pin!(deadline);
    let mut cancel_handled = false;

    let reason = loop {
        tokio::select! {
            outcome = &mut drain => {
                // The child exited on its own. If that was in response to a
                // cancel, report Canceled here — this is the only place that
                // knows whether the shutdown had to escalate to SIGKILL.
                return if job.was_canceled() {
                    Outcome::Canceled { escalated: escalated.load(Ordering::SeqCst) }
                } else {
                    outcome
                };
            }
            // Cancel escalation finished; keep draining so the final lines and
            // the real exit status still land. Disabled afterwards: polling a
            // completed future panics, and there is nothing left to escalate.
            _ = &mut escalate_on_cancel, if !cancel_handled => {
                cancel_handled = true;
                continue;
            }
            _ = &mut deadline => break StopReason::Timeout,
            _ = idle_expired(&job, idle_limit) => break StopReason::Idle,
        }
    };

    match reason {
        StopReason::Timeout => {
            tracing::warn!(%job_id, "job timed out after {}s", timeout_dur.as_secs())
        }
        StopReason::Idle => {
            tracing::warn!(%job_id, "job produced no output for {idle_limit:?}; terminating")
        }
    }

    // Signal and keep draining concurrently, so anything the child writes on its
    // way out is stored rather than lost with the dropped future.
    {
        let shutdown = shut_down(&kill, grace, &escalated);
        tokio::pin!(shutdown);
        let drained = tokio::select! {
            // The child exited during shutdown: `drain` has the full output.
            _ = &mut drain => true,
            _ = &mut shutdown => false,
        };
        if !drained {
            // Shutdown finished first: give the drain a bounded moment to collect
            // the last lines and the exit status now that the process is gone.
            let _ = timeout(POST_SIGNAL_DRAIN, &mut drain).await;
        }
    }

    let escalated = escalated.load(Ordering::SeqCst);
    // A cancel racing the deadline still reads as a cancel: that is what the
    // caller asked for, and it outranks the mechanical reason we stopped waiting.
    if job.was_canceled() {
        return Outcome::Canceled { escalated };
    }
    match reason {
        StopReason::Timeout => Outcome::TimedOut { escalated },
        StopReason::Idle => Outcome::IdleTimedOut { escalated },
    }
}

/// Why the runner stopped waiting for the child.
#[derive(Debug, Clone, Copy)]
enum StopReason {
    Timeout,
    Idle,
}

/// Grace for the drain to finish after the process has been signalled. The
/// process is gone by then, so this only covers reading what it already wrote.
const POST_SIGNAL_DRAIN: Duration = Duration::from_secs(2);

/// Resolve once the job has been silent for longer than `limit`.
///
/// Never resolves when `limit` is `None`, so the caller's `select!` arm is inert
/// unless idle detection was requested.
async fn idle_expired(job: &Job, limit: Option<Duration>) {
    let Some(limit) = limit else {
        // Pending forever: no idle timeout configured.
        return std::future::pending().await;
    };
    let limit_ms = limit.as_millis() as u64;
    // Poll at a fraction of the limit. Sleeping for the whole remaining budget
    // would skip past output that arrived mid-sleep and reset the clock, killing
    // a job that was in fact talking the entire time.
    let step = Duration::from_millis((limit_ms / 4).clamp(20, 1_000));
    loop {
        if job.idle_ms() >= limit_ms {
            return;
        }
        tokio::time::sleep(step).await;
    }
}

/// How often to re-check for exit while waiting out a grace period.
const GRACE_POLL: Duration = Duration::from_millis(50);

/// Wait out `grace`, then `SIGKILL` if the group is still alive. Records whether
/// escalation was needed. Assumes `SIGTERM` has already been sent.
async fn enforce_grace(kill: &KillHandle, grace: Duration, escalated: &AtomicBool) {
    if grace.is_zero() {
        kill.kill_group();
        escalated.store(true, Ordering::SeqCst);
        return;
    }
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        if !kill.is_alive() {
            return; // exited on its own terms
        }
        tokio::time::sleep(GRACE_POLL).await;
    }
    kill.kill_group();
    escalated.store(true, Ordering::SeqCst);
}

/// Ask the process group to stop, escalating to `SIGKILL` if it outlasts `grace`.
///
/// `SIGTERM` first is what lets `sbt` flush and `cargo` release its lock files;
/// going straight to `SIGKILL` (as this used to) guarantees neither happens.
async fn shut_down(kill: &KillHandle, grace: Duration, escalated: &AtomicBool) {
    if grace.is_zero() {
        kill.kill_group();
        escalated.store(true, Ordering::SeqCst);
        return;
    }
    kill.terminate_group();
    enforce_grace(kill, grace, escalated).await;
}

/// Try a PTY; fall back to pipes if the platform/sandbox refuses `openpty`.
fn blocking_loop(
    opts: PtyOptions,
    line_tx: &mpsc::Sender<Chunk>,
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
        Ok(pty) => {
            disable_onlcr(&pty.slave);
            run_with_pty(opts, line_tx, kill, pty)
        }
        Err(_) => run_with_pipes(opts, line_tx, kill),
    }
}

/// Stop the line discipline from rewriting the child's `\n` as `\r\n`.
///
/// With `ONLCR` on (the default), a real newline reaches us as CRLF, which makes
/// it indistinguishable from a progress bar's overwrite `\r` — and the reader has
/// to tell those apart to avoid storing one line per redraw frame. Clearing it
/// means `\n` means newline and a lone `\r` means overwrite, full stop.
///
/// Best-effort: on failure the reader still handles CRLF (it treats `\r\n` as a
/// single terminator), so this degrades rather than breaks. Applied before
/// `spawn`, so there is no pending output to drain.
fn disable_onlcr<Fd: std::os::fd::AsFd>(slave: &Fd) {
    use nix::sys::termios::{tcgetattr, tcsetattr, OutputFlags, SetArg};

    match tcgetattr(slave) {
        Ok(mut tio) => {
            tio.output_flags.remove(OutputFlags::ONLCR);
            if let Err(e) = tcsetattr(slave, SetArg::TCSANOW, &tio) {
                tracing::debug!("could not clear ONLCR: {e}; reader will handle CRLF");
            }
        }
        Err(e) => tracing::debug!("could not read termios: {e}; reader will handle CRLF"),
    }
}

fn run_with_pty(
    opts: PtyOptions,
    line_tx: &mpsc::Sender<Chunk>,
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
    let _ = line_tx.blocking_send(Chunk::Pid(child.id() as i32));

    let master_fd = pty.master.into_raw_fd();
    // SAFETY: `master_fd` is an owned fd from `openpty`; `File` takes ownership
    // and closes it on drop.
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    // Type the answers before reading, so a command that blocks on a prompt finds
    // them already in the tty buffer. Deliberately not sending EOF when there is
    // no input: on a PTY that means writing `^D`, and a batch-mode `sbt` reading
    // its console would take that as "quit".
    if let Some(input) = &opts.input {
        if let Err(e) = master.write_all(input.as_bytes()) {
            // The child may already have exited; that is not our failure to report.
            tracing::debug!("could not write job input: {e}");
        }
    }
    pump(master, line_tx);

    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    Ok(status.code().unwrap_or(-1))
}

fn run_with_pipes(
    opts: PtyOptions,
    line_tx: &mpsc::Sender<Chunk>,
    kill: &KillHandle,
) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;

    tracing::debug!("openpty unavailable; falling back to pipes");

    let mut cmd = Command::new(&opts.cmd);
    cmd.args(&opts.args)
        // Feed the answers through a pipe, or close stdin outright. Inheriting the
        // sidecar's stdin would leave a prompting command blocked on a descriptor
        // nobody writes to.
        .stdin(if opts.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    let _ = line_tx.blocking_send(Chunk::Pid(child.id() as i32));

    // Write the answers, then drop the handle so the child sees EOF rather than
    // waiting for more.
    if let (Some(input), Some(mut sink)) = (&opts.input, child.stdin.take()) {
        if let Err(e) = sink.write_all(input.as_bytes()) {
            tracing::debug!("could not write job input: {e}");
        }
    }

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
    // Overlay a fresh login-shell environment (rotating registry token, resolver creds,
    // …) on top of the sidecar's frozen launch-time env, then let per-request env win.
    for (key, val) in crate::env_refresh::fresh_env().iter() {
        cmd.env(key, val);
    }
    for (key, val) in &opts.env {
        cmd.env(key, val);
    }
}

/// Read `reader` in buffered chunks and forward each completed, non-empty,
/// ANSI-stripped line to `tx`. Stops on EOF, read error, or a closed channel
/// (the receiver went away).
fn pump<R: Read>(reader: R, tx: &mpsc::Sender<Chunk>) {
    let mut buf = BufReader::with_capacity(8192, reader);
    let mut asm = LineAssembler::default();

    loop {
        let chunk = match buf.fill_buf() {
            Ok([]) => break, // EOF
            Ok(c) => c,
            Err(_) => break,
        };
        let n = chunk.len();
        // `feed` borrows the fill buffer, so collect completed lines before
        // consuming: we cannot hold the borrow across `buf.consume`.
        let lines = asm.feed(chunk);
        buf.consume(n);

        if lines.is_empty() {
            // Bytes arrived but completed no line — an in-place redraw. Report
            // it so idle detection sees a live job rather than a silent one.
            if tx.blocking_send(Chunk::Activity).is_err() {
                return;
            }
        }
        for line in lines {
            if !emit(line, tx) {
                return;
            }
        }
    }

    // Whatever the child left without a trailing newline is still a line.
    if let Some(last) = asm.finish() {
        emit(last, tx);
    }
}

/// Strip ANSI, drop the line if nothing is left, and send. Returns `false` when
/// the receiver is gone and pumping should stop.
fn emit(raw: Vec<u8>, tx: &mpsc::Sender<Chunk>) -> bool {
    let text = strip_ansi(&String::from_utf8_lossy(&raw));
    if text.is_empty() {
        // Still activity, even though there is no line to store.
        return tx.blocking_send(Chunk::Activity).is_ok();
    }
    tx.blocking_send(Chunk::Line(text)).is_ok()
}

/// Assembles bytes into lines, honouring carriage return as *overwrite* rather
/// than as a line break.
///
/// `\r` is how a terminal redraws the current line — progress bars, spinners,
/// `sbt`'s status line. Treating it as a terminator (as this code used to) stores
/// one line per redraw frame, which inflates `line_count` and evicts real output
/// through the ring buffer's cap. The rules:
///
/// - `\n` ends a line.
/// - `\r\n` is a single terminator, not "overwrite, then an empty line".
/// - a lone `\r` discards the pending text: the frame it drew is being replaced.
/// - at EOF, pending text is a line (no trailing newline required).
#[derive(Default)]
struct LineAssembler {
    pending: Vec<u8>,
    /// Text cleared by the most recent lone `\r`. Emitted at EOF only when
    /// `pending` is empty — i.e. the child's final write ended in `\r`, so this
    /// holds the frame the terminal was actually displaying.
    last_overwritten: Option<Vec<u8>>,
    /// The previous byte was `\r`; a `\n` here completes a CRLF pair.
    after_cr: bool,
}

impl LineAssembler {
    /// Consume `chunk`, returning every line it completed.
    fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for &byte in chunk {
            match byte {
                b'\n' => {
                    if self.after_cr {
                        // CRLF: the `\r` already cleared `pending` into
                        // `last_overwritten`; that text was a real line after all.
                        self.after_cr = false;
                        if let Some(text) = self.last_overwritten.take() {
                            out.push(text);
                            continue;
                        }
                    }
                    out.push(std::mem::take(&mut self.pending));
                    self.last_overwritten = None;
                }
                b'\r' => {
                    // Hold the text aside rather than dropping it: a following
                    // `\n` makes it a completed line, and at EOF it may be the
                    // last frame the terminal showed.
                    self.after_cr = true;
                    self.last_overwritten = Some(std::mem::take(&mut self.pending));
                }
                _ => {
                    self.after_cr = false;
                    self.pending.push(byte);
                }
            }
        }
        out
    }

    /// The final line, if the child's last write left one.
    fn finish(&mut self) -> Option<Vec<u8>> {
        if !self.pending.is_empty() {
            return Some(std::mem::take(&mut self.pending));
        }
        // Ended on a bare `\r`: the overwritten frame is what was on screen.
        self.last_overwritten.take().filter(|t| !t.is_empty())
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

    /// Feed `input` through the assembler in one chunk and collect the lines.
    fn assemble(input: &[u8]) -> Vec<String> {
        let mut asm = LineAssembler::default();
        let mut out: Vec<String> = asm
            .feed(input)
            .into_iter()
            .map(|l| String::from_utf8_lossy(&l).into_owned())
            .collect();
        if let Some(last) = asm.finish() {
            out.push(String::from_utf8_lossy(&last).into_owned());
        }
        out
    }

    /// Collect the text of `Chunk::Line`s, ignoring activity/pid bookkeeping.
    fn lines_of(chunks: Vec<Chunk>) -> Vec<String> {
        chunks
            .into_iter()
            .filter_map(|c| match c {
                Chunk::Line(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn newline_terminates_a_line() {
        assert_eq!(assemble(b"one\ntwo\n"), vec!["one", "two"]);
    }

    #[test]
    fn crlf_is_a_single_terminator() {
        // Not "overwrite, then an empty line" — this is what the PTY emits when
        // ONLCR is on, and what any Windows-style tool emits regardless.
        assert_eq!(assemble(b"one\r\ntwo\r\n"), vec!["one", "two"]);
    }

    #[test]
    fn a_progress_bar_collapses_to_its_final_frame() {
        // The defect this whole change exists for: three redraws of one visual
        // line used to store three lines.
        assert_eq!(
            assemble(b"a [....]\ra [==..]\ra [====]\n"),
            vec!["a [====]"]
        );
    }

    #[test]
    fn overwritten_text_does_not_reach_the_output() {
        assert_eq!(assemble(b"bar\rreal\n"), vec!["real"]);
    }

    #[test]
    fn eof_flushes_a_line_with_no_trailing_newline() {
        assert_eq!(
            assemble(b"start\nwork [..]\rwork [==]"),
            vec!["start", "work [==]"]
        );
    }

    #[test]
    fn trailing_cr_still_yields_the_last_frame() {
        // The child's final write ended in `\r`, so `pending` is empty at EOF —
        // without `last_overwritten` the frame the terminal was showing is lost.
        assert_eq!(assemble(b"done 50%\rdone 100%\r"), vec!["done 100%"]);
    }

    #[test]
    fn split_across_chunks_matches_a_single_chunk() {
        // The assembler is fed the BufReader's fill buffer, so a bar can straddle
        // a chunk boundary; state must carry across `feed` calls.
        let mut asm = LineAssembler::default();
        let mut got: Vec<String> = Vec::new();
        for chunk in [&b"a [..]\ra [="[..], &b"=]\nnext\n"[..]] {
            got.extend(
                asm.feed(chunk)
                    .into_iter()
                    .map(|l| String::from_utf8_lossy(&l).into_owned()),
            );
        }
        assert_eq!(got, vec!["a [==]", "next"]);
    }

    #[test]
    fn cr_split_across_chunks_is_still_crlf() {
        // Worst case: the `\r` and its `\n` land in different chunks.
        let mut asm = LineAssembler::default();
        let mut got: Vec<String> = Vec::new();
        for chunk in [&b"line\r"[..], &b"\nnext\n"[..]] {
            got.extend(
                asm.feed(chunk)
                    .into_iter()
                    .map(|l| String::from_utf8_lossy(&l).into_owned()),
            );
        }
        assert_eq!(got, vec!["line", "next"]);
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\x1b[31merror\x1b[0m"), "error");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[1;32mok\x1b[0m done"), "ok done");
    }

    #[test]
    fn pump_forwards_nonempty_lines() {
        use std::io::Cursor;
        let (tx, mut rx) = mpsc::channel::<Chunk>(16);
        let data = Cursor::new(b"\x1b[32mfirst\x1b[0m\n\nsecond\n".to_vec());
        // Blank line (between the two \n) is dropped.
        std::thread::spawn(move || pump(data, &tx));
        let mut got = Vec::new();
        while let Some(chunk) = rx.blocking_recv() {
            got.push(chunk);
        }
        assert_eq!(
            lines_of(got),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    /// Runs a real command through a real PTY and returns the stored lines.
    ///
    /// The in-memory tests above cannot see the line discipline, which is exactly
    /// why the `\r` defect survived: with `ONLCR` on, a child's `\n` arrives as
    /// CRLF, so "split on `\r`" looked correct and "split only on `\n`" would
    /// have merged every job into one line. This is the only test that would
    /// catch that.
    #[cfg(unix)]
    async fn run_through_pty(script: &str) -> Vec<String> {
        let (lines, _) = run_script(script, Duration::from_secs(30), None).await;
        lines
    }

    /// Drive a script through the full runner and return its lines and outcome.
    #[cfg(unix)]
    async fn run_script(
        script: &str,
        job_timeout: Duration,
        idle_timeout: Option<Duration>,
    ) -> (Vec<String>, Outcome) {
        run_script_with_input(script, job_timeout, idle_timeout, None).await
    }

    /// As [`run_script`], but types `input` at the process before reading output.
    #[cfg(unix)]
    async fn run_script_with_input(
        script: &str,
        job_timeout: Duration,
        idle_timeout: Option<Duration>,
        input: Option<&str>,
    ) -> (Vec<String>, Outcome) {
        use std::sync::Arc;

        let job = Job::new_for_test("pty-test", 10_000);
        let opts = PtyOptions {
            cmd: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            env: Vec::new(),
            cols: 220,
            rows: 50,
            timeout: job_timeout,
            kill_grace: Duration::from_secs(2),
            idle_timeout,
            input: input.map(str::to_string),
            verbose: false,
        };
        let outcome = run(Arc::clone(&job), opts).await;
        let (lines, _, _) = job.read_window(0, 10_000).await;
        (lines.iter().map(|l| l.text.clone()).collect(), outcome)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn progress_bar_is_one_line_through_a_real_pty() {
        let lines =
            run_through_pty("printf 'dl [....]\\rdl [==..]\\rdl [====]\\n'; printf 'REAL\\n'")
                .await;
        assert_eq!(
            lines,
            vec!["dl [====]", "REAL"],
            "a 3-frame bar plus one real line must store 2 lines"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plain_output_is_not_merged_through_a_real_pty() {
        // The regression guard: naively splitting only on `\n` under ONLCR
        // produces a single giant line. Three writes must stay three lines.
        let lines = run_through_pty("printf 'one\\ntwo\\nthree\\n'").await;
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    /// A prompting command used to hang until the job timeout, because the PTY
    /// made it look interactive and nothing could ever answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn queued_input_answers_a_prompt() {
        let (lines, outcome) = run_script_with_input(
            "printf 'Set as default? (Y/n): '; read a; printf 'got=%s\\n' \"$a\"",
            Duration::from_secs(30),
            None,
            Some("Y\n"),
        )
        .await;
        assert!(
            lines.iter().any(|l| l.contains("got=Y")),
            "the answer must reach the prompt: {lines:?}"
        );
        assert!(
            matches!(outcome, Outcome::Completed { exit_code: 0 }),
            "{outcome:?}"
        );
    }

    /// A PTY has no EOF to give: the slave stays open for as long as the child
    /// holds it, so an unanswered prompt blocks until a deadline kills it. That is
    /// why `input` exists — and why `idle_timeout` is the guard for the case where
    /// a prompt was not anticipated.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unanswered_prompt_is_caught_by_the_idle_timeout() {
        let (_, outcome) = run_script_with_input(
            "printf 'Set as default? (Y/n): '; read a; printf 'got=%s\\n' \"$a\"",
            Duration::from_secs(60),
            Some(Duration::from_secs(1)),
            None,
        )
        .await;
        assert!(
            matches!(outcome, Outcome::IdleTimedOut { .. }),
            "a wedged prompt must be reported as idle, not run to the wall clock: {outcome:?}"
        );
    }

    /// Drive a script through the full runner and return its lines and outcome.
    ///
    /// Waits for the child to announce readiness (its first line) before letting
    /// the deadline start, so a test that needs a `trap` to be installed is not
    /// racing process startup — under parallel load that race made these tests
    /// flaky in a way that had nothing to do with what they assert.
    #[cfg(unix)]
    async fn run_script_when_ready(
        script: &str,
        after_ready: Duration,
        idle_timeout: Option<Duration>,
    ) -> (Vec<String>, Outcome) {
        use std::sync::Arc;

        let job = Job::new_for_test("pty-test", 10_000);
        let opts = PtyOptions {
            cmd: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            env: Vec::new(),
            cols: 220,
            rows: 50,
            // Generous: the deadline that matters is enforced by the cancel below.
            timeout: Duration::from_secs(60),
            kill_grace: Duration::from_secs(2),
            idle_timeout,
            input: None,
            verbose: false,
        };

        let runner = Arc::clone(&job);
        let handle = tokio::spawn(async move { run(runner, opts).await });

        // Wait for the child's first line: proof it is running and set up.
        for _ in 0..600 {
            if job.line_count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(job.line_count() > 0, "child never produced its ready line");

        tokio::time::sleep(after_ready).await;
        job.cancel();

        let outcome = handle.await.expect("runner task");
        let (lines, _, _) = job.read_window(0, 10_000).await;
        (lines.iter().map(|l| l.text.clone()).collect(), outcome)
    }

    // ─── Option 2: signal ladder, cancel durability, idle detection ───────────

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_lets_the_child_clean_up_first() {
        // The defect: SIGKILL-only meant a trap handler never ran, so sbt never
        // flushed and cargo never released its locks.
        let (lines, outcome) = run_script_when_ready(
            "trap 'echo FLUSHED; exit 0' TERM; echo started; sleep 30",
            Duration::from_millis(50),
            None,
        )
        .await;
        assert!(
            lines.iter().any(|l| l == "FLUSHED"),
            "TERM handler must have run; got {lines:?}"
        );
        assert!(
            matches!(outcome, Outcome::Canceled { escalated: false }),
            "a well-behaved child should not need SIGKILL: {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_escalates_when_sigterm_is_ignored() {
        // A TERM-deaf child must still die, and the outcome must record that it
        // was escalated — that is how a caller learns cleanup did not happen.
        let (_, outcome) = run_script_when_ready(
            "trap '' TERM; echo deaf; sleep 30",
            Duration::from_millis(50),
            None,
        )
        .await;
        assert!(
            matches!(outcome, Outcome::Canceled { escalated: true }),
            "a TERM-deaf child must be SIGKILLed and say so: {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idle_timeout_kills_a_silent_job() {
        let (_, outcome) = run_script(
            "echo working; sleep 30",
            Duration::from_secs(30),
            Some(Duration::from_millis(800)),
        )
        .await;
        assert!(
            matches!(outcome, Outcome::IdleTimedOut { .. }),
            "a silent job should hit the idle timeout: {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idle_timeout_spares_a_chatty_job() {
        // The false-positive guard: steady output must keep resetting the clock,
        // so the job completes rather than being killed mid-run.
        let (lines, outcome) = run_script(
            "for i in 1 2 3 4 5 6; do echo tick$i; sleep 0.1; done",
            Duration::from_secs(60),
            Some(Duration::from_secs(15)),
        )
        .await;
        assert!(
            matches!(outcome, Outcome::Completed { exit_code: 0 }),
            "chatty job must not be killed: {outcome:?}"
        );
        assert_eq!(lines.len(), 6, "got {lines:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_place_redraws_count_as_activity() {
        // A spinner emits no completed line, so line-count-based idle detection
        // would kill it. Activity chunks are what prevent that.
        let (_, outcome) = run_script(
            "i=0; while [ $i -lt 8 ]; do printf 'spin %s\\r' $i; i=$((i+1)); sleep 0.1; done; echo done",
            Duration::from_secs(60),
            Some(Duration::from_secs(15)),
        )
        .await;
        assert!(
            matches!(outcome, Outcome::Completed { .. }),
            "a redrawing spinner must not read as idle: {outcome:?}"
        );
    }

    #[test]
    fn cancel_before_spawn_is_replayed_not_lost() {
        // The window the old code dropped: a cancel arriving before the PID is
        // known must be remembered and delivered by `set`.
        let kill = KillHandle::default();
        assert!(
            kill.terminate_group(),
            "a pre-spawn cancel is recorded, not refused"
        );
        assert!(
            kill.0.pending.load(Ordering::SeqCst),
            "the pending flag must be set"
        );
        // `set` on a PID that cannot be signalled still clears the flag; we are
        // asserting the replay happens, not that this fake process dies.
        kill.set(999_999);
        assert!(
            !kill.0.pending.load(Ordering::SeqCst),
            "pending cancel must be consumed by set()"
        );
    }

    #[test]
    fn signal_group_rejects_unspawned_pids() {
        assert!(!signal_group(0, libc::SIGTERM));
        assert!(!signal_group(-1, libc::SIGTERM));
    }
}
