#!/usr/bin/env python3
"""
PreToolUse hook: intercepts direct sbt/cargo/pytest/go test Bash calls
and redirects Claude to use the sidecar /jobs create-then-poll pattern.

The suggested command carries this session's id, so the sidecar can attribute
the call. One sidecar serves every Claude Code session on the machine, so
without it the metrics store and the TUI show a single undifferentiated stream.
"""
import sys, json, re

data = json.load(sys.stdin)
cmd = data.get("tool_input", {}).get("command", "")
# Claude Code supplies this on every hook invocation; default to omitting the
# field rather than sending a blank one the sidecar would have to filter.
#
# Validated, not merely interpolated: this value is spliced into a JSON literal
# inside a single-quoted shell string, so a quote or brace in it would produce a
# broken (or attacker-chosen) request. Session ids are UUIDs, so anything outside
# that alphabet is dropped rather than escaped.
session_id = str(data.get("session_id") or "")
if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", session_id):
    session_id = ""

# Already routed through sidecar — let it through
if "localhost:8765" in cmd:
    sys.exit(0)

blocked = (
    bool(re.match(r"^\s*sbt(\s|$)", cmd)) or
    bool(re.search(r"\bsbt\s+", cmd)) or
    "cargo test" in cmd or
    "cargo build" in cmd or
    "cargo check" in cmd or
    "pytest" in cmd or
    re.search(r"\bgo\s+test\b", cmd) is not None or
    "mvn test" in cmd or
    "mvn verify" in cmd or
    "gradle test" in cmd or
    "gradle build" in cmd
)

if blocked:
    tool = cmd.split()[0]
    session_field = f',"session_id":"{session_id}"' if session_id else ""
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": (
                f"Direct `{tool}` call blocked — the Bash tool timeout will kill it mid-run.\n"
                "Use the sidecar /jobs create-then-poll pattern (see ~/.claude/CLAUDE.md):\n\n"
                "  JOB=$(curl -s -X POST http://localhost:8765/jobs \\\n"
                "    -H 'Content-Type: application/json' \\\n"
                f"    -d '{{\"cmd\":\"{tool}\",\"args\":[...],\"cwd\":\"$PWD\"{session_field}}}' \\\n"
                "    | python3 -c \"import sys,json; print(json.load(sys.stdin)['job_id'])\")\n"
                "  FROM=0\n"
                "  while :; do\n"
                "    POLL=\"$TMPDIR/sidecar-poll-$$.json\"\n"
                "    curl -s \"http://localhost:8765/jobs/$JOB/lines?from=$FROM&wait_ms=25000\" > \"$POLL\"\n"
                "    python3 - \"$POLL\" << 'PYEOF'\n"
                "import json, sys\n"
                "d = json.load(open(sys.argv[1]))\n"
                "if d.get('dropped'): print(f\"[{d['dropped']} earlier lines dropped]\")\n"
                "for l in d['lines']: print(l['text'])\n"
                "open('/tmp/sc-from', 'w').write(str(d['next_from']))\n"
                "sys.exit(0 if d['running'] else 1)\n"
                "PYEOF\n"
                "    RET=$?; FROM=$(cat /tmp/sc-from 2>/dev/null || echo 0)\n"
                "    rm -f \"$POLL\" /tmp/sc-from\n"
                "    [ $RET -ne 0 ] && break\n"
                "  done\n"
                "  curl -s \"http://localhost:8765/jobs/$JOB/status\"\n\n"
                "`wait_ms` makes the server hold the request until there is output or the "
                "job ends — do NOT add a `sleep` to this loop."
            )
        }
    }))
