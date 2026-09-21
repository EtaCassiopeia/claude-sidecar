<!-- BEGIN claude-sidecar (managed by install.sh — edits will be overwritten) -->
# Sidecar — Running Blocked or Long Commands

When the sandbox blocks a Bash command, **do not ask the user to run it
manually** — route it through the local `claude-sidecar` instead.

## Decision tree

1. **Is it a long-running command** (`sbt`, `cargo`, `pytest`, `go test`, `mvn`,
   `gradle`)? → POST `http://localhost:8765/jobs` (job API — returns a job ID,
   then poll for lines).
2. **Is it any other command** (`gh`, `git`, `go`, `npm`, `python3`,
   `curl`, `grep`, `find`, `jq`, `security`, `jira`, …)? → POST `http://localhost:8765/exec` — buffered JSON response, 60s
   timeout.
3. **Is it several commands run in order** (e.g. `git init` → `git
   add` → `git commit`)? → POST `http://localhost:8765/batch` — one call, each
   step validated exactly as `/exec`, stop-on-nonzero by default.
4. **Did fetching a web page fail** (WebFetch blocked, empty body, login wall,
   paywall, or a JS-heavy page that returns no readable content), **or is it a
   YouTube video** whose spoken content you need? → POST
   `http://localhost:8765/browser/fetch` — reads the page through the user's
   real Chrome session, so anything they can see in the browser is readable,
   and returns a watch URL's **transcript** rather than its DOM.
5. **Is it a Google Doc, Sheet, or Slides deck?** → POST
   `http://localhost:8765/gdocs/read` — **never** `/browser/fetch` for these.
   Docs renders to a canvas, so scraping returns UI chrome and a fraction of the
   text; `/gdocs/read` uses Google's own export and returns the whole document.
6. **Is the sidecar not running?** → start it with `claude-sidecar --detach`.
   `--detach` matters: without it the daemon keeps the launching terminal and
   competes with `sidecar-tui` for it, which suspends the TUI on its next read.

Any command is permitted except a short configured denylist (default: `sudo`).
A denied command returns 403. The denylist lives in
`~/.config/claude-sidecar/config.toml` — the sidecar's startup banner prints the
active policy.

## Always send `session_id`

One sidecar serves **every** Claude Code session on the machine, so a call with
no `session_id` is indistinguishable from every other session's traffic — the
monitor groups it under "no session" and `GROUP BY session` cannot separate it.

Pass your own session id on `/exec`, `/batch`, and `/jobs`. It is the
`session_id` from the current session (the same value as the transcript
filename); omit the field entirely if you do not know it rather than sending a
placeholder or a blank string.

```bash
curl -s -X POST http://localhost:8765/exec \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"git","args":["status"],"cwd":"/repo","session_id":"08278e3c-eea1-…"}'
```

The sidecar never interprets the value — it is an opaque tag used only for
grouping. It is recorded in the metrics store's `session` column, so
`claude-sidecar query "SELECT session, count(*) FROM calls GROUP BY session"`
attributes work per session. `/browser/*` and `/gdocs/*` take no session id.

## POST /exec — short commands (< 60s)

```bash
curl -s -X POST http://localhost:8765/exec \
  -H 'Content-Type: application/json' \
  -d '{"cmd":"git","args":["status"],"cwd":"/path/to/repo","session_id":"<this session>"}'
# → {"stdout":"...","stderr":"...","exit_code":0}
```

## POST /batch — ordered commands in one call

Runs each step in sequence, validating every step against the policy exactly
as `/exec` does (no shell, no `&&`, no `$(...)` — args stay an explicit vector).
The whole batch is validated up front, so a single denied step 403s the
request before anything runs. Stops at the first non-zero exit unless
`continue_on_error` is set.

```bash
curl -s -X POST http://localhost:8765/batch \
  -H 'Content-Type: application/json' \
  -d '{"steps":[
        {"cmd":"git","args":["init","-b","main"]},
        {"cmd":"git","args":["add","-A"]},
        {"cmd":"git","args":["commit","-m","Initial commit"]}
      ],"cwd":"/path/to/repo","session_id":"<this session>"}'
# → {"steps":[{"cmd":"git","args":[...],"stdout":"...","stderr":"...","exit_code":0}, ...],
#    "aborted":false}
```

Options: top-level `cwd` (default for every step), per-step `cwd` / `timeout_secs`
/ `env` overrides, `continue_on_error` (bool, default false), top-level
`session_id` (applied to every step — a batch is one caller's request). On
failure the `steps` array holds only the steps that ran and `aborted` is true.
There is no value substitution between steps — if step 2 needs a value from step
1, read it from the response and build step 2 in a second call.

## POST /jobs — long commands (create-then-poll)

```bash
# 1. Start job — returns immediately with a job ID
JOB=$(curl -s -X POST http://localhost:8765/jobs \
  -H 'Content-Type: application/json' \
  -d "{\"cmd\":\"sbt\",\"args\":[\"validate\"],\"cwd\":\"$PWD\",\"timeout_secs\":3600,\"session_id\":\"<this session>\"}" \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['job_id'])")

# 2. Poll with `wait_ms` — the server holds the request until there is output or
#    the job ends, so DO NOT add a `sleep` here: that would reintroduce the very
#    latency `wait_ms` exists to remove.
FROM=0
while :; do
  POLL="$TMPDIR/sidecar-poll-$$.json"
  curl -s "http://localhost:8765/jobs/$JOB/lines?from=$FROM&wait_ms=25000" > "$POLL"
  python3 - "$POLL" << 'PYEOF'
import json, sys
d = json.load(open(sys.argv[1]))
if d.get('dropped'): print(f"[{d['dropped']} earlier lines dropped]")
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

`wait_ms` is capped at 30000 server-side. Omit it (or pass 0) for the old
non-blocking behavior.

### Commands that stop on a prompt

Jobs run under a PTY, so tools that ask before acting (`sdk install java …` →
`Do you want … set as default? (Y/n):`) will ask, and a PTY has no EOF to give
them — the job sits at the prompt until a deadline kills it. Answer up front with
`input`, which is typed at the process before its output is read:

```bash
-d '{"cmd":"sdk","args":["install","java","25.0.4-amzn"],"input":"Y\n"}'
```

Send every answer the run needs (`"Y\nY\n"`); there is no way to reply to a
prompt you did not anticipate. For anything unanticipated, set
`idle_timeout_secs` so a wedged job fails in seconds instead of at the hour mark
— a job stuck on a prompt produces no output, which is exactly what that check
detects. Prefer a non-interactive flag when the tool has one (`-y`,
`--non-interactive`); `input` is for tools that don't.

`/exec` and `/batch` need none of this: their stdin is closed, so a prompting
command gets EOF and takes its default rather than hanging.

## POST /browser/fetch — read a web page through the user's Chrome

Use this as a fallback when normal web fetching fails or returns nothing useful
(login walls, paywalls, JS-rendered pages). It opens the URL in a new tab of the
user's Chrome, waits for it to load, extracts the rendered content, and closes
the tab.

```bash
# Main content as markdown, site chrome stripped
curl -s -X POST http://localhost:8765/browser/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com/article","max_chars":40000}'
# → {"url":"https://…","title":"…","content":"# Heading\n\n…","truncated":false}

# Read whatever tab the user currently has focused ("read this page")
curl -s 'http://localhost:8765/browser/tab?max_chars=40000'
```

Request fields: `url` (required, http/https only), `wait_secs` (page-load wait,
default 20, cap 120), `keep_tab` (leave the tab open for debugging), plus the
four below, which `/browser/tab` also takes as query params:

- `format` — `"markdown"` (**default**) extracts the page's main content only:
  site chrome is dropped and structure survives, and a YouTube watch URL yields
  the video's transcript. `"text"` is whole-page `innerText`, `"html"` the full
  DOM, and `"dom"` that same full DOM converted to Markdown server-side.
- `max_chars` — cap the content; sets `truncated:true` when it bites.
- `include_links` — default false. Link *text* is always kept; this only adds
  the URLs, which are a large share of the bytes. Turn it on to follow links.

If `"markdown"` picks the wrong block on some page, retry with `"text"`, or with
`"dom"` when you need everything the extractor scored away. For canvas-rendered
docs `"text"` returns only the UI chrome, and `"dom"` is the one that works. For
**Google Docs/Sheets/Slides specifically, use `/gdocs/read` instead** — it
returns the whole document rather than the fraction `"dom"` recovers.

**YouTube videos.** A watch URL (`/watch?v=…` or `/shorts/…`) returns the
video's **transcript** instead of the page — no extra options, the default
`markdown` format does it:

```bash
curl -s -X POST http://localhost:8765/browser/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://www.youtube.com/watch?v=…","max_chars":40000}'
```

You get `# title`, then a fact list (channel, published, duration, views,
captions), the description, `## Related videos` as markdown links, and
`## Transcript` as `[mm:ss]`-stamped paragraphs. Sections run shortest-first,
so `max_chars` clips the transcript tail and leaves the metadata and links
intact. This is the cheap way to answer "what does this video say?" — don't
ask the user to summarise it, and don't try `youtube.com` with WebFetch, which
sees no transcript at all.

The transcript comes from YouTube's own "Show transcript" panel, which
`/browser/fetch` opens in its throwaway tab. `/browser/tab` deliberately does
*not* click anything in the user's live tab, so there you get metadata and
related videos but a transcript only if the user already opened the panel — to
transcribe a video the user is watching, read the URL from `/browser/tab` and
re-request it via `/browser/fetch`. A video with no captions still returns
metadata, with the transcript section stating why it is empty — that message is
the real answer, not a failure to retry. `"format":"text"` reads a watch page as
an ordinary page.

macOS only. First use needs Chrome's *View → Developer → Allow JavaScript from
Apple Events* enabled and the terminal granted Automation access to Chrome — the
error message says which one is missing.

## POST /gdocs/read — read a Google Doc, Sheet, or Slides deck

The way to read Google-native documents. Use it whenever the user points at a
Doc — by URL, by title, or by saying "the design doc". **Do not use
`/browser/fetch` for these**: Docs renders to a canvas, so scraping the DOM
returns the UI chrome plus a fraction of the text. Measured on a real 44 KB
design doc, `/browser/fetch` returned ~6 KB polluted with a Workspace sharing
banner; `/gdocs/read` returned the whole document as Markdown.

```bash
# By URL — the type (Doc/Sheet/Slides) comes from the URL
curl -s -X POST http://localhost:8765/gdocs/read \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://docs.google.com/document/d/1HDS…/edit"}' \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['content'])"

# By title — resolved against the local Google Drive mount
curl -s -X POST http://localhost:8765/gdocs/read \
  -H 'Content-Type: application/json' \
  -d '{"title":"Apply Simulation — Design Iteration 2"}'
# → {"doc_id":"1HDS…","kind":"document","format":"md","url":"https://…/edit",
#    "content":"# Idempotent, race-free…","bytes":44212,"title":"…"}
```

Name the document exactly one of three ways — supplying two is a 400, since
guessing between them risks reading the wrong doc:

| Field | Use when |
|---|---|
| `url` | You have a `docs.google.com` link. Carries the document type. |
| `title` | The user named the doc. Resolved against the Drive mount; an ambiguous title returns the candidates so you can pick. |
| `doc_id` | You already have the bare id (e.g. from `/gdocs/list`). |

Other fields: `format`, `kind`, `wait_secs` (default 30, cap 180).

`format` defaults to the best available per type, which is usually what you
want — pass it only to override:

| Document type | Default | Also available |
|---|---|---|
| Doc (`document`) | `md` — real headings, tables, links | `txt`, `html` |
| Sheet (`spreadsheet`) | `csv` — **first sheet only** | `tsv`, `html` |
| Slides (`presentation`) | `txt` | `html` |

Asking for a format a type doesn't support (e.g. `md` for Slides) is a 400, not
a silent empty read. With a bare `doc_id` the type is assumed to be a Doc — pass
`"kind":"spreadsheet"` or `"presentation"` if it isn't, or use the URL instead
and skip the guess.

Requires the user to be signed into Google in Chrome. A permissions failure says
so explicitly and names the document, and a sign-in page is reported as an error
rather than handed back as content — so a successful response is really the
document, never a login page. Nothing is written to disk and no tab is left open.

### GET /gdocs/list — find a document on the Drive mount

Index of the user's Google Drive, for turning a remembered title into an id, or
answering "what docs do I have about X".

```bash
curl -s "http://localhost:8765/gdocs/list?filter=simulation" \
  | python3 -c "import sys,json; d=json.load(sys.stdin); [print(x['kind'],'|',x['title']) for x in d['docs']]"
# → {"docs":[{"title":"Apply Simulation — Design Iteration 2",
#             "doc_id":"1HDS…","kind":"document","path":"/Users/…/My Drive/….gdoc"}],
#    "count":1,"truncated":false,"mounts":["/Users/…/GoogleDrive-user@corp.com"]}
```

`filter` is an optional case-insensitive substring match on the title. Check
`truncated`: when true a traversal cap was hit, so a document missing from
`docs` may still exist — don't report "no such doc" on a truncated listing.

Note the mount is an **index only**. Google-native files sync as ~200-byte JSON
stubs holding just a `doc_id`, so reading a `.gdoc` off disk yields no content —
that's why `/gdocs/read` exists. Real binaries on the mount (`.xlsx`, `.pdf`)
are ordinary files and can be read directly with the Read tool.

Writing to the mount works for plain files and syncs to Drive, but dropping a
`.md` there creates *a file named `.md`*, not a Doc — converting needs the Drive
API. To get formatted content into a Doc, use `/gdocs/clipboard` below. Shared
drives are typically read-only.

## POST /gdocs/clipboard — Markdown → Google Docs, via the clipboard

Converts Markdown to HTML that pastes into Google Docs with formatting intact,
and loads it onto the clipboard. Use it when the user wants a Markdown document
moved into Docs — then tell them to press Cmd-V.

```bash
curl -s -X POST http://localhost:8765/gdocs/clipboard \
  -H 'Content-Type: application/json' \
  -d '{"path":"/abs/path/doc.md"}'
# → {"html_path":"/var/folders/…/gdocs-<uuid>.html","html_bytes":48213,
#    "clipboard_set":true,"clipboard_flavor":"«class HTML», 48213",
#    "code_blocks":26,"languages":["Scala","Diff"],
#    "unhighlighted_languages":["gherkin"],"tables":2,
#    "diagrams":0,"images":0,"warnings":[]}
```

Request fields: `path` (absolute path to a .md file) **or** `markdown` (inline
source) — exactly one; `set_clipboard` (default true; `false` renders and writes
the file only); `out_path` (absolute, must end in `.html`; defaults to a temp
path); `theme` (`"github"` default, `"gdocs"`, `"oceanlight"`,
`"solarizedlight"`); `diagrams` (`"off"` default, `"chrome"` to render Mermaid).

Note on Docs' own **"Code blocks"** building block: it can't be produced by a
paste — Docs applies that highlighting client-side after insertion, so no
clipboard HTML can trigger it. `"theme":"gdocs"` reproduces its palette
(`#b80672` keywords, `#188038` strings, `#1967d2` numbers, `#37474f` text)
instead, which is what to use when a document mixes pasted code with natively
inserted code blocks.

Why HTML and not Markdown: Docs does no Markdown parsing on paste, so raw
Markdown arrives as literal `##` and `**`. Two constraints follow, and both are
already handled here — worth knowing before hand-rolling an alternative:

- **Docs ignores `<style>` blocks entirely.** Every rule must be an inline
  `style=` attribute, so syntax highlighting is emitted as per-token
  `<span style="color:#…">`.
- **The clipboard must carry the `«class HTML»` flavor.** `pbcopy` sets plain
  text, which makes Docs paste visible tags.

### Diagrams and images

Pass `"diagrams":"chrome"` to render ```` ```mermaid ```` fences into embedded
images (headless Chrome + a vendored `mermaid.js`, so no network and no `mmdc`
needed). It is off by default because it spawns Chrome and adds a few seconds per
diagram. Local images referenced as `![alt](pic.png)` are embedded as base64
automatically — relative paths resolve against the source document's directory,
so this only works with `path`, not inline `markdown`. Local `.svg` files need
`"diagrams":"chrome"` too, since Docs cannot render SVG and it must be rasterized
first.

Anything that can't be embedded degrades visibly rather than vanishing: the
diagram source stays as a code block with a note, and a broken diagram reports
Mermaid's actual parse error in `warnings`. Remote image URLs are never fetched.

Check `warnings` before reporting success — it is non-empty when content
degraded. The HTML file is kept after the paste, since clipboards get
overwritten; `grep` it to verify output rather than trusting the summary.

macOS only (the clipboard step uses osascript), and the sidecar must run outside
a sandbox — a sandboxed process has no pasteboard access.

## Health check

```bash
curl -s http://localhost:8765/health 2>/dev/null | grep -q ok && echo "up" || echo "down"
```
<!-- END claude-sidecar -->
