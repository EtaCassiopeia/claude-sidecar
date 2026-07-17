#!/usr/bin/env bash
# Pre-flight auth checks before starting claude-sidecar.
# Validates: gh CLI, Docker daemon.
#
# Exit codes:
#   0 — all checks passed (or user fixed them interactively)
#   1 — one or more checks failed and user declined to fix
#
# Usage:
#   ./check-auth.sh             — interactive (prompt to fix each issue)
#   ./check-auth.sh --non-interactive  — fail fast without prompting
set -uo pipefail

# ── helpers ──────────────────────────────────────────────────────────────────
green()  { printf '\033[32m  ✓ %s\033[0m\n' "$*"; }
yellow() { printf '\033[33m  ⚠ %s\033[0m\n' "$*"; }
red()    { printf '\033[31m  ✗ %s\033[0m\n' "$*"; }
bold()   { printf '\033[1m%s\033[0m\n' "$*"; }

INTERACTIVE=true
for arg in "$@"; do
  case "$arg" in
    --non-interactive|-n) INTERACTIVE=false ;;
  esac
done

FAILED=0

# ── 1. gh CLI ─────────────────────────────────────────────────────────────────
bold "Checking GitHub CLI (gh) auth..."

if ! command -v gh &>/dev/null; then
  red "gh not found in PATH"
  FAILED=1
else
  # Use a live API call as the truth — GH_TOKEN, stored creds, and keychain all
  # work this way.  `gh auth status` only checks stored credentials and fails
  # when GH_TOKEN is the auth source.
  GH_USER=$(gh api user --jq .login 2>/dev/null || true)
  if [[ -n "$GH_USER" ]]; then
    green "gh authenticated as $GH_USER"
  else
    yellow "gh cannot reach GitHub API — credentials missing or expired"
    if $INTERACTIVE; then
      printf '  Run: gh auth login -h github.com\n'
      printf '  Press Enter to run it now, or Ctrl-C to skip: '
      read -r _
      gh auth login -h github.com
      GH_USER=$(gh api user --jq .login 2>/dev/null || true)
      if [[ -n "$GH_USER" ]]; then
        green "gh auth succeeded as $GH_USER"
      else
        red "gh auth failed — sidecar will start but GitHub calls may 401"
        FAILED=1
      fi
    else
      red "gh auth required — run: gh auth login -h github.com"
      FAILED=1
    fi
  fi
fi

# ── 2. Docker daemon ──────────────────────────────────────────────────────────
bold "Checking Docker..."

if ! command -v docker &>/dev/null; then
  yellow "docker not found in PATH — skipping"
elif docker info &>/dev/null 2>&1; then
  DOCKER_VER=$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo "unknown")
  green "Docker daemon running (v$DOCKER_VER)"
else
  yellow "Docker daemon is not reachable"
  if $INTERACTIVE; then
    printf '  Try starting Docker Desktop, then press Enter to re-check, or Ctrl-C to skip: '
    read -r _
    if docker info &>/dev/null 2>&1; then
      green "Docker daemon is now running"
    else
      red "Docker still not reachable — sidecar will start but Docker commands may fail"
      FAILED=1
    fi
  else
    red "Docker daemon not running"
    FAILED=1
  fi
fi

# ── Summary ───────────────────────────────────────────────────────────────────
printf '\n'
if [[ $FAILED -eq 0 ]]; then
  green "Auth pre-flight complete"
  exit 0
else
  if $INTERACTIVE; then
    yellow "Some checks failed — sidecar will start but certain tools may not work"
    exit 0  # still let the sidecar start in interactive mode
  else
    red "Auth pre-flight failed"
    exit 1
  fi
fi

