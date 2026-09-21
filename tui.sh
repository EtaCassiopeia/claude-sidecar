#!/usr/bin/env bash
# Build the sidecar-tui binary and launch it.
#
# Usage:
#   ./tui.sh            — build (release) and run
#   ./tui.sh --dev      — build (debug, faster) and run
#   ./tui.sh --port N   — connect to sidecar on port N (default 8765)
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kill-previous.sh
source "${REPO_DIR}/kill-previous.sh"
PROFILE="release"
CARGO_PROFILE_FLAG="--release"
PORT=""

for arg in "$@"; do
  case "$arg" in
    --dev)    PROFILE="debug"; CARGO_PROFILE_FLAG="" ;;
    --port)   shift; PORT="$1" ;;
    --port=*) PORT="${arg#--port=}" ;;
  esac
done

# Make sure the sidecar is actually running before we bother building.
if ! curl -s http://localhost:${PORT:-8765}/health 2>/dev/null | grep -q '"status":"ok"'; then
  echo "sidecar is not running on port ${PORT:-8765} — start it first with ./rebuild.sh"
  exit 1
fi

# Before the build, not after: a stray instance holds the terminal for the whole
# compile otherwise, and a suspended one keeps the tty open indefinitely — it
# cannot handle a signal until something resumes it. Only one process group can
# own the tty, so a leftover TUI reading stdin from the background takes SIGTTIN
# and suspends, which is what filled the shell with `[1] … [9] suspended` jobs.
#
# Fatal if one survives. `exec`ing a second TUI onto a tty another is still
# reading is precisely the fight described above, and the new one loses it just
# as readily as the old — better to say so than to hand back a suspended job.
if ! kill_previous_tui; then
  echo "Refusing to start: a previous sidecar-tui survived SIGKILL." >&2
  exit 1
fi

echo "Building sidecar-tui ($PROFILE)..."
cd "$REPO_DIR"
# shellcheck disable=SC2086
cargo build $CARGO_PROFILE_FLAG --features tui --bin sidecar-tui

PORT_FLAG=""
[[ -n "$PORT" ]] && PORT_FLAG="--port $PORT"

# Run straight from the build directory rather than installing to ~/.local/bin.
# An endpoint security policy SIGKILLs this binary from there (`Killed: 9` before
# it executes), while the identical file runs fine from the repo.
# shellcheck disable=SC2086
exec "${REPO_DIR}/target/${PROFILE}/sidecar-tui" $PORT_FLAG
