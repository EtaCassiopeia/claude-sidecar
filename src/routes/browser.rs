//! Browser bridge (macOS) — read page content through the user's real Chrome
//! session via AppleScript, so pages behind a login or paywall the user already
//! has access to are readable from the sandbox.
//!
//! YouTube watch pages are a special case handled by a second extractor: what a
//! reader wants from a video is what is said in it, and that is not in the DOM.
//! See `extract_youtube.js`.
//!
//! Unlike `/exec`, nothing here is caller-controlled beyond the target URL: the
//! AppleScript and the JavaScript executed in the page are fixed templates, and
//! the URL travels as an osascript argv item — it is never spliced into script
//! text — so this endpoint cannot be used to script arbitrary applications.
//!
//! One-time Chrome setup: View → Developer → Allow JavaScript from Apple
//! Events, plus the macOS Automation permission prompt on first use.

use std::time::{Duration, Instant};

use axum::{
    extract::{Json, Query, State},
    response::Json as JsonResponse,
};
use htmd::HtmlToMarkdown;
use serde::{Deserialize, Serialize};
use tokio::{process::Command, time::timeout};

use crate::{error::SidecarError, events::ActivityKind, logger, AppState};

/// Always present on macOS; deliberately not part of `ALLOWED_COMMANDS` —
/// callers get these two fixed scripts, not general osascript access.
const OSASCRIPT: &str = "/usr/bin/osascript";

const DEFAULT_WAIT_SECS: u64 = 20;
const MAX_WAIT_SECS: u64 = 120;
/// Extra headroom on top of the page-load wait for Chrome startup, script
/// evaluation, and serializing large pages.
const SCRIPT_MARGIN_SECS: u64 = 15;
/// `/browser/tab` reads an already-loaded tab, so it only needs the margin.
const TAB_TIMEOUT_SECS: u64 = SCRIPT_MARGIN_SECS;

/// Ticks (of 0.5s) the prepare step may spend before extraction runs anyway.
/// Only a YouTube watch page ever uses them; everything else answers `"ready"`
/// on the first call.
const PREPARE_TICKS: u32 = 20;

/// Open the URL in a new tab of the front window, wait for it to finish
/// loading (bounded by the tick budget), give the prepare step its own bounded
/// run, extract the page, and close the tab unless asked to keep it.
/// Extraction runs even if either budget runs out — partial content beats none.
const FETCH_SCRIPT: &str = r#"on run argv
    set theUrl to item 1 of argv
    set ticksLeft to (item 2 of argv) as integer
    set prepareJs to item 3 of argv
    set extractJs to item 4 of argv
    set keepTab to item 5 of argv
    set prepareTicks to (item 6 of argv) as integer
    tell application "Google Chrome"
        if (count of windows) = 0 then make new window
        tell front window to set theTab to make new tab with properties {URL:theUrl}
        repeat while (loading of theTab) and ticksLeft > 0
            delay 0.5
            set ticksLeft to ticksLeft - 1
        end repeat
        delay 0.5
        try
            repeat while prepareTicks > 0
                if (execute theTab javascript prepareJs) is "ready" then exit repeat
                delay 0.5
                set prepareTicks to prepareTicks - 1
            end repeat
            set payload to execute theTab javascript extractJs
        on error errMsg number errNum
            if keepTab is "0" then close theTab
            error errMsg number errNum
        end try
        if keepTab is "0" then close theTab
        return payload
    end tell
end run"#;

const TAB_SCRIPT: &str = r#"on run argv
    set extractJs to item 1 of argv
    tell application "Google Chrome"
        if (count of windows) = 0 then error "no Chrome window is open"
        return execute (active tab of front window) javascript extractJs
    end tell
end run"#;

/// Driven in a loop by `FETCH_SCRIPT` until it answers `"ready"`. YouTube
/// renders a video's transcript only once the viewer asks for it, and Chrome's
/// `execute javascript` returns synchronously and cannot await, so the waiting
/// has to live in AppleScript — which in turn means this step is called
/// repeatedly and must be idempotent. Clicking the button a second time would
/// close the panel again, hence the two checks before the click.
///
/// Anything that is not a YouTube watch page — overwhelmingly the common case —
/// answers on the first call, so this costs other fetches a single round trip.
const OPEN_TRANSCRIPT_JS: &str = r##"(() => {
  if (!/(^|\.)youtube\.com$/.test(location.hostname)) return "ready";
  const watch =
    new URLSearchParams(location.search).get("v") || /^\/shorts\//.test(location.pathname);
  if (!watch) return "ready";
  if (document.querySelector("ytd-transcript-segment-renderer")) return "ready";
  // No caption track in the page's own player data means no transcript button
  // will ever appear, and waiting for one would just burn the budget.
  const captioned = [...document.querySelectorAll("script")].some((s) =>
    (s.textContent || "").includes("\"captionTracks\""));
  if (!captioned) return "ready";
  // The panel is open but still filling in; waiting is all that is left.
  if (document.querySelector("ytd-transcript-renderer")) return "wait";
  document.querySelector("#description-inline-expander #expand")?.click();
  const button = [...document.querySelectorAll("button, tp-yt-paper-button")].find((el) =>
    `${el.getAttribute("aria-label") || ""} ${el.textContent || ""}`
      .toLowerCase()
      .includes("transcript"));
  if (button) button.click();
  return "wait";
})()"##;

/// The prepare step for formats that want the page exactly as it stands.
const ALREADY_READY_JS: &str = r#""ready""#;

const EXTRACT_TEXT_JS: &str =
    "JSON.stringify({url:location.href,title:document.title,content:document.body.innerText})";
const EXTRACT_HTML_JS: &str = "JSON.stringify({url:location.href,title:document.title,content:document.documentElement.outerHTML})";
/// The article extractor exports `(keepLinks) => json` and the YouTube
/// extractor `() => json | null`; these are their only call sites. `concat!`
/// builds them at compile time, so each remains fixed script text — a request
/// can select between them but can never contribute to them.
///
/// The YouTube extractor is chained in front because a watch page defeats the
/// article extractor entirely: what a reader wants is the spoken word, which is
/// not in the DOM. It returns `null` on anything that is not a watch page, so
/// `||` hands the page straight to the article path in every other case.
const EXTRACT_MARKDOWN_JS: &str = concat!(
    "(",
    include_str!("extract_youtube.js"),
    ")() || (",
    include_str!("extract_markdown.js"),
    ")(false)"
);
const EXTRACT_MARKDOWN_LINKS_JS: &str = concat!(
    "(",
    include_str!("extract_youtube.js"),
    ")() || (",
    include_str!("extract_markdown.js"),
    ")(true)"
);

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Main content only, as markdown. The default because it is the cheapest
    /// faithful rendering: site chrome is dropped and structure survives. On a
    /// YouTube watch page this yields the video's transcript, metadata, and
    /// related-video links instead of the DOM.
    #[default]
    Markdown,
    /// Rendered text of the whole page (`document.body.innerText`) — the escape
    /// hatch for when the extractor picks the wrong block, and the way to read
    /// a YouTube watch page as a page rather than as a transcript.
    Text,
    /// Full DOM (`document.documentElement.outerHTML`).
    Html,
    /// Full DOM converted to markdown server-side. Keeps everything `Markdown`
    /// drops, so it is the fallback for a page whose real content the article
    /// extractor scores away — and for canvas-rendered docs, where `Text`
    /// returns only the UI chrome. Far smaller than `Html` because scripts,
    /// styles and `<head>` are skipped during conversion (see
    /// `html_to_markdown`). For Google Docs/Sheets/Slides use `/gdocs/read`
    /// instead: it returns the whole document rather than the fraction this
    /// recovers.
    Dom,
}

impl Format {
    /// `include_links` only reaches the markdown extractor; the other scripts
    /// have no notion of it.
    fn extract_js(self, include_links: bool) -> &'static str {
        match self {
            Format::Markdown if include_links => EXTRACT_MARKDOWN_LINKS_JS,
            Format::Markdown => EXTRACT_MARKDOWN_JS,
            Format::Text => EXTRACT_TEXT_JS,
            // `Dom` is derived from the same HTML capture, converted in `finish`.
            Format::Html | Format::Dom => EXTRACT_HTML_JS,
        }
    }

    /// Run repeatedly before extraction until it answers `"ready"`. Only the
    /// markdown path asks the page for anything: it is what opens a YouTube
    /// transcript panel, while `text`, `html` and `dom` are documented as
    /// reading the page exactly as it stands.
    fn prepare_js(self) -> &'static str {
        match self {
            Format::Markdown => OPEN_TRANSCRIPT_JS,
            Format::Text | Format::Html | Format::Dom => ALREADY_READY_JS,
        }
    }

    /// Post-process a freshly extracted page. `Dom` converts the captured HTML;
    /// every other format is already in its final shape.
    fn finish(self, page: Page) -> Result<Page, SidecarError> {
        match self {
            Format::Dom => Ok(Page {
                content: html_to_markdown(&page.content)?,
                ..page
            }),
            Format::Markdown | Format::Text | Format::Html => Ok(page),
        }
    }
}

/// Convert captured HTML to Markdown. The capture is the whole document
/// (`documentElement.outerHTML`), so non-content tags — styles, scripts,
/// `<head>` metadata — are skipped; otherwise their text leaks into the output
/// (e.g. inline CSS rendered as a paragraph). A failure here is a server-side
/// processing fault (the extraction already succeeded), so it propagates as
/// `Internal` rather than being silently returned as raw HTML.
fn html_to_markdown(html: &str) -> Result<String, SidecarError> {
    HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "head", "noscript", "iframe"])
        .build()
        .convert(html)
        .map_err(|e| SidecarError::Internal(format!("HTML-to-Markdown conversion failed: {e}")))
}

#[derive(Debug, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    /// Max seconds to wait for the page to finish loading (default 20, cap 120).
    pub wait_secs: Option<u64>,
    #[serde(default)]
    pub format: Format,
    /// Leave the tab open after extraction (useful for debugging what the page
    /// actually rendered).
    #[serde(default)]
    pub keep_tab: bool,
    /// Cap the returned content at this many characters.
    pub max_chars: Option<usize>,
    /// Keep link and image targets in markdown output. Off by default because
    /// URLs are a large share of the bytes and are rarely what the caller came
    /// for; link text is always kept either way.
    #[serde(default)]
    pub include_links: bool,
}

#[derive(Debug, Deserialize)]
pub struct TabQuery {
    #[serde(default)]
    pub format: Format,
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub include_links: bool,
}

/// Extracted page. Doubles as the JSON shape produced by the in-page
/// JavaScript, so deserializing the script output yields the response directly
/// — `truncated` is the one field the caller-side sets, hence its default.
#[derive(Debug, Serialize, Deserialize)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub truncated: bool,
}

impl Page {
    /// Cut `content` to `max_chars` characters, flagging the response so a
    /// short page and a clipped one are never confused.
    fn truncate(mut self, max_chars: Option<usize>) -> Self {
        let Some(max) = max_chars else { return self };
        if self.content.chars().count() <= max {
            return self;
        }
        self.content = self.content.chars().take(max).collect();
        self.truncated = true;
        self
    }
}

/// `POST /browser/fetch` — open a URL in the user's Chrome and return the
/// rendered page.
pub async fn fetch(
    State(state): State<AppState>,
    Json(req): Json<FetchRequest>,
) -> Result<JsonResponse<Page>, SidecarError> {
    validate_url(&req.url).map_err(SidecarError::InvalidRequest)?;
    let wait_secs = req
        .wait_secs
        .unwrap_or(DEFAULT_WAIT_SECS)
        .min(MAX_WAIT_SECS);
    // The load-wait loop ticks every 0.5s.
    let ticks = (wait_secs * 2).to_string();
    let keep_tab = if req.keep_tab { "1" } else { "0" };

    logger::log_request(
        "POST",
        "/browser/fetch",
        "chrome",
        std::slice::from_ref(&req.url),
        None,
    );
    let started = Instant::now();
    let activity = state.events.start(
        ActivityKind::Browser,
        "chrome fetch",
        std::slice::from_ref(&req.url),
        None,
    );
    let prepare_ticks = PREPARE_TICKS.to_string();
    let result = run_script(
        FETCH_SCRIPT,
        &[
            &req.url,
            &ticks,
            req.format.prepare_js(),
            req.format.extract_js(req.include_links),
            keep_tab,
            &prepare_ticks,
        ],
        // The prepare loop runs on the same clock as the page load, so its
        // budget has to be covered here too or a slow transcript panel would
        // time the whole call out.
        wait_secs + SCRIPT_MARGIN_SECS + u64::from(PREPARE_TICKS) / 2,
    )
    .await
    .and_then(|page| req.format.finish(page));
    logger::log_completion(
        "/browser/fetch",
        Some(if result.is_ok() { 0 } else { 1 }),
        started.elapsed().as_millis(),
    );
    report(&state, activity, &result);
    result.map(|page| JsonResponse(page.truncate(req.max_chars)))
}

/// `GET /browser/tab` — return the page currently focused in Chrome. Lets the
/// user navigate somewhere themselves and say "read this".
pub async fn tab(
    State(state): State<AppState>,
    Query(q): Query<TabQuery>,
) -> Result<JsonResponse<Page>, SidecarError> {
    logger::log_request("GET", "/browser/tab", "chrome", &[], None);
    let started = Instant::now();
    let activity = state
        .events
        .start(ActivityKind::Browser, "chrome tab", &[], None);
    let result = run_script(
        TAB_SCRIPT,
        &[q.format.extract_js(q.include_links)],
        TAB_TIMEOUT_SECS,
    )
    .await
    .and_then(|page| q.format.finish(page));
    logger::log_completion(
        "/browser/tab",
        Some(if result.is_ok() { 0 } else { 1 }),
        started.elapsed().as_millis(),
    );
    report(&state, activity, &result);
    result.map(|page| JsonResponse(page.truncate(q.max_chars)))
}

/// Close out an activity from a fallible result: a failure carries its reason
/// rather than a synthesized exit code it never had.
fn report(state: &AppState, activity: u64, result: &Result<Page, SidecarError>) {
    match result {
        Ok(_) => state.events.finish(activity, Some(0), None),
        Err(e) => state.events.finish(activity, None, Some(e.to_string())),
    }
}

async fn run_script(script: &str, args: &[&str], timeout_secs: u64) -> Result<Page, SidecarError> {
    if !cfg!(target_os = "macos") {
        return Err(SidecarError::Browser(
            "the browser bridge requires macOS (it drives Chrome via osascript)".into(),
        ));
    }

    let mut cmd = Command::new(OSASCRIPT);
    cmd.arg("-e").arg(script).args(args).kill_on_drop(true);

    let output = timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| SidecarError::Timeout { secs: timeout_secs })??;

    if !output.status.success() {
        return Err(browser_error(&String::from_utf8_lossy(&output.stderr)));
    }
    parse_page(String::from_utf8_lossy(&output.stdout).trim())
}

fn parse_page(raw: &str) -> Result<Page, SidecarError> {
    serde_json::from_str(raw).map_err(|_| {
        let excerpt: String = raw.chars().take(200).collect();
        SidecarError::Browser(format!("unexpected script output: {excerpt}"))
    })
}

/// Map an osascript failure to an actionable error. The two setup failures
/// every new user hits get explicit remediation hints.
fn browser_error(stderr: &str) -> SidecarError {
    let msg = stderr.trim();
    let lower = msg.to_lowercase();
    let hint = if lower.contains("javascript") {
        // "Executing JavaScript through AppleScript is turned off."
        Some("enable View > Developer > Allow JavaScript from Apple Events in Chrome")
    } else if msg.contains("-1743") || lower.contains("not authorized") {
        Some(
            "grant this terminal Automation access to Google Chrome in \
             System Settings > Privacy & Security > Automation",
        )
    } else {
        None
    };
    match hint {
        Some(hint) => SidecarError::Browser(format!("{msg} ({hint})")),
        None => SidecarError::Browser(msg.to_string()),
    }
}

fn validate_url(url: &str) -> Result<(), String> {
    // Scheme allowlist keeps `javascript:`, `file:`, `chrome:` etc. out of the
    // user's browser.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("url must start with http:// or https://".to_string());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("url must not contain whitespace or control characters".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every format, so a new variant cannot quietly skip the shared contracts
    /// asserted below.
    const ALL: [Format; 4] = [Format::Markdown, Format::Text, Format::Html, Format::Dom];

    #[test]
    fn https_and_http_urls_allowed() {
        assert!(validate_url("https://medium.com/some-article").is_ok());
        assert!(validate_url("http://localhost:3000/page").is_ok());
    }

    #[test]
    fn non_http_schemes_rejected() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("chrome://settings").is_err());
        assert!(validate_url("ftp://example.com").is_err());
    }

    #[test]
    fn urls_with_whitespace_rejected() {
        assert!(validate_url("https://example.com/a b").is_err());
        assert!(validate_url("https://example.com/\n").is_err());
    }

    #[test]
    fn page_parses_from_script_output() {
        let page = parse_page(r#"{"url":"https://x.com/","title":"T","content":"body"}"#)
            .expect("valid page JSON");
        assert_eq!(page.title, "T");
        assert_eq!(page.content, "body");
    }

    #[test]
    fn non_json_script_output_is_browser_error() {
        // Chrome returns "missing value" when the JS evaluates to undefined.
        assert!(matches!(
            parse_page("missing value"),
            Err(SidecarError::Browser(_))
        ));
    }

    #[test]
    fn fetch_request_defaults() {
        let req: FetchRequest = serde_json::from_str(r#"{"url":"https://x.com"}"#)
            .expect("minimal request deserializes");
        assert!(matches!(req.format, Format::Markdown));
        assert!(!req.keep_tab);
        assert!(req.wait_secs.is_none());
        assert!(req.max_chars.is_none());
        assert!(!req.include_links);
    }

    #[test]
    fn every_format_maps_to_a_script_returning_the_page_shape() {
        for format in ALL {
            for links in [false, true] {
                let js = format.extract_js(links);
                assert!(js.contains("JSON.stringify"), "{format:?} must emit JSON");
                for field in ["url", "title", "content"] {
                    assert!(js.contains(field), "{format:?} must emit {field}");
                }
            }
        }
    }

    #[test]
    fn include_links_selects_a_different_markdown_script() {
        assert_ne!(
            Format::Markdown.extract_js(false),
            Format::Markdown.extract_js(true)
        );
        assert!(Format::Markdown.extract_js(true).ends_with(")(true)"));
        assert!(Format::Markdown.extract_js(false).ends_with(")(false)"));
    }

    #[test]
    fn markdown_tries_youtube_before_the_article_extractor() {
        for links in [false, true] {
            let js = Format::Markdown.extract_js(links);
            let youtube = js
                .find("youtube.com")
                .expect("YouTube extractor is present");
            let article = js
                .find("Readability")
                .expect("article extractor is present");
            assert!(youtube < article, "YouTube must be tried first");
            // The fallback is what keeps every non-YouTube page working.
            assert!(js.contains(")() || ("), "the two must be chained with ||");
        }
    }

    #[test]
    fn only_markdown_reaches_into_the_page_before_extracting() {
        assert_eq!(Format::Markdown.prepare_js(), OPEN_TRANSCRIPT_JS);
        for format in [Format::Text, Format::Html, Format::Dom] {
            assert_eq!(format.prepare_js(), ALREADY_READY_JS);
        }
    }

    #[test]
    fn every_prepare_step_can_answer_ready() {
        // The AppleScript loop exits on "ready" and otherwise spends its whole
        // budget, so a step with no path to that string would stall the fetch.
        for format in ALL {
            assert!(format.prepare_js().contains(r#""ready""#));
        }
    }

    #[test]
    fn only_markdown_gets_youtube_handling() {
        // `text`, `html` and `dom` are the documented ways to read a watch page
        // as a page; they must stay untouched by the chain.
        for format in [Format::Text, Format::Html, Format::Dom] {
            assert!(!format.extract_js(false).contains("youtube.com"));
        }
    }

    #[test]
    fn include_links_only_affects_markdown() {
        for format in [Format::Text, Format::Html, Format::Dom] {
            assert_eq!(format.extract_js(false), format.extract_js(true));
        }
    }

    #[test]
    fn dom_format_deserializes_and_captures_html() {
        let req: FetchRequest = serde_json::from_str(r#"{"url":"https://x.com","format":"dom"}"#)
            .expect("dom request deserializes");
        assert!(matches!(req.format, Format::Dom));
        // Dom is derived from the HTML capture, so it uses the HTML JS.
        assert_eq!(req.format.extract_js(false), EXTRACT_HTML_JS);
    }

    #[test]
    fn dom_finish_converts_captured_html() {
        let converted = Format::Dom
            .finish(page("<h1>Title</h1><p>Hello <strong>world</strong></p>"))
            .expect("conversion succeeds");
        assert_eq!(converted.url, "https://x.com/");
        assert_eq!(converted.title, "T");
        assert!(
            converted.content.contains("# Title"),
            "expected a Markdown heading, got: {}",
            converted.content
        );
        assert!(
            converted.content.contains("**world**"),
            "expected bold Markdown, got: {}",
            converted.content
        );
    }

    #[test]
    fn formats_other_than_dom_finish_pass_through() {
        for format in [Format::Markdown, Format::Text, Format::Html] {
            assert_eq!(
                format.finish(page("<p>raw</p>")).unwrap().content,
                "<p>raw</p>",
                "{format:?} must not post-process"
            );
        }
    }

    #[test]
    fn dom_skips_style_and_script_noise() {
        // A full-document capture carries <head><style>…</style></head> and
        // inline scripts; neither should leak into the Markdown body.
        let md = Format::Dom
            .finish(page(
                "<html><head><style>body{color:red}</style></head>\
                 <body><script>alert(1)</script><p>Real content</p></body></html>",
            ))
            .unwrap()
            .content;
        assert!(md.contains("Real content"), "got: {md}");
        assert!(!md.contains("color:red"), "CSS leaked into markdown: {md}");
        assert!(
            !md.contains("alert(1)"),
            "script leaked into markdown: {md}"
        );
    }

    fn page(content: &str) -> Page {
        Page {
            url: "https://x.com/".into(),
            title: "T".into(),
            content: content.into(),
            truncated: false,
        }
    }

    #[test]
    fn truncate_clips_and_flags_only_when_over_the_cap() {
        let clipped = page("hello world").truncate(Some(5));
        assert_eq!(clipped.content, "hello");
        assert!(clipped.truncated);

        let untouched = page("hello").truncate(Some(5));
        assert_eq!(untouched.content, "hello");
        assert!(!untouched.truncated);
    }

    #[test]
    fn truncate_is_a_no_op_without_a_cap() {
        let kept = page("hello world").truncate(None);
        assert_eq!(kept.content, "hello world");
        assert!(!kept.truncated);
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // Slicing by byte offset here would panic mid-codepoint.
        let clipped = page("héllo→wörld").truncate(Some(6));
        assert_eq!(clipped.content, "héllo→");
        assert!(clipped.truncated);
    }

    #[test]
    fn page_parses_without_the_truncated_flag() {
        // The in-page script never emits it; it is set on the Rust side.
        let page = parse_page(r#"{"url":"https://x.com/","title":"T","content":"body"}"#)
            .expect("script output deserializes");
        assert!(!page.truncated);
    }
}
