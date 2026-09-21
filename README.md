# claude-sidecar

A small HTTP server that runs commands **outside** a sandbox, so a sandboxed
coding agent (such as Claude Code) can reach them over localhost. It exists to
run long jobs — builds, test suites — that would otherwise be killed when an
agent's per-command timeout fires mid-run, and has grown a few more bridges out
of the sandbox since.

```
Coding agent (sandboxed)
       │
       │  HTTP to localhost (typically allowed even in a sandbox)
       ▼
claude-sidecar  ←── runs outside the sandbox, as you
       │
       ├─ POST /exec            → subprocess with pipes  → short commands (buffered)
       ├─ POST /batch           → subprocesses in order  → several commands, one call
       ├─ POST /jobs            → subprocess with a PTY  → long commands (create-then-poll)
       ├─ POST /browser/fetch   → AppleScript → Chrome   → pages your browser can see (macOS)
       ├─ POST /gdocs/read      → Drive export           → Google Docs/Sheets/Slides
       ├─ POST /gdocs/clipboard → Markdown → Docs HTML   → paste rich text into Docs
       ├─ GET  /events          → SSE                    → live server-wide activity
       └─ GET  /stats           → Parquet + DataFusion   → durable call metrics
```

Multiple agent sessions can hit one sidecar concurrently. It's built on
[axum](https://github.com/tokio-rs/axum) + Tokio and is designed to stream job
progress back smoothly under load.

## How it works

- **`POST /exec`** runs a short command and returns its full output once it
  finishes (default 60s timeout). Good for `git status`, `gh pr view`, etc.
- **`POST /batch`** runs an ordered list of commands in one call, so
  `git init` → `git add` → `git commit` is one request, not three.
- **`POST /jobs`** spawns a long command under a pseudo-terminal, returns a
  `job_id` immediately, and streams output in the background. The client then
  **polls** `/jobs/{id}/lines?from=N` (or **watches** `/jobs/{id}/stream`) until
  the job finishes. This is the pattern for `sbt`, `cargo test`, `pytest`,
  `mvn`, `gradle`, `go test`, etc. — anything that can outlive a tool timeout.
- **`POST /browser/fetch`** and **`POST /gdocs/read`** read content the sandbox
  cannot: pages behind your browser session, and Google Docs via Drive's own
  export.
- **`GET /events`** fans out every call the server handles as SSE, which is what
  `sidecar-tui` renders. **`GET /stats`** reads the durable metrics archive.

Output is captured line-by-line (ANSI stripped), buffered in memory, and
optionally spilled to disk — see [Output buffering](#output-buffering).

## Build & install

Requires a recent stable Rust toolchain.

```bash
cargo build --release
# binary: target/release/claude-sidecar
```

Helper scripts are included:

```bash
./rebuild.sh        # build, install to ~/.local/bin/claude-sidecar, restart in background
./rebuild.sh -v     # build, install, run in the foreground with verbose logging
./install.sh        # full first-time machine setup (PATH, auto-start, Claude hook, CLAUDE.md)
./tui.sh            # build and run the terminal monitor (see below)
```

Run it directly:

```bash
claude-sidecar                 # listen on 127.0.0.1:8765, attached to this terminal
claude-sidecar --detach        # fork into a new session — use this for background starts
claude-sidecar -v --port 9000  # verbose, custom port
```

**`--detach` matters when backgrounding.** A daemon started with a plain `&`
keeps the launching terminal as its controlling tty, which makes it a process
group competing for that terminal — and `sidecar-tui` is what loses the fight,
taking `SIGTTIN` on its next stdin read and suspending. `--detach` forks into a
new session so the problem cannot arise.

### Terminal monitor

`sidecar-tui` (built from the optional `tui` feature) is a read-only view of a
running sidecar: live activity from `/events`, job output, and a metrics overlay
fed by `/stats`.

```bash
cargo build --release --features tui   # binary: target/release/sidecar-tui
./tui.sh                               # build + run, killing any previous instance
```

## Claude Code integration

The sidecar is useless to an agent that doesn't know it exists. `./install.sh`
wires it into Claude Code in three places, all idempotent — re-run it any time
to refresh:

| What | Where | Source of truth |
|---|---|---|
| Instructions telling Claude when to route through the sidecar | `~/.claude/CLAUDE.md` | `assets/claude-md-block.md` |
| `PreToolUse` hook that intercepts blocked/long Bash calls and points Claude here | `~/.claude/hooks/` + `~/.claude/settings.json` | `assets/sidecar-redirect.py` |
| Auto-start on shell login, plus `~/.local/bin` on `PATH` | your shell rc | — |

**Both assets are canonical — edit them here, not in `~/.claude/`.** The
installer replaces the existing CLAUDE.md block by matching its first heading
(`# Sidecar — Running Blocked or Long Commands`), so edits made directly in
`~/.claude/CLAUDE.md` are overwritten on the next `./install.sh`. Changing the
API without updating `assets/claude-md-block.md` leaves every future session
following stale instructions.

Manual setup, if you'd rather not run the installer:

```bash
cargo build --release && cp target/release/claude-sidecar ~/.local/bin/
cat assets/claude-md-block.md >> ~/.claude/CLAUDE.md
```

Then start it (`claude-sidecar &`) and confirm Claude can see it — ask it to
run `curl -s http://localhost:8765/health`. The hook is optional; without it
Claude follows the CLAUDE.md decision tree on its own, it just isn't nudged
when the sandbox blocks something.

Per-project instead of global? Put the block in a project's `./CLAUDE.md`
rather than `~/.claude/CLAUDE.md` — same content, narrower scope.

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
| `--kill-grace <SECS>` | `SIDECAR_KILL_GRACE` | `5` | Seconds a `SIGTERM`'d job gets before `SIGKILL`. `0` skips straight to `SIGKILL`. |
| `--config <PATH>` | `SIDECAR_CONFIG` | `~/.config/claude-sidecar/config.toml` | Command policy file. Missing means built-in defaults; malformed is a startup error. |
| `--metrics-dir <PATH>` | `SIDECAR_METRICS_DIR` | `~/.config/claude-sidecar/metrics` | Where the durable call metrics are written. |
| `--no-metrics` | `SIDECAR_NO_METRICS` | off | Don't record call metrics to disk. |
| `--detach` | `SIDECAR_DETACH` | off | Fork into a new session with no controlling tty. |

Two subcommands read the metrics store directly, so they work whether or not the
daemon is up:

```bash
claude-sidecar stats --days 7
claude-sidecar query "SELECT cmd, count(*) FROM calls GROUP BY cmd ORDER BY 2 DESC"
```

The table is `calls`; its columns are `date`, `ts`, `kind`, `cmd`, `sub`,
`nargs`, `ms`, `exit`, `error`, and `session`.

## Command policy

The policy is **deny-by-exception**: every command is permitted except those
named in the denylist, which defaults to `sudo`. It lives in
`~/.config/claude-sidecar/config.toml` — see
[`assets/config.example.toml`](assets/config.example.toml):

```toml
denied = ["sudo"]
```

Matching is on the command basename, so `sudo` also refuses `/usr/bin/sudo`. A
denied command returns `403`. `denied = []` denies nothing; omitting the key
keeps the built-in default; a syntax error aborts startup rather than silently
reverting to defaults. The startup banner prints the active policy.

**This is not a containment boundary.** The sidecar runs as you, on your
machine, so anything that can reach it can already run code as you — and most
denials are trivially reachable through a wrapper anyway (`env openssl`,
`xargs openssl`, a shell script, a build tool's own hooks). The denylist makes a
deliberate refusal loud; it does not contain a hostile caller. Only run the
sidecar on a machine you control.

Per-request environment variables can be passed in the `env` field of `/exec`,
`/batch`, and `/jobs` requests.

### Always send `session_id`

One sidecar serves every agent session on the machine, so a call with no
`session_id` is indistinguishable from every other session's traffic. `/exec`,
`/batch`, and `/jobs` all take the field; it is an opaque tag the sidecar never
interprets, recorded in the metrics store's `session` column so
`GROUP BY session` can separate callers. Omit it entirely rather than sending a
placeholder. `/browser/*` and `/gdocs/*` take no session id.

### Shell commands

The POSIX shells (`bash`, `sh`, `zsh`) accept an inline command string, so you
can run compound commands directly:

```bash
curl -s -X POST http://localhost:8765/exec \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"bash","args":["-c","git rev-parse HEAD && git status --short"]}'
```

A `bash -c` string is interpreted by the shell and can invoke any binary on the
machine, including a denied one — which is the concrete reason the denylist is
documented above as a loud refusal rather than a boundary.

## API

All request bodies and responses are JSON. Errors are returned as
`{"error": "..."}` with an appropriate status code (`403` denied, `404`
not found, `503` too many jobs, `504` timeout, …).

### `GET /health`

```bash
curl -s http://localhost:8765/health
# {"status":"ok","version":"3","jobs":0,"job_ids":[],"diagnostics":0}
```

### `GET /logs` — the daemon's own warnings and errors

These otherwise only reach stderr, which is unreachable for an autostarted
daemon whose launching terminal is long gone. `/health` reports the count, so a
client can tell there is something worth fetching without polling this endpoint.

### `POST /exec` — short commands

Request: `cmd` (required), `args`, `cwd`, `timeout_secs` (default 60), `env`
(list of `[key, value]` pairs), `session_id`.

```bash
curl -s -X POST http://localhost:8765/exec \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"git","args":["status"],"cwd":"/path/to/repo","session_id":"08278e3c-…"}'
# {"stdout":"...","stderr":"...","exit_code":0}
```

### `POST /batch` — run several commands in sequence

Runs an ordered list of commands one after another, so a chain like
`git init && git add -A && git commit` is a single request instead of several.
Each step is a `{cmd, args, cwd?, timeout_secs?, env?}` object; the top-level
`cwd`, `timeout_secs` (per step, default 60), `env`, and `session_id` supply
defaults a step can override.

Every step is validated against the policy **before any step runs**, so a
denied command anywhere in the list rejects the whole batch (`403`) without
executing side effects. By default the batch stops at the first step that exits
nonzero; set `continue_on_error: true` to run every step regardless. (A step that
times out or fails to spawn aborts the batch with the matching error status even
under `continue_on_error`.)

```bash
curl -s -X POST http://localhost:8765/batch \
  -H 'Content-Type: application/json' \
  -d '{"cwd":"/path/to/repo","steps":[
        {"cmd":"git","args":["init","-b","main"]},
        {"cmd":"git","args":["add","-A"]},
        {"cmd":"git","args":["commit","-m","Initial commit"]}
      ]}'
# {"steps":[{"cmd":"git","args":["init","-b","main"],"stdout":"...","stderr":"","exit_code":0}, ...],
#  "failed_at":null,"success":true}
```

The response has `steps` (results for the steps that ran, in order — shorter than
the request if it stopped early), `failed_at` (index of the first nonzero exit,
or `null`), and `success` (true when every requested step ran and exited zero).
Steps run in-process and buffer their output; for a single long-running build,
use `/jobs` instead. Max 100 steps per batch.

### `POST /jobs` — start a long command

Request: `cmd` (required), `args`, `cwd`, `timeout_secs` (default 3600),
`idle_timeout_secs`, `input`, `session_id`, `cols`/`rows` (PTY size, default
220×50), `env`.

```bash
curl -s -X POST http://localhost:8765/jobs \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"sbt","args":["validate"],"cwd":"'"$PWD"'","timeout_secs":3600}'
# {"job_id":"977bd5eb-48ae-4653-9f96-c32b97768812"}
```

**Commands that stop on a prompt.** Jobs run under a PTY, so a tool that asks
before acting (`Do you want … set as default? (Y/n):`) will ask — and a PTY has
no EOF to give it, so the job sits at the prompt until a deadline kills it.
Answer up front with `input`, which is typed at the process before its output is
read:

```bash
-d '{"cmd":"sdk","args":["install","java","25.0.4-amzn"],"input":"Y\n"}'
```

Send every answer the run needs (`"Y\nY\n"`); there is no way to reply to a
prompt you did not anticipate. For anything unanticipated, set
`idle_timeout_secs` so a wedged job fails in seconds instead of at the hour mark
— a job stuck on a prompt produces no output, which is exactly what that check
detects. `/exec` and `/batch` need none of this: their stdin is closed, so a
prompting command gets EOF and takes its default.

### `GET /jobs` — list live jobs

Returns a snapshot of every job the registry currently holds, which is what the
monitor lists.

### `GET /jobs/{id}/lines?from=N&wait_ms=M` — poll output

Returns lines `[N, N+500)` plus the cursor to poll from next. Call repeatedly,
advancing `from` to `next_from`, until `running` is `false`.

`wait_ms` (capped at 30000) holds the request open until there is output or the
job ends, instead of returning an empty window immediately — so a polling loop
needs no `sleep` of its own. Omit it, or pass `0`, for non-blocking behavior.

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

### `DELETE /jobs/{id}` — cancel a running job

Terminates the job's process group with the same `SIGTERM` → `--kill-grace` →
`SIGKILL` sequence a timeout uses, so nothing is left running.

### `GET /events` — live server-wide activity (SSE)

Every call the server handles — exec, batch, job, browser, gdocs — announced as
it starts and again as it finishes, plus the daemon's own diagnostics. This is
the feed `sidecar-tui` renders; unlike `/jobs/{id}/stream` it spans the whole
server rather than one job.

### `GET /stats` and `POST /stats/query` — call metrics

Calls are recorded to a Parquet archive under
`~/.config/claude-sidecar/metrics`, queried in-process by DataFusion so nothing
external is needed to read them back. `GET /stats` returns daily counts and
per-command aggregates (what the monitor's overlay draws); `POST /stats/query`
takes `{"sql":"…"}` against the `calls` table and is what `claude-sidecar query`
posts. A query's result text is capped so a `SELECT *` over a year of history
cannot buffer unboundedly.

Both return an error when metrics are disabled (`--no-metrics`, or no writable
metrics directory) — the sidecar itself runs normally in that case.

### `POST /gdocs/read` — read a Google Doc, Sheet, or Slides deck

Reads through Drive's own export rather than the DOM, which is the only thing
that works: Docs renders to a canvas, so `/browser/fetch` returns UI chrome and
a fraction of the text. Name the document exactly one way — `doc_id`, `url`, or
`title` (resolved against the local Drive mount) — since guessing between them
risks reading the wrong document.

```bash
curl -s -X POST http://localhost:8765/gdocs/read \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://docs.google.com/document/d/1HDS…/edit"}'
# {"doc_id":"1HDS…","kind":"document","format":"md","url":"…","content":"# …","bytes":44212,"title":"…"}
```

`format` defaults per type: `md` for Docs, `csv` for Sheets, `txt` for Slides.
With `doc_id` you must also pass `kind` for anything that isn't a Doc; `url` and
`title` determine it themselves. An ambiguous title is an error listing the
candidates rather than a coin flip.

### `GET /gdocs/list` — index the Drive mount

Lists documents found in the local Google Drive mount, which is how `title`
lookups resolve.

### `POST /gdocs/clipboard` — Markdown into Google Docs

Converts Markdown to the inline-styled HTML Docs accepts on paste and puts it on
the clipboard, so rich text — headings, tables, syntax-highlighted code, and
rendered Mermaid diagrams — can be pasted into a document. Docs ignores `<style>`
blocks, hence inline attributes; it also cannot render SVG, so diagrams are
rasterized to base64 `<img>` data URIs first and travel fully self-contained.

Mermaid rendering is off by default (`"diagrams":"chrome"` turns it on) and
drives headless Chrome over the vendored `assets/mermaid.min.js`, so every
diagram type works and nothing depends on a CDN. Every failure path falls back
to a visible code block plus a note carrying Mermaid's own parse error — a
diagram can be ugly or absent-with-explanation, never silently missing.

### `POST /browser/fetch` — read a page through Chrome (macOS)

Opens the URL in a new tab of the user's real Chrome — real profile, real
cookies — waits for it to load, extracts the rendered page, and closes the tab.
Pages behind a login or paywall the user already has access to come back
readable, which plain `curl` can't do.

```bash
curl -s -X POST http://localhost:8765/browser/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://medium.com/some-paywalled-article"}'
# {"url":"…","title":"…","content":"# Heading\n\nArticle text…","truncated":false}
```

Options:

| Option | Default | Meaning |
|---|---|---|
| `format` | `"markdown"` | `"markdown"` = main content as markdown; `"text"` = whole-page `innerText`; `"html"` = full DOM; `"dom"` = full DOM converted to markdown server-side |
| `max_chars` | none | Cap the content; sets `truncated: true` when it bites |
| `include_links` | `false` | Keep link/image targets in markdown (link text is always kept) |
| `wait_secs` | `20` | Max page-load wait, cap 120 |
| `keep_tab` | `false` | Leave the tab open (useful to see what actually rendered) |

**On `markdown`.** The extractor drops site chrome — nav, cookie banners,
related-article rails, comments — then serializes what's left, keeping
headings, lists, tables, and fenced code with language tags. Publication date
and author are re-attached as a one-line italic byline when they can be found,
since dropping the masthead otherwise takes them with it, and "when was this
published?" is a question worth ~40 characters. They come from `<head>`
metadata (OpenGraph, Schema.org JSON-LD, citation tags), falling back to a
byline-shaped element in the DOM. Measured against
`innerText`: −10% on a Cloudflare blog post, −16% on MDN and the Rust book,
−77% on a chrome-heavy landing page, and roughly break-even on a long Wikipedia
article, where markdown's table scaffolding offsets what the strip pass removes.
Structure is the bigger win; the size drop is a bonus, not a step change —
`innerText` is already a decent extractor.

Link targets are excluded by default because they cost more than they return:
turning `include_links` on grows the MDN page from 13k to 21k characters, and a
Wikipedia article by 40%. Turn it on when the agent needs to follow links.

If the extractor picks the wrong block on some page, fall back to
`"format":"text"`, or to `"dom"` when you need everything the extractor scored
away.

**On `dom`.** The same full-DOM capture as `"html"`, converted to Markdown
server-side with `script`, `style`, `head`, `noscript`, and `iframe` skipped so
inline CSS doesn't surface as a paragraph. It keeps what `markdown` drops, at
the cost of the site chrome coming back, and it is the format that works on
canvas-rendered docs where `"text"` returns only the UI. For Google
Docs/Sheets/Slides specifically use [`/gdocs/read`](#post-gdocsread--read-a-google-doc-sheet-or-slides-deck)
instead — it returns the whole document rather than the fraction `"dom"`
recovers. Unlike `markdown`, `dom` gets no YouTube handling: it reads a watch
page as a page.

**On YouTube.** A watch page is the one case where the article extractor cannot
win: what a reader wants from a video is what is *said* in it, and that is not
in the page's prose. So `markdown` recognises watch URLs (`/watch?v=…` and
`/shorts/…`) and returns the transcript instead of the DOM, with the metadata
and the sidebar links around it:

```bash
curl -s -X POST http://localhost:8765/browser/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://www.youtube.com/watch?v=aircAruvnKk"}'
```
```markdown
# But what is a neural network? | Deep learning chapter 1

- **Channel:** 3Blue1Brown
- **Video:** https://www.youtube.com/watch?v=aircAruvnKk
- **Published:** 2017-10-05
- **Duration:** 18:40
- **Views:** 23,934,603
- **Captions:** 31 languages

## Description
…
## Related videos
- [Gradient descent, how neural networks learn | Deep Learning Chapter 2](https://www.youtube.com/watch?v=IHZwWFHWa-w) — 3Blue1Brown · 20:33
…
## Transcript

[0:04] This is a 3. It's sloppily written and rendered at an extremely low resolution…
```

The transcript comes from the panel YouTube renders behind "Show transcript",
which `/browser/fetch` opens in its throwaway tab before extracting. That is a
deliberate detour: the `timedtext` caption URL the page hands out answers `200`
with an empty body unless the request carries a token only the player can mint,
and the `get_transcript` InnerTube endpoint rejects an unsigned request with
`FAILED_PRECONDITION`. The rendered panel is the one source that stays readable,
and it is the same text a viewer sees.

Three consequences worth knowing:

- **Sections are ordered shortest-first** — metadata, description, related
  videos, then the transcript. With `max_chars` set, truncation eats the
  transcript tail instead of swallowing the links.
- **Captions are grouped into ~600-character paragraphs**, each stamped with the
  time of its first line (`[12:30]`), so a model can still cite a moment without
  paying for one newline per spoken phrase.
- **`/browser/tab` does not open the panel.** It reads the tab you are looking
  at, and clicking things there would be reaching into your session; you get the
  metadata and related videos, plus the transcript only if you already opened it.

A video with no captions still returns metadata and related videos, with the
transcript section saying why it is empty rather than coming back silently
blank. `"format":"text"` reads a watch page as an ordinary page.

### `GET /browser/tab` — read the currently focused tab (macOS)

Returns the page the user is looking at right now — navigate somewhere
yourself, then have the agent read it. Takes the same `format`, `max_chars`,
and `include_links` options as query parameters.

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

# 2. Poll — `wait_ms` holds the request until there is output or the job ends,
#    so this loop needs no `sleep` of its own.
FROM=0
while :; do
  RESP=$(curl -s "http://localhost:8765/jobs/$JOB/lines?from=$FROM&wait_ms=25000")
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
- Timed-out and cancelled jobs are killed as a process group (`setsid` +
  `killpg`), so nothing is left running.
- Graceful shutdown on Ctrl-C drains in-flight requests.
- Falls back from PTY to plain pipes automatically when `openpty` is unavailable
  (e.g. inside a restricted container).
- Spawned commands get a freshly sourced login-shell environment overlaid on the
  daemon's own, so a long-lived sidecar doesn't hand builds a `PATH` or an auth
  token frozen at the moment it started.
- `/browser/*` and `/gdocs/*` are macOS-only: they drive Chrome and the Drive
  mount through `osascript`.

## Security

The server binds `127.0.0.1` and applies **no authentication** — any process on
the host can reach it, and it runs real commands as you. The
[command policy](#command-policy) is deny-by-exception and is a way to make a
refusal loud, not a containment boundary: a shell, an interpreter, or a build
tool's own hooks all reach past it. Only run it on a machine you control, and do
not expose it to a network.

The browser bridge runs two fixed AppleScript templates through
`/usr/bin/osascript`, with the URL passed as an argv item and never spliced into
script text, and accepts only `http(s)` URLs — callers get those two scripts,
not general osascript access. Fetched page content, exported documents, and the
metrics archive all go to whatever local process asked for them, so the
localhost caveat above covers them too.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.

