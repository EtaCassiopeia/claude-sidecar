#!/usr/bin/env bash
set -euo pipefail

# Usage:
#   ./rebuild.sh               — build, deploy, restart in background
#   ./rebuild.sh -v            — build, deploy, run in foreground with verbose output
#   ./rebuild.sh --skip-auth   — skip auth pre-flight checks

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY="${HOME}/.local/bin/claude-sidecar"
FOREGROUND=false
VERBOSE_FLAG=""
SKIP_AUTH=false

for arg in "$@"; do
  case "$arg" in
    -v|--verbose|--watch) FOREGROUND=true; VERBOSE_FLAG="-v" ;;
    --skip-auth)          SKIP_AUTH=true ;;
  esac
done

# ── Auth pre-flight ───────────────────────────────────────────────────────────
if ! $SKIP_AUTH; then
  printf '\n\033[1m==> Pre-flight auth checks\033[0m\n'
  bash "${REPO_DIR}/check-auth.sh" || true  # non-fatal: let sidecar start regardless
  printf '\n'
fi

echo "Building..."
cd "$REPO_DIR"
cargo build --release
cp target/release/claude-sidecar "$BINARY"
echo "Installed → $BINARY"

pkill -x claude-sidecar 2>/dev/null && sleep 0.3 || true

if $FOREGROUND; then
  echo "Starting in foreground (verbose) — Ctrl-C to stop"
  echo ""
  exec "$BINARY" $VERBOSE_FLAG
else
  "$BINARY" >>"${TMPDIR:-/tmp}/claude-sidecar.log" 2>&1 &
  sleep 0.5
  curl -s http://localhost:8765/health
  echo ""
  echo "Sidecar running in background. For live output: $0 -v"
fi


