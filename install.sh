#!/usr/bin/env bash
set -euo pipefail

# claude-sidecar install script
# Full setup for a new machine: builds the binary, wires PATH, auto-start,
# a PreToolUse hook, and ~/.claude/CLAUDE.md.
# Safe to re-run — all steps are idempotent.
# On macOS, installs missing tools via Homebrew automatically.

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kill-previous.sh
source "${REPO_DIR}/kill-previous.sh"
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

# ── 2. Build ──────────────────────────────────────────────────────────────────
step "Building claude-sidecar"
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
green "  built → $BINARY"

# Drop an install left behind under the upstream `-rs` crate name. It is the same
# daemon bound to the same port, so leaving it on PATH means a shell RC autostart
# line, a stale alias, or plain tab-completion can start the *old* build and the
# new one then fails to bind. Removing the file is what makes that unrepeatable;
# `kill_previous_sidecar` only stops the copy that happens to be running now.
STALE_BINARY="${BIN_DIR}/claude-sidecar-rs"
if [[ -e "$STALE_BINARY" ]]; then
  rm -f "$STALE_BINARY"
  yellow "  removed stale install → $STALE_BINARY"
fi

# ── 3. PATH ───────────────────────────────────────────────────────────────────
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

# ── 4. Auto-start in shell RC ─────────────────────────────────────────────────
step "Wiring auto-start in shell RC"

if [[ -n "$SHELL_RC" ]]; then
  # Remove any previous autostart block so re-running upgrades it in place.
  if grep -qF 'claude-sidecar auto-start' "$SHELL_RC" 2>/dev/null; then
    python3 - "$SHELL_RC" << 'PYEOF'
import pathlib, re, sys

p = pathlib.Path(sys.argv[1])
lines = p.read_text().split("\n")

BEGIN = "# >>> claude-sidecar auto-start >>>"
END = "# <<< claude-sidecar auto-start <<<"
# Any marker this block has ever been written with, decorations and all.
MARKER = re.compile(r"^#\s*[─<>-]*\s*claude-sidecar auto-start\s*[─<>-]*\s*$")
# A line that can plausibly belong to the block. Anything else ends it.
OURS = re.compile(r"^(#|if\b|fi$|\s+claude-sidecar\b|pgrep\b|claude-sidecar\b)")

# Why a bounded line scan and not one regex: the block has shipped in two shapes,
# and only the newer one ends in `fi`. A pattern anchored on `fi` therefore ran
# past the older one-liner form and matched the *next* `fi` in the file — which,
# once both forms were present, meant everything between them. On a real shell rc
# that was 104 lines of unrelated user functions, silently deleted on the second
# install. The scan below can only ever consume lines it recognizes, stops at the
# first blank line, and is capped, so the worst case is leaving a stale block
# behind rather than eating someone's config.
out, i, removed = [], 0, 0
while i < len(lines):
    if not MARKER.match(lines[i]):
        out.append(lines[i])
        i += 1
        continue
    # Sentinel form: exact, delimited, no guessing needed.
    if lines[i].strip() == BEGIN:
        j = i + 1
        while j < len(lines) and lines[j].strip() != END:
            j += 1
        i = j + 1 if j < len(lines) else j
        removed += 1
        continue
    # Legacy form: consume only recognizable lines, and stop at the terminator.
    i += 1
    for _ in range(20):
        if i >= len(lines):
            break
        line = lines[i]
        if not line.strip() or not OURS.match(line):
            break
        i += 1
        # `fi` closes the if/fi shape; a trailing `&` closes the one-liner shape.
        if line.strip() == "fi" or line.rstrip().endswith("&"):
            break
    removed += 1

text = "\n".join(out)
# Collapse the blank runs the removal leaves behind.
text = re.sub(r"\n{3,}", "\n\n", text).rstrip() + "\n"
p.write_text(text)
print(f"  removed {removed} previous auto-start block(s)")
PYEOF
  fi

  # Sentinels, so the next upgrade delimits this block exactly instead of
  # inferring where it ends.
  cat >> "$SHELL_RC" << 'AUTOSTART'

# >>> claude-sidecar auto-start >>>
# `pgrep -a` is required: without it macOS pgrep excludes the caller's own
# ancestors, so the sidecar's env-capture shell (a child of the sidecar) never
# sees the running server and launches a rival that dies on EADDRINUSE.
# SIDECAR_ENV_CAPTURE marks that capture shell — never autostart from it.
# `--detach` puts the daemon in its own session: started from here it would
# otherwise inherit this terminal and compete with sidecar-tui for it, and the
# TUI is what loses — SIGTTIN on its next stdin read, then suspended.
if [[ -z "${SIDECAR_ENV_CAPTURE:-}" ]] && ! pgrep -ax claude-sidecar >/dev/null 2>&1; then
  claude-sidecar --detach >>"${TMPDIR:-/tmp}/claude-sidecar-$$.log" 2>&1
fi
# <<< claude-sidecar auto-start <<<
AUTOSTART
  green "  auto-start wired in $SHELL_RC"
fi

# ── 5. PreToolUse hook ────────────────────────────────────────────────────────
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

# ── 6. ~/.claude/CLAUDE.md ────────────────────────────────────────────────────
step "Patching ~/.claude/CLAUDE.md"

# Patch using the canonical block stored in the repo. The block carries explicit
# BEGIN/END sentinels so its own bash examples cannot be mistaken for its edges.
python3 - "$CLAUDE_MD" "${REPO_DIR}/assets/claude-md-block.md" << 'PYEOF'
import sys, pathlib
target, block_path = sys.argv[1], sys.argv[2]
new_block = pathlib.Path(block_path).read_text().strip()
BEGIN = new_block.split('\n')[0]
END = new_block.rsplit('\n', 1)[-1]
HEADING = '# Sidecar — Running Blocked or Long Commands'
# The last line of the block's final section, in every version shipped so far.
LEGACY_TAIL = 'curl -s http://localhost:8765/health'
try:
    text = pathlib.Path(target).read_text()
except FileNotFoundError:
    text = ""

def strip_sentinel_blocks(s):
    """Remove every BEGIN..END region. Repeated installs must converge."""
    n = 0
    while True:
        i = s.find(BEGIN)
        if i < 0:
            return s, n
        j = s.find(END, i)
        if j < 0:
            return s[:i].rstrip() + '\n', n + 1
        s = s[:i].rstrip() + '\n\n' + s[j + len(END):].lstrip('\n')
        n += 1

def strip_legacy(s):
    """Heal pre-sentinel installs.

    The old patcher ended the block with a lookahead for the next `^# `, which
    matched a `# → {"stdout": …}` comment inside the block's own bash fence. So it
    replaced a fragment and appended a whole new copy every run, leaving several
    overlapping copies with *unbalanced* code fences — which is why neither fence
    tracking nor heading detection can find the real edge.

    Every version of this block has ended with the `## Health check` section, so
    that is the anchor: cut from the first legacy heading through the fence that
    closes the last Health check section.
    """
    i = s.find(HEADING)
    if i < 0:
        return s, 0
    anchor = s.rfind(LEGACY_TAIL)
    if anchor < i:
        return s, 0
    close = s.find('```', anchor + len(LEGACY_TAIL))
    end = len(s) if close < 0 else close + 3
    return (s[:i].rstrip() + '\n\n' + s[end:].lstrip('\n')), 1

text, removed = strip_sentinel_blocks(text)
text, legacy = strip_legacy(text)
text = text.rstrip()
out = (text + '\n\n' if text else '') + new_block + '\n'
pathlib.Path(target).write_text(out)
if removed or legacy:
    print(f"  replaced block (removed {removed} managed, {legacy} legacy)")
else:
    print("  appended new block")
PYEOF
green "  $CLAUDE_MD patched"

# ── 7. Smoke test ─────────────────────────────────────────────────────────────
step "Starting sidecar and running smoke tests"

# The shared helper, not a bare `pkill`: that sent one SIGTERM, never resumed a
# stopped process so it could handle the signal, never escalated to SIGKILL, and
# then slept a flat 0.3s whether or not anything had exited. A survivor keeps
# port 8765, so the smoke test below would be answered by the *old* binary and
# the install would report success without having tested what it just built.
if ! kill_previous_sidecar; then
  red "  a previous sidecar survived SIGKILL — cannot smoke-test this build"; exit 1
fi
if ! wait_port_free 8765; then
  red "  port 8765 is still held (see above) — cannot smoke-test this build"; exit 1
fi
# `--detach`, and the pid is deliberately not captured. The old form backgrounded
# the daemon with a bare `&` and recorded `$!` that nothing ever read, so the
# install ended with a sidecar still holding the installing terminal as its
# controlling tty — the exact process-group contention that suspends sidecar-tui,
# left behind by the script that documents the problem. Detaching makes the
# survivor correct rather than accidental: it is the daemon the user wants
# running, in its own session, and `kill_previous_sidecar` can stop it by name.
"$BINARY" --detach >>"${TMPDIR:-/tmp}/claude-sidecar-smoke.log" 2>&1
SIDECAR_LOG="${TMPDIR:-/tmp}/claude-sidecar-smoke.log"
# Wait for the socket, not a flat sleep: the detaching parent returns immediately,
# so `sleep 0.6` was racing the child's bind on a cold start.
for _ in $(seq 1 25); do
  curl -s http://localhost:8765/health 2>/dev/null | grep -q '"status":"ok"' && break
  sleep 0.2
done

if curl -s http://localhost:8765/health | grep -q '"version":"3"'; then
  green "  /health OK (v3)"
else
  red "  /health failed — check $SIDECAR_LOG"; exit 1
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

HOOK_OUT=$(echo '{"tool_name":"Bash","tool_input":{"command":"sbt validate"},"session_id":"install-selftest"}' | python3 "$HOOK")
# Read the decision out of the JSON rather than grepping the serialized form:
# json.dumps puts a space after the colon, which a literal pattern misses.
HOOK_DECISION=$(printf '%s' "$HOOK_OUT" | python3 -c \
  'import json,sys; print(json.load(sys.stdin).get("hookSpecificOutput",{}).get("permissionDecision",""))' \
  2>/dev/null)
[[ "$HOOK_DECISION" == "deny" ]] && green "  hook OK" || { red "  hook failed: $HOOK_OUT"; exit 1; }

# The hook must put the session id into the command it suggests, or every call
# lands unattributed and the metrics store cannot group by session.
if printf '%s' "$HOOK_OUT" | grep -q 'install-selftest'; then
  green "  hook passes session_id OK"
else
  red "  hook did not include session_id in its suggested command"; exit 1
fi

# End-to-end: a session-tagged call must be queryable by session.
curl -s -o /dev/null -X POST http://localhost:8765/exec \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"git","args":["--version"],"session_id":"install-selftest"}'
SESSIONS=$("$BINARY" query \
  "SELECT session FROM calls WHERE session = 'install-selftest' LIMIT 1" 2>/dev/null || true)
if printf '%s' "$SESSIONS" | grep -q 'install-selftest'; then
  green "  session attribution OK"
else
  yellow "  session attribution not visible yet (metrics may be disabled)"
fi

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
printf '  Log:           %s\n' "${TMPDIR:-/tmp}/claude-sidecar-<pid>.log"
printf '\n'
[[ -n "$SHELL_RC" ]] && yellow "  Restart your shell or: source $SHELL_RC"
printf '\n'
