# Sidecar — Running Blocked or Long Commands

When the sandbox blocks a Bash command, **do not ask the user to run it
manually** — route it through the local `claude-sidecar` instead.

## Decision tree

1. **Is it a long-running command** (`sbt`, `cargo`, `pytest`, `go test`, `mvn`,
   `gradle`)? → POST `http://localhost:8765/jobs` (job API — returns a job ID,
   then poll for lines).
2. **Is it any other allowlisted command** (`gh`, `git`, `go`, `npm`, `python3`,
   `curl`, `grep`, `find`, `jq`, `security`, …)? → POST `http://localhost:8765/exec` — buffered JSON response, 60s
   timeout.
3. **Is the sidecar not running?** → start it with `claude-sidecar &`.

Allowed: gh, git, go, sbt, cargo, mvn, gradle, npm, node, python3, pytest, curl, docker, docker-compose, grep, rg, find, ls, cat, head, tail, wc, diff, sed, awk, sort, uniq, cut, tr, xargs, cp, mv, rm, mkdir, touch, chmod, jq, yq, which, env, printenv, echo, printf, date, uname, security

## POST /exec — short commands (< 60s)

```bash
curl -s -X POST http://localhost:8765/exec \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"git","args":["status"],"cwd":"/path/to/repo"}'
# → {"stdout":"...","stderr":"...","exit_code":0}
```

## POST /jobs — long commands (create-then-poll)

```bash
# 1. Start job — returns immediately with a job ID
JOB=$(curl -s -X POST http://localhost:8765/jobs \
  -H 'Content-Type: application/json' \
  -d "{\"cmd\":\"sbt\",\"args\":[\"validate\"],\"cwd\":\"$PWD\",\"timeout_secs\":3600}" \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['job_id'])")

# 2. Poll every 15s — print new lines, stop when done
FROM=0
for i in $(seq 1 40); do
  sleep 15
  POLL="$TMPDIR/sidecar-poll-$$.json"
  curl -s "http://localhost:8765/jobs/$JOB/lines?from=$FROM" > "$POLL"
  python3 - "$POLL" << 'PYEOF'
import json, sys
d = json.load(open(sys.argv[1]))
for l in d['lines']: print(l['text'])
open('/tmp/sc-from', 'w').write(str(d['next_from']))
sys.exit(0 if d['running'] else 1)
PYEOF
  RET=$?; FROM=$(cat /tmp/sc-from 2>/dev/null || echo 0)
  rm -f "$POLL" /tmp/sc-from
  [ $RET -ne 0 ] && break
done

# 3. Final status
curl -s "http://localhost:8765/jobs/$JOB/status"
```

## Health check

```bash
curl -s http://localhost:8765/health 2>/dev/null | grep -q ok && echo "up" || echo "down"
```

