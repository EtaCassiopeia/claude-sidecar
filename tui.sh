#!/usr/bin/env bash
# Build the sidecar-tui binary and launch it.
#
# Usage:
#   ./tui.sh            — build (release) and run
#   ./tui.sh --dev      — build (debug, faster) and run
#   ./tui.sh --port N   — connect to sidecar on port N (default 8765)
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
BINARY="${BIN_DIR}/sidecar-tui"
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

echo "Building sidecar-tui ($PROFILE)..."
cd "$REPO_DIR"
# shellcheck disable=SC2086
cargo build $CARGO_PROFILE_FLAG --features tui --bin sidecar-tui

mkdir -p "$BIN_DIR"
cp "target/${PROFILE}/sidecar-tui" "$BINARY"
echo "Installed → $BINARY"
echo ""

PORT_FLAG=""
[[ -n "$PORT" ]] && PORT_FLAG="--port $PORT"

# shellcheck disable=SC2086
exec "$BINARY" $PORT_FLAG

