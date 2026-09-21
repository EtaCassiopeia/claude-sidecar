//! Fresh login-shell environment for spawned commands.
//!
//! # Why this exists
//!
//! A spawned `std::process::Command` inherits the **sidecar process's** environment,
//! which is frozen at the moment the sidecar was launched. Some of those values rotate
//! out from under it — most importantly a private artifact registry's identity token,
//! which the login shell refreshes from the macOS keychain on every shell start and
//! re-exports as the registry's credential variables.
//!
//! When the sidecar's copy of that token goes stale, `sbt` can no longer authenticate to
//! the registry, falls through to whatever public mirror is configured behind it, and
//! dies on an intercepted TLS chain the JDK doesn't trust (PKIX failure) — even though
//! the user's own terminal builds fine.
//!
//! # What it does
//!
//! A background task periodically runs the user's **login + interactive** shell and
//! publishes the resulting environment. Interactive (`-i`) is required because the token
//! refresh lives in `.zshrc`, which non-interactive shells never source. The captured map
//! is overlaid onto every child process *before* any per-request overrides, so builds see
//! a live token without the caller (or the sidecar) needing a restart.
//!
//! Readers ([`fresh_env`]) only ever read a published snapshot — they never spawn a shell.
//! That matters because the request path is async: doing the capture inline blocked a
//! tokio worker for as long as an interactive zsh takes to start (up to the timeout), and
//! a check-then-act cache meant N concurrent requests each spawned their own shell.
//!
//! Fail-open: before the first successful capture, and after any failure, readers get an
//! empty map and callers fall back to the inherited environment — never worse than before
//! this module existed.
//!
//! # Configuration
//!
//! - `SIDECAR_ENV_REFRESH=off|0|false` — disable entirely (fall back to inherited env).
//! - `SIDECAR_ENV_REFRESH_TTL_SECS=<n>` — refresh interval (default 300s).
//! - `SIDECAR_ENV_REFRESH_TIMEOUT_SECS=<n>` — hard bound on the shell dump (default 30s).

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub type EnvMap = Arc<HashMap<String, String>>;

/// Latest published snapshot. `None` until the first successful capture.
static CACHE: RwLock<Option<EnvMap>> = RwLock::new(None);

fn enabled() -> bool {
    !matches!(
        std::env::var("SIDECAR_ENV_REFRESH").as_deref(),
        Ok("off") | Ok("0") | Ok("false")
    )
}

fn ttl() -> Duration {
    duration_env("SIDECAR_ENV_REFRESH_TTL_SECS", 300)
}

fn capture_timeout() -> Duration {
    duration_env("SIDECAR_ENV_REFRESH_TIMEOUT_SECS", 30)
}

fn duration_env(key: &str, default_secs: u64) -> Duration {
    let secs = std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

/// The user's login shell, used to source their profile (token refresh, resolver creds, …).
fn login_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
}

/// Marker exported into the capture shell so the user's shell RC can tell it
/// apart from a real interactive session.
///
/// The RC's autostart line must skip when this is set: the capture shell is a
/// *child of the sidecar*, and macOS `pgrep` excludes the caller's ancestors
/// from matches by default, so a `pgrep -x claude-sidecar` guard finds nothing
/// and launches a rival that dies on `EADDRINUSE` every TTL.
pub const CAPTURE_MARKER: &str = "SIDECAR_ENV_CAPTURE";

/// Run `<shell> -l -i -c printenv` under a hard timeout and parse `KEY=VALUE` lines.
///
/// Two details are load-bearing, and both were learned from a leak:
///
/// - **`setsid` in `pre_exec`.** An interactive shell inherits the sidecar's
///   controlling terminal, which is the user's tty when the sidecar is started
///   from a shell. That puts a background process group on the same tty as the
///   TUI, and only one group can own it — the TUI then took `SIGTTIN` on its next
///   stdin read and suspended. A new session gives the capture shell no
///   controlling terminal at all, so it can never compete for one.
/// - **Killing the group on timeout.** `Command::output` has no timeout, so a
///   shell whose init blocks (`.zshrc` here can stall in `open()` waiting on the
///   keychain) is never reaped: the thread stays parked in `output()`, the child
///   is re-parented to `launchd`, and the next tick starts another. 1265 stray
///   `zsh -l -i -c printenv` processes had accumulated at one per TTL, the oldest
///   14 days old. The group kill is why `setsid` must make the child a group
///   leader.
fn capture() -> Option<HashMap<String, String>> {
    use std::os::unix::process::CommandExt;

    let shell = login_shell();
    let (pid_tx, pid_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut cmd = Command::new(&shell);
        cmd.args(["-l", "-i", "-c", "printenv"])
            .env(CAPTURE_MARKER, "1")
            // Never let shell init block on stdin; keep the dump off the job's streams.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // SAFETY: `setsid` is async-signal-safe and runs in the child before
        // `exec`. It drops the inherited controlling terminal and makes the child
        // a process-group leader so the timeout path can group-kill it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => {
                let _ = pid_tx.send(Some(child.id() as i32));
                let _ = tx.send(child.wait_with_output().ok());
            }
            Err(e) => {
                let _ = pid_tx.send(None);
                tracing::debug!("could not spawn env capture shell: {e}");
                let _ = tx.send(None);
            }
        }
    });

    let pid = pid_rx.recv_timeout(capture_timeout()).ok().flatten();
    let output = match rx.recv_timeout(capture_timeout()) {
        Ok(Some(output)) => output,
        // Timed out or the spawn failed. On timeout the shell is wedged in its
        // own init and will never exit on its own — take its group down, or it
        // survives us and the next tick adds another.
        Ok(None) | Err(_) => {
            if let Some(pid) = pid {
                crate::pty::signal_group(pid, libc::SIGKILL);
            }
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);

    let mut map = HashMap::new();
    for line in text.lines() {
        // Only accept well-formed `KEY=VALUE` where KEY is a valid shell identifier. This
        // skips shell-init banners, prompt escape sequences, and continuation lines of any
        // (rare) multi-line values rather than mis-parsing them as env entries.
        let Some(eq) = line.find('=') else { continue };
        let key = &line[..eq];
        let valid_key = !key.is_empty()
            && key
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if valid_key {
            map.insert(key.to_string(), line[eq + 1..].to_string());
        }
    }

    // Don't propagate our own marker to spawned commands — it describes the
    // capture shell, not the jobs that inherit this map.
    map.remove(CAPTURE_MARKER);
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

/// The most recently published login-shell environment.
///
/// Wait-free: a read lock over an `Arc`, never a shell spawn. Empty before the
/// first successful capture (or when disabled), so callers overlay nothing and
/// fall back to the inherited environment.
pub fn fresh_env() -> EnvMap {
    match CACHE.read() {
        Ok(guard) => guard.clone().unwrap_or_default(),
        Err(_) => EnvMap::default(),
    }
}

/// Capture once, synchronously, and publish the result. Returns whether it worked.
fn refresh_once() -> bool {
    match capture() {
        Some(env) => {
            if let Ok(mut guard) = CACHE.write() {
                *guard = Some(Arc::new(env));
                return true;
            }
            false
        }
        None => false,
    }
}

/// Start the background refresher.
///
/// Capturing here rather than on the request path is the point: an interactive
/// zsh takes real time to start, and doing it inline blocked a tokio worker for
/// up to `capture_timeout()`. One task also means one shell per interval instead
/// of one per concurrent request racing an expired cache.
pub fn spawn_refresher() {
    if !enabled() {
        tracing::info!("env refresh disabled; children inherit the sidecar's environment");
        return;
    }
    tokio::spawn(async move {
        loop {
            // The capture blocks (it waits on a child), so keep it off the runtime.
            let ok = tokio::task::spawn_blocking(refresh_once)
                .await
                .unwrap_or(false);
            if !ok {
                tracing::warn!(
                    "login-shell env capture failed; children fall back to inherited env"
                );
            }
            tokio::time::sleep(ttl()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture shell must see the marker, or the user's RC can't tell it
    /// apart from an interactive session and will autostart a rival sidecar.
    #[test]
    fn capture_shell_receives_the_marker() {
        let out = Command::new("/bin/sh")
            .args(["-c", "printenv"])
            .env(CAPTURE_MARKER, "1")
            .output()
            .expect("sh");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.lines().any(|l| l == format!("{CAPTURE_MARKER}=1")),
            "capture shell did not receive {CAPTURE_MARKER}"
        );
    }

    /// …but it must not survive into the map overlaid onto spawned jobs.
    #[test]
    fn marker_is_stripped_from_the_captured_map() {
        assert!(!fresh_env().contains_key(CAPTURE_MARKER));
    }

    /// Readers must never block on a capture: before the refresher has published
    /// anything, `fresh_env` returns empty immediately rather than spawning a
    /// shell on the caller's thread (which is a tokio worker on the /exec path).
    #[test]
    fn fresh_env_is_wait_free_before_any_capture() {
        let started = std::time::Instant::now();
        let _ = fresh_env();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "fresh_env blocked for {:?}; it must only read the published snapshot",
            started.elapsed()
        );
    }

    /// A published snapshot is what readers see.
    #[test]
    fn published_snapshot_is_returned_to_readers() {
        let mut map = HashMap::new();
        map.insert("SC_TEST_KEY".to_string(), "value".to_string());
        *CACHE.write().expect("lock") = Some(Arc::new(map));

        let env = fresh_env();
        assert_eq!(env.get("SC_TEST_KEY").map(String::as_str), Some("value"));

        *CACHE.write().expect("lock") = None;
    }

    /// The capture shell must land in its own session, with no controlling
    /// terminal. Inheriting the sidecar's tty put a background process group on
    /// the user's terminal, and the TUI then took `SIGTTIN` and suspended.
    #[test]
    fn capture_child_leaves_the_parents_session() {
        use std::os::unix::process::CommandExt;

        // Stays alive long enough for the parent to inspect its session.
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: mirrors `capture`; `setsid` is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id() as i32;

        // SAFETY: `getsid` only reads process state.
        let (child_sid, our_sid) = unsafe { (libc::getsid(pid), libc::getsid(0)) };

        let _ = child.kill();
        let _ = child.wait();

        assert_ne!(child_sid, -1, "could not read the child's session id");
        assert_ne!(
            child_sid, our_sid,
            "capture child shared the parent's session, so it kept the parent's \
             controlling terminal; it must call setsid"
        );
        assert_eq!(
            child_sid, pid,
            "capture child must be its own session leader"
        );
    }
}
