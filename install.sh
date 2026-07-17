#!/usr/bin/env bash
set -euo pipefail

# claude-sidecar install script
# Full setup for a new machine: builds the binary, wires PATH, auto-start,
# a PreToolUse hook, and ~/.claude/CLAUDE.md.
# Safe to re-run — all steps are idempotent.
# On macOS, installs missing tools via Homebrew automatically.

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
BINARY="${BIN_DIR}/claude-sidecar"
CLAUDE_MD="${HOME}/.claude/CLAUDE.md"
HOOK_DIR="${HOME}/.claude/hooks"
HOOK="${HOOK_DIR}/sidecar-redirect.py"
SETTINGS="${HOME}/.claude/settings.json"

# ── colours ──────────────────────────────────────────────────────────────────
green()  { printf '\033[32m%s\033[0m\n' "$*"; }
yellow() { printf '\033[33m%s\033[0m\n' "$*"; }
red()    { printf '\033[31m%s\033[0m\n' "$*"; }
step()   { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

IS_MACOS=false
[[ "$(uname)" == "Darwin" ]] && IS_MACOS=true

brew_install() {
  local formula="$1"
  if $IS_MACOS; then
    yellow "  installing $formula via Homebrew..."
    brew install "$formula"
  else
    red "  $formula not found — install with your system package manager"; exit 1
  fi
}

# ── 1. Prerequisites ──────────────────────────────────────────────────────────
step "Checking prerequisites"

mkdir -p "$BIN_DIR" "$HOOK_DIR" "${HOME}/.claude"

if $IS_MACOS && ! command -v brew &>/dev/null; then
  yellow "  Homebrew not found — installing..."
  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
fi

if ! command -v rustc &>/dev/null; then
  if $IS_MACOS; then
    yellow "  Rust not found — installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    source "${HOME}/.cargo/env"
  else
    red "  Rust not found. Install: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
  fi
fi
green "  rustc $(rustc --version | awk '{print $2}')"

if ! command -v gh &>/dev/null; then
  yellow "  gh CLI not found"; brew_install gh
fi
green "  gh $(gh --version | head -1 | awk '{print $3}')"

# ── 2. Auth pre-flight ────────────────────────────────────────────────────────
step "Auth pre-flight checks"
bash "${REPO_DIR}/check-auth.sh" || true  # non-fatal: installation continues regardless

# ── 3. Build ──────────────────────────────────────────────────────────────────
step "Building claude-sidecar"
cd "$REPO_DIR"
cargo build --release
cp target/release/claude-sidecar "$BINARY"
green "  built → $BINARY"

# ── 4. PATH ───────────────────────────────────────────────────────────────────
step "Ensuring ~/.local/bin is in PATH"

SHELL_RC=""
case "${SHELL:-}" in
  */zsh)  SHELL_RC="${HOME}/.zshrc" ;;
  */bash) SHELL_RC="${HOME}/.bashrc" ;;
esac

if [[ -n "$SHELL_RC" ]] && ! grep -qF 'local/bin' "$SHELL_RC" 2>/dev/null; then
  printf '\n# claude-sidecar\nexport PATH="$HOME/.local/bin:$PATH"\n' >> "$SHELL_RC"
  yellow "  added PATH export to $SHELL_RC"
else
  green "  ~/.local/bin already in PATH config"
fi

# ── 5. Auto-start in shell RC ─────────────────────────────────────────────────
step "Wiring auto-start in shell RC"

if [[ -n "$SHELL_RC" ]] && ! grep -qF 'claude-sidecar auto-start' "$SHELL_RC" 2>/dev/null; then
  cat >> "$SHELL_RC" << 'AUTOSTART'

# claude-sidecar auto-start
pgrep -x claude-sidecar >/dev/null || claude-sidecar >>"${TMPDIR:-/tmp}/claude-sidecar.log" 2>&1 &
AUTOSTART
  green "  added auto-start to $SHELL_RC"
else
  green "  auto-start already wired"
fi

# ── 6. PreToolUse hook ────────────────────────────────────────────────────────
step "Installing PreToolUse redirect hook"

# Write hook script from the repo copy (canonical source of truth)
cp "${REPO_DIR}/assets/sidecar-redirect.py" "$HOOK"
chmod +x "$HOOK"
green "  installed → $HOOK"

# Wire into settings.json
[[ -f "$SETTINGS" ]] || echo '{}' > "$SETTINGS"
python3 - "$SETTINGS" "$HOOK" << 'PYEOF'
import json, sys
path, hook = sys.argv[1], sys.argv[2]
with open(path) as f:
    s = json.load(f)
hooks = s.setdefault("hooks", {})
pre = hooks.setdefault("PreToolUse", [])
for entry in pre:
    for h in entry.get("hooks", []):
        if hook in h.get("command", ""):
            print("  already wired — skipping"); sys.exit(0)
pre.append({"matcher": "Bash", "hooks": [{"type": "command", "command": f"python3 {hook}", "timeout": 5}]})
with open(path, "w") as f:
    json.dump(s, f, indent=2); f.write("\n")
print("  wired")
PYEOF
green "  hook wired in $SETTINGS"

# ── 7. ~/.claude/CLAUDE.md ────────────────────────────────────────────────────
step "Patching ~/.claude/CLAUDE.md"

# Patch using the canonical block stored in the repo
python3 - "$CLAUDE_MD" "${REPO_DIR}/assets/claude-md-block.md" << 'PYEOF'
import sys, re, pathlib
target, block_path = sys.argv[1], sys.argv[2]
new_block = pathlib.Path(block_path).read_text().strip()
marker = new_block.split('\n')[0]  # first line is the heading
try:
    text = pathlib.Path(target).read_text()
except FileNotFoundError:
    text = ""
pattern = re.compile(r"(?m)^" + re.escape(marker) + r".*?(?=\n^# |\Z)", re.DOTALL | re.MULTILINE)
if pattern.search(text):
    pathlib.Path(target).write_text(pattern.sub(new_block, text, count=1))
    print("  updated existing block")
else:
    with open(target, "a") as f:
        f.write(("\n\n" if text else "") + new_block + "\n")
    print("  appended new block")
PYEOF
green "  $CLAUDE_MD patched"

# ── 8. Smoke test ─────────────────────────────────────────────────────────────
step "Starting sidecar and running smoke tests"

pkill -x claude-sidecar 2>/dev/null && sleep 0.3 || true
"$BINARY" >>"${TMPDIR:-/tmp}/claude-sidecar.log" 2>&1 &
sleep 0.6

if curl -s http://localhost:8765/health | grep -q '"version":"3"'; then
  green "  /health OK (v3)"
else
  red "  /health failed — check ${TMPDIR:-/tmp}/claude-sidecar.log"; exit 1
fi

EXEC_OUT=$(curl -s -X POST http://localhost:8765/exec \
  -H 'Content-Type: application/json' -d '{"cmd":"git","args":["--version"]}')
echo "$EXEC_OUT" | grep -q '"exit_code":0' && green "  /exec OK" || { red "  /exec failed: $EXEC_OUT"; exit 1; }

JOB_ID=$(curl -s -X POST http://localhost:8765/jobs \
  -H 'Content-Type: application/json' -d '{"cmd":"git","args":["--version"]}' \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['job_id'])")
sleep 0.5
STATUS=$(curl -s "http://localhost:8765/jobs/$JOB_ID/status")
echo "$STATUS" | grep -q '"running":false' && green "  /jobs OK" || { red "  /jobs failed: $STATUS"; exit 1; }

HOOK_OUT=$(echo '{"tool_name":"Bash","tool_input":{"command":"sbt validate"}}' | python3 "$HOOK")
echo "$HOOK_OUT" | grep -q '"permissionDecision":"deny"' && green "  hook OK" || { red "  hook failed"; exit 1; }

# ── Done ──────────────────────────────────────────────────────────────────────
printf '\n'
green "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
green " claude-sidecar v3 installed successfully"
green "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
printf '\n'
printf '  Binary:        %s\n' "$BINARY"
printf '  Hook:          %s\n' "$HOOK"
printf '  CLAUDE.md:     %s\n' "$CLAUDE_MD"
printf '  settings.json: %s\n' "$SETTINGS"
printf '  Log:           %s\n' "${TMPDIR:-/tmp}/claude-sidecar.log"
printf '\n'
[[ -n "$SHELL_RC" ]] && yellow "  Restart your shell or: source $SHELL_RC"
printf '\n'


