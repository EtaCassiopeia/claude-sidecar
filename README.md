# claude-sidecar

A small HTTP server that runs a fixed allowlist of commands **outside** a
sandbox, so a sandboxed coding agent (such as Claude Code) can reach them over
localhost. It exists to run long jobs — builds, test suites — that would
otherwise be killed when an agent's per-command timeout fires mid-run.

```
Coding agent (sandboxed)
       │
       │  HTTP to localhost (typically allowed even in a sandbox)
       ▼
claude-sidecar  ←── runs in your terminal, outside the sandbox
       │
       ├─ POST /exec          → subprocess with pipes  → short commands (buffered)
       ├─ POST /jobs          → subprocess with a PTY  → long commands (create-then-poll)
       └─ POST /browser/fetch → AppleScript → Chrome   → pages the user's browser can see (macOS)
```

Multiple agent sessions can hit one sidecar concurrently. It's built on
[axum](https://github.com/tokio-rs/axum) + Tokio and is designed to stream job
progress back smoothly under load.

## How it works

- **`POST /exec`** runs a short command and returns its full output once it
  finishes (default 60s timeout). Good for `git status`, `gh pr view`, etc.
- **`POST /jobs`** spawns a long command under a pseudo-terminal, returns a
  `job_id` immediately, and streams output in the background. The client then
  **polls** `/jobs/{id}/lines?from=N` (or **watches** `/jobs/{id}/stream`) until
  the job finishes. This is the pattern for `sbt`, `cargo test`, `pytest`,
  `mvn`, `gradle`, `go test`, etc. — anything that can outlive a tool timeout.

Output is captured line-by-line (ANSI stripped), buffered in memory, and
optionally spilled to disk — see [Output buffering](#output-buffering).

## Build & install

Requires a recent stable Rust toolchain.

```bash
cargo build --release
# binary: target/release/claude-sidecar
```

Two helper scripts are included:

```bash
./rebuild.sh        # build, install to ~/.local/bin/claude-sidecar, restart in background
./rebuild.sh -v     # build, install, run in the foreground with verbose logging
./install.sh        # full first-time machine setup (PATH, auto-start, Claude hook, CLAUDE.md)
```

Run it directly:

```bash
claude-sidecar                 # listen on 127.0.0.1:8765
claude-sidecar -v --port 9000  # verbose, custom port
```

## Configuration

Every flag has an environment-variable equivalent.

| Flag | Env | Default | Description |
|------|-----|---------|-------------|
| `-p`, `--port <PORT>` | `SIDECAR_PORT` | `8765` | Port to listen on (binds `127.0.0.1`). |
| `-v`, `--verbose` | `SIDECAR_VERBOSE` | off | Log every output line to stderr. |
| `--max-jobs <N>` | `SIDECAR_MAX_JOBS` | `100` | Concurrent jobs before `/jobs` returns `503`. |
| `--max-lines <N>` | `SIDECAR_MAX_LINES` | `50000` | Output lines retained **in memory** per job. |
| `--spill` | `SIDECAR_SPILL` | off | Spill lines beyond `--max-lines` to a temp file instead of dropping them. |
| `--job-ttl <SECS>` | `SIDECAR_JOB_TTL` | `600` | Seconds a finished job is kept before eviction. |

## Allowlist

Only these commands may be executed (matched by base name):

```
gh  git  go  sbt  cargo  mvn  gradle  npm  python3  pytest  curl
```

The list lives in [`src/config.rs`](src/config.rs) (`ALLOWED_COMMANDS`) — edit it
to fit the tools you need. Per-request environment variables can be passed in the
`env` field of `/exec` and `/jobs` requests.

## API

All request bodies and responses are JSON. Errors are returned as
`{"error": "..."}` with an appropriate status code (`403` not allowed, `404`
not found, `503` too many jobs, `504` timeout, …).

### `GET /health`

```bash
curl -s http://localhost:8765/health
# {"status":"ok","version":"3","jobs":0,"job_ids":[]}
```

### `POST /exec` — short commands

Request: `cmd` (required), `args`, `cwd`, `timeout_secs` (default 60), `env`
(list of `[key, value]` pairs).

```bash
curl -s -X POST http://localhost:8765/exec \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"git","args":["status"],"cwd":"/path/to/repo"}'
# {"stdout":"...","stderr":"...","exit_code":0}
```

### `POST /jobs` — start a long command

Request: `cmd` (required), `args`, `cwd`, `timeout_secs` (default 3600),
`cols`/`rows` (PTY size, default 220×50), `env`.

```bash
curl -s -X POST http://localhost:8765/jobs \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"sbt","args":["validate"],"cwd":"'"$PWD"'","timeout_secs":3600}'
# {"job_id":"977bd5eb-48ae-4653-9f96-c32b97768812"}
```

### `GET /jobs/{id}/lines?from=N` — poll output

Returns lines `[N, N+500)` plus the cursor to poll from next. Call repeatedly,
advancing `from` to `next_from`, until `running` is `false`.

```jsonc
{
  "lines": [ { "index": 0, "text": "[info] compiling", "ts": 1700000000000 } ],
  "next_from": 1,     // pass this as ?from= on the next poll
  "dropped": 0,       // lines lost to the in-memory cap (0 unless overflowing without --spill)
  "running": true,
  "exit_code": null   // set once finished
}
```

### `GET /jobs/{id}/status` — snapshot

```jsonc
{
  "job_id": "…", "cmd": "cargo", "args": ["test"],
  "running": false, "exit_code": 0,
  "line_count": 1234,   // total lines produced (including any evicted)
  "elapsed_ms": 5678    // frozen at completion
}
```

### `GET /jobs/{id}/stream` — live SSE

Server-Sent Events, for a human watching in a terminal. Replays the job's
history, then streams live lines, then a terminal `exit` event, then closes.
Includes keep-alive comments so idle connections aren't dropped.

```
data: {"index":0,"text":"[info] compiling","ts":1700000000000}

data: {"type":"exit","outcome":{"outcome":"completed","exit_code":0},"exit_code":0,"ts":1700000000000}
```

### `POST /browser/fetch` — read a page through Chrome (macOS)

Opens the URL in a new tab of the user's real Chrome — real profile, real
cookies — waits for it to load, extracts the rendered page, and closes the tab.
Pages behind a login or paywall the user already has access to come back
readable, which plain `curl` can't do.

```bash
curl -s -X POST http://localhost:8765/browser/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://medium.com/some-paywalled-article"}'
# {"url":"…","title":"…","content":"rendered page text"}
```

Options: `wait_secs` (max page-load wait, default 20, cap 120), `format`
(`"text"` = `innerText`, default; `"html"` = full DOM), `keep_tab` (leave the
tab open, default false).

### `GET /browser/tab` — read the currently focused tab (macOS)

Returns the page the user is looking at right now — navigate somewhere
yourself, then have the agent read it. Takes `?format=text|html`.

### Browser bridge setup (one-time)

1. **Rebuild and restart the sidecar** so the `/browser/*` routes exist:
   `./rebuild.sh` (builds, installs to `~/.local/bin/claude-sidecar`, restarts).
   A sidecar started before this feature landed returns 404 for these routes.
2. **Enable JavaScript from Apple Events in Chrome:** menu bar → **View →
   Developer → Allow JavaScript from Apple Events**. Without it every call
   fails with a 502 naming this exact menu path.
3. **Grant macOS Automation permission:** the first call pops a dialog asking
   to let your terminal control Google Chrome — approve it. If it was ever
   denied, re-enable it under **System Settings → Privacy & Security →
   Automation → \<your terminal\> → Google Chrome**.
4. Verify:

   ```bash
   curl -s -X POST http://localhost:8765/browser/fetch \
     -H 'Content-Type: application/json' \
     -d '{"url":"https://example.com"}' | head -c 200
   ```

Errors from a missing step come back with the fix in the message, so agents
can relay the remediation instead of guessing.

**Security model:** the endpoints run two fixed AppleScript templates via
`/usr/bin/osascript`; the URL is passed as an argv item, never spliced into
script text, and only `http(s)` URLs are accepted — callers get these two
scripts, not general osascript access (which is deliberately absent from the
allowlist). Fetched content goes to whatever local process called the
endpoint, so the usual localhost caveat in [Security](#security) applies.

## Create-then-poll pattern

The canonical client loop for a long job (also embedded into Claude via the
install hook):

```bash
# 1. Start the job — returns immediately
JOB=$(curl -s -X POST http://localhost:8765/jobs \
  -H 'Content-Type: application/json' \
  -d "{\"cmd\":\"sbt\",\"args\":[\"validate\"],\"cwd\":\"$PWD\",\"timeout_secs\":3600}" \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['job_id'])")

# 2. Poll every 15s — print new lines, stop when done
FROM=0
while :; do
  sleep 15
  RESP=$(curl -s "http://localhost:8765/jobs/$JOB/lines?from=$FROM")
  echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); [print(l['text']) for l in d['lines']]"
  FROM=$(echo "$RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['next_from'])")
  RUNNING=$(echo "$RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['running'])")
  [ "$RUNNING" = "False" ] && break
done

# 3. Final status / exit code
curl -s "http://localhost:8765/jobs/$JOB/status"
```

## Output buffering

Each job keeps the most recent `--max-lines` (default 50,000) lines in memory.
A line's `index` is a stable logical position, so `?from=N` polling stays
correct no matter what happens to the buffer.

- **Default (memory-only).** Once the cap is exceeded, the oldest lines are
  dropped. Polling reports how many via `dropped`, and `next_from` jumps past
  the gap, so loss is visible rather than silent. Bounds memory per job.
- **`--spill`.** Overflow lines are appended to a per-job temp file
  (`$TMPDIR/claude-sidecar-<pid>/<job-id>.jsonl`) instead of being dropped, so
  the **full** log stays retrievable via `/lines` and `/stream`. `dropped`
  stays `0`. The file is removed when the job is evicted. On any disk error it
  falls back to memory-only mode.

For typical builds and test runs the 50k in-memory window loses nothing; reach
for `--spill` when you need the complete log of an exceptionally chatty job.

## Notes

- Binds `127.0.0.1` only — never exposed off-host.
- Timed-out jobs are killed as a process group (`setsid` + `killpg`), so nothing
  is left running.
- Graceful shutdown on Ctrl-C drains in-flight requests.
- Falls back from PTY to plain pipes automatically when `openpty` is unavailable
  (e.g. inside a restricted container).

## Security

The server binds `127.0.0.1` and applies **no authentication** — any process on
the host can reach it. It executes an allowlist of real commands, so only run it
on a machine you control, and keep the allowlist limited to what you need. It is
not intended to be exposed to a network.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.

