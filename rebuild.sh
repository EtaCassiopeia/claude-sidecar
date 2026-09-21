#!/usr/bin/env bash
set -euo pipefail

# Usage:
#   ./rebuild.sh        — build, deploy, restart in background
#   ./rebuild.sh -v     — build, deploy, run in foreground with verbose output

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kill-previous.sh
source "${REPO_DIR}/kill-previous.sh"
BINARY="${HOME}/.local/bin/claude-sidecar"
PORT="${SIDECAR_PORT:-8765}"
FOREGROUND=false
VERBOSE_FLAG=""

for arg in "$@"; do
  case "$arg" in
    -v|--verbose|--watch) FOREGROUND=true; VERBOSE_FLAG="-v" ;;
  esac
done

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

# Covers the installed copy, one run straight out of target/release, and any
# instance left over under the upstream `-rs` crate name — all three bind the
# same port.
#
# Both checks are fatal rather than advisory. Starting anyway is the case that
# looks like success and isn't: a survivor keeps the port, the new process dies
# on EADDRINUSE, and the health check below is then answered by the *old* binary
# — so a rebuild that changed nothing reports that it worked.
if ! kill_previous_sidecar; then
  echo "Refusing to start: a previous sidecar survived SIGKILL." >&2
  exit 1
fi
if ! wait_port_free "$PORT"; then
  echo "Refusing to start: port $PORT is still held (see above)." >&2
  exit 1
fi

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
    if curl -s http://localhost:${PORT}/health 2>/dev/null | grep -q '"status":"ok"'; then
      break
    fi
    sleep 0.2
  done
  curl -s http://localhost:${PORT}/health
  echo ""
  echo "Log: ${TMPDIR:-/tmp}/claude-sidecar-$$.log"
  echo "Sidecar running in background. For live output: $0 -v"
fi
