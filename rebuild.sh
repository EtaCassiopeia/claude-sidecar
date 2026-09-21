#!/usr/bin/env bash
set -euo pipefail

# Usage:
#   ./rebuild.sh                 — build, deploy, restart in background
#   ./rebuild.sh -v              — build, deploy, run in foreground with verbose output
#   ./rebuild.sh --skip-auth     — skip auth pre-flight checks
#   ./rebuild.sh --verbose-auth  — show the full auth report, not just problems

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kill-previous.sh
source "${REPO_DIR}/kill-previous.sh"
BINARY="${HOME}/.local/bin/claude-sidecar"
FOREGROUND=false
VERBOSE_FLAG=""
SKIP_AUTH=false
VERBOSE_AUTH=false

for arg in "$@"; do
  case "$arg" in
    -v|--verbose|--watch) FOREGROUND=true; VERBOSE_FLAG="-v" ;;
    --skip-auth)          SKIP_AUTH=true ;;
    --verbose-auth)       VERBOSE_AUTH=true ;;
  esac
done

# ── Auth pre-flight ───────────────────────────────────────────────────────────
if ! $SKIP_AUTH; then
  # `--quiet`: say nothing when everything is set up. These checks exist to
  # remind you about an expired gh token or a forgotten `awsproxy ent`, and that
  # reminder only lives in the failure case — a banner plus a column of green
  # ticks on every rebuild is noise you learn to skip past. `--verbose-auth`
  # brings the full report back; `--skip-auth` skips the checks entirely.
  #
  # `--non-interactive`: the result is already non-fatal, so this must not be
  # able to block the build on a prompt. A failing check (expired gh creds, a
  # TLS-rejected API call) otherwise drops into `read`/`gh auth login` and the
  # rebuild appears hung. Run ./check-auth.sh directly to fix anything it flags.
  AUTH_FLAGS=(--non-interactive)
  # `if`, not `&&`: a bare false command aborts the script under `set -e`.
  if $VERBOSE_AUTH; then
    printf '\n\033[1m==> Pre-flight auth checks\033[0m\n'
  else
    AUTH_FLAGS+=(--quiet)
  fi
  bash "${REPO_DIR}/check-auth.sh" "${AUTH_FLAGS[@]}" || true
fi

echo "Building..."
cd "$REPO_DIR"
cargo build --release
# Install via a temp file plus an atomic rename, never a straight `cp` onto
# "$BINARY". Overwriting a Mach-O in place while a copy of it is still running
# rewrites the pages that process has mapped, which invalidates the binary's
# code signature — macOS then SIGKILLs the *newly installed* binary on its next
# exec (observed: `claude-sidecar --version` dying with 137 right after the cp).
# `mv` onto the path allocates a new inode, so the running process keeps the old
# one until it is stopped and the new file is untouched.
cp target/release/claude-sidecar "${BINARY}.new"
mv -f "${BINARY}.new" "$BINARY"
echo "Installed → $BINARY"

# Matches both the installed copy and one run straight out of target/release:
# the crate's binary carries the same name, so a single pattern covers both.
kill_previous claude-sidecar

if $FOREGROUND; then
  echo "Starting in foreground (verbose) — Ctrl-C to stop"
  echo ""
  exec "$BINARY" $VERBOSE_FLAG
else
  # `--detach`, not just `&`: a backgrounded job keeps the launching terminal as
  # its controlling tty, which makes the daemon a process group competing for it.
  # `sidecar-tui` is what loses that fight — it takes SIGTTIN on its next stdin
  # read and suspends. `--detach` forks into a new session, so the log file below
  # is the only place its output goes.
  "$BINARY" --detach >>"${TMPDIR:-/tmp}/claude-sidecar-$$.log" 2>&1
  # The parent exits as soon as the child is detached, so this waits for the
  # server to bind rather than for the process to appear.
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if curl -s http://localhost:8765/health 2>/dev/null | grep -q '"status":"ok"'; then
      break
    fi
    sleep 0.2
  done
  curl -s http://localhost:8765/health
  echo ""
  echo "Log: ${TMPDIR:-/tmp}/claude-sidecar-$$.log"
  echo "Sidecar running in background. For live output: $0 -v"
fi
