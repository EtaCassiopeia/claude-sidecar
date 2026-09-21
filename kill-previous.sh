#!/usr/bin/env bash
# Stop previous instances before starting a new one.
#
# Sourced by install.sh, rebuild.sh and tui.sh rather than duplicated: all three
# need the same SIGCONT-then-escalate sequence, and getting it subtly different
# in one of them is how strays survive.

# Signal one pid, ignoring "no such process".
#
# Signals go one pid per call rather than `kill $pids`. Splitting a newline-
# separated list into separate arguments is bash word-splitting, which zsh does
# not do to parameter expansions — there `$pids` stays a single argument with
# embedded newlines, `kill` rejects it, and the `|| true` swallows the error. The
# function then printed "Stopping previous …" and killed nothing. These files
# carry a bash shebang so that path was fine, but macOS logs you into zsh, and a
# helper meant to be sourced should not depend on which shell sourced it.
_kp_signal() {
  kill "-$1" "$2" 2>/dev/null || true
}

# Pids of running processes with this exact name, minus our own.
#
# `pgrep -x` matches the process name, so it finds an instance however it was
# launched — absolute path, relative path, through PATH, or re-parented to launchd
# after `--detach`. All four are verified to match.
_kp_pids() {
  pgrep -x "$1" 2>/dev/null | grep -vx "$$" || true
}

# Kill every running process with the exact name(s) given.
#
# `kill -CONT` first, and this is the part that matters: a *stopped* process
# never handles a signal until it is resumed, so a plain SIGTERM to a suspended
# instance leaves it suspended and holding the terminal open. Debugging sessions
# accumulated a row of `[1] … [9] suspended` jobs for exactly that reason, and a
# stray instance is not inert — only one process group can own the tty, so a
# leftover reading stdin from the background takes SIGTTIN and drags the new
# instance into the same fight.
#
# Escalates TERM → KILL so a wedged process cannot outlive the call, and skips
# our own pid so a script can never kill itself.
#
# Returns non-zero if anything survived. Callers start a new instance next, and
# a survivor still holds the port — the newcomer then dies on EADDRINUSE, or
# worse, the caller's health check is answered by the *old* process and a failed
# restart reports success.
kill_previous() {
  local name pid pids rc=0
  for name in "$@"; do
    pids="$(_kp_pids "$name")"
    [[ -z "$pids" ]] && continue

    echo "Stopping previous $name: $(echo "$pids" | tr '\n' ' ')"
    echo "$pids" | while IFS= read -r pid; do
      [[ -n "$pid" ]] || continue
      _kp_signal CONT "$pid"
      _kp_signal TERM "$pid"
    done

    # Wait briefly for a clean exit before forcing it.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      pids="$(_kp_pids "$name")"
      [[ -z "$pids" ]] && break
      sleep 0.1
    done

    if [[ -n "$pids" ]]; then
      echo "$pids" | while IFS= read -r pid; do
        [[ -n "$pid" ]] || continue
        _kp_signal KILL "$pid"
      done
      # SIGKILL is not deliverable-then-pending the way TERM is, but the process
      # table takes a moment to catch up; without this the check below can still
      # see a pid that is already gone.
      for _ in 1 2 3 4 5; do
        pids="$(_kp_pids "$name")"
        [[ -z "$pids" ]] && break
        sleep 0.1
      done
    fi

    if [[ -n "$pids" ]]; then
      echo "  warning: $name still running after SIGKILL: $(echo "$pids" | tr '\n' ' ')" >&2
      rc=1
    fi
  done
  return $rc
}

# Stop every sidecar, under every name the daemon has shipped under.
#
# `claude-sidecar-rs` is the crate name the upstream tree carries. A machine that
# ever built or ran that tree has processes — and an installed copy — under that
# name, and they bind the same port as the current one. Stopping only the current
# name leaves a stray that the next start collides with, and because the stray
# answers `/health` identically, the collision reports as success.
#
# Names are written out rather than held in a variable and splatted: splitting
# `$NAMES` into separate words is bash word-splitting, which zsh does not do to
# parameter expansions — the same trap documented on `_kp_signal` above.
kill_previous_sidecar() {
  kill_previous claude-sidecar claude-sidecar-rs
}

# Stop every TUI. One name so far; the wrapper exists so that a future rename has
# a single place to add the old name instead of three call sites to remember.
kill_previous_tui() {
  kill_previous sidecar-tui
}

# Wait for a TCP port to have no listener, so a freshly started instance binds
# instead of dying on EADDRINUSE.
#
# Worth checking separately from `kill_previous`: the port is the resource that
# actually matters, and it can be held by something that pgrep never sees — an
# instance running under a different name, a half-closed socket, or an unrelated
# service. Returns non-zero if the port is still held when the wait runs out.
wait_port_free() {
  local port="$1" attempts="${2:-30}" i
  for ((i = 0; i < attempts; i++)); do
    lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 || return 0
    sleep 0.1
  done
  echo "  warning: port $port is still in use by:" >&2
  lsof -nP -iTCP:"$port" -sTCP:LISTEN >&2 2>/dev/null || true
  return 1
}
