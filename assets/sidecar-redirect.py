#!/usr/bin/env python3
"""
PreToolUse hook: intercepts direct sbt/cargo/pytest/go test Bash calls
and redirects Claude to use the sidecar /jobs create-then-poll pattern.
"""
import sys, json, re

data = json.load(sys.stdin)
cmd = data.get("tool_input", {}).get("command", "")

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
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": (
                f"Direct `{tool}` call blocked — the Bash tool timeout will kill it mid-run.\n"
                "Use the sidecar /jobs create-then-poll pattern (see ~/.claude/CLAUDE.md):\n\n"
                "  JOB=$(curl -s -X POST http://localhost:8765/jobs \\\n"
                "    -H 'Content-Type: application/json' \\\n"
                f"    -d '{{\"cmd\":\"{tool}\",\"args\":[...],\"cwd\":\"$PWD\"}}' \\\n"
                "    | python3 -c \"import sys,json; print(json.load(sys.stdin)['job_id'])\")\n"
                "  FROM=0\n"
                "  for i in $(seq 1 40); do\n"
                "    sleep 15\n"
                "    POLL=\"$TMPDIR/sidecar-poll-$$.json\"\n"
                "    curl -s \"http://localhost:8765/jobs/$JOB/lines?from=$FROM\" > \"$POLL\"\n"
                "    python3 - \"$POLL\" << 'PYEOF'\n"
                "import json, sys\n"
                "d = json.load(open(sys.argv[1]))\n"
                "for l in d['lines']: print(l['text'])\n"
                "open('/tmp/sc-from', 'w').write(str(d['next_from']))\n"
                "sys.exit(0 if d['running'] else 1)\n"
                "PYEOF\n"
                "    RET=$?; FROM=$(cat /tmp/sc-from 2>/dev/null || echo 0)\n"
                "    rm -f \"$POLL\" /tmp/sc-from\n"
                "    [ $RET -ne 0 ] && break\n"
                "  done\n"
                "  curl -s \"http://localhost:8765/jobs/$JOB/status\""
            )
        }
    }))


