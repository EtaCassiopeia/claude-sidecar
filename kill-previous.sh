#!/usr/bin/env bash
# Terminate previous instances of a binary before starting a new one.
#
# Sourced by rebuild.sh and tui.sh rather than duplicated: both need the same
# SIGCONT-then-escalate sequence, and getting it subtly different in one of them
# is how strays survive.

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
kill_previous() {
  local name pids deadline
  for name in "$@"; do
    pids=$(pgrep -x "$name" 2>/dev/null | grep -vx "$$" || true)
    [[ -z "$pids" ]] && continue

    echo "Stopping previous $name: $(echo "$pids" | tr '\n' ' ')"
    # shellcheck disable=SC2086 — word splitting is wanted; these are bare pids.
    kill -CONT $pids 2>/dev/null || true
    kill -TERM $pids 2>/dev/null || true

    # Wait briefly for a clean exit before forcing it.
    for deadline in 1 2 3 4 5 6 7 8 9 10; do
      pids=$(pgrep -x "$name" 2>/dev/null | grep -vx "$$" || true)
      [[ -z "$pids" ]] && break
      sleep 0.1
    done

    if [[ -n "$pids" ]]; then
      # shellcheck disable=SC2086
      kill -KILL $pids 2>/dev/null || true
    fi
  done
}
