//! Browser bridge (macOS) — read page content through the user's real Chrome
//! session via AppleScript, so pages behind a login or paywall the user already
//! has access to are readable from the sandbox.
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
    extract::{Json, Query},
    response::Json as JsonResponse,
};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, time::timeout};

use crate::{error::SidecarError, logger};

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

/// Open the URL in a new tab of the front window, wait for it to finish
/// loading (bounded by the tick budget), extract the page, and close the tab
/// unless asked to keep it. Extraction runs even if the tick budget runs out —
/// partial content beats none.
const FETCH_SCRIPT: &str = r#"on run argv
    set theUrl to item 1 of argv
    set ticksLeft to (item 2 of argv) as integer
    set extractJs to item 3 of argv
    set keepTab to item 4 of argv
    tell application "Google Chrome"
        if (count of windows) = 0 then make new window
        tell front window to set theTab to make new tab with properties {URL:theUrl}
        repeat while (loading of theTab) and ticksLeft > 0
            delay 0.5
            set ticksLeft to ticksLeft - 1
        end repeat
        delay 0.5
        try
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

const EXTRACT_TEXT_JS: &str =
    "JSON.stringify({url:location.href,title:document.title,content:document.body.innerText})";
const EXTRACT_HTML_JS: &str = "JSON.stringify({url:location.href,title:document.title,content:document.documentElement.outerHTML})";

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Rendered text (`document.body.innerText`) — what a reader sees.
    #[default]
    Text,
    /// Full DOM (`document.documentElement.outerHTML`).
    Html,
}

impl Format {
    fn extract_js(self) -> &'static str {
        match self {
            Format::Text => EXTRACT_TEXT_JS,
            Format::Html => EXTRACT_HTML_JS,
        }
    }
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
}

#[derive(Debug, Deserialize)]
pub struct TabQuery {
    #[serde(default)]
    pub format: Format,
}

/// Extracted page. Doubles as the JSON shape produced by the in-page
/// JavaScript, so deserializing the script output yields the response directly.
#[derive(Debug, Serialize, Deserialize)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub content: String,
}

/// `POST /browser/fetch` — open a URL in the user's Chrome and return the
/// rendered page.
pub async fn fetch(Json(req): Json<FetchRequest>) -> Result<JsonResponse<Page>, SidecarError> {
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
    let result = run_script(
        FETCH_SCRIPT,
        &[&req.url, &ticks, req.format.extract_js(), keep_tab],
        wait_secs + SCRIPT_MARGIN_SECS,
    )
    .await;
    logger::log_completion(
        "/browser/fetch",
        Some(if result.is_ok() { 0 } else { 1 }),
        started.elapsed().as_millis(),
    );
    result.map(JsonResponse)
}

/// `GET /browser/tab` — return the page currently focused in Chrome. Lets the
/// user navigate somewhere themselves and say "read this".
pub async fn tab(Query(q): Query<TabQuery>) -> Result<JsonResponse<Page>, SidecarError> {
    logger::log_request("GET", "/browser/tab", "chrome", &[], None);
    let started = Instant::now();
    let result = run_script(TAB_SCRIPT, &[q.format.extract_js()], TAB_TIMEOUT_SECS).await;
    logger::log_completion(
        "/browser/tab",
        Some(if result.is_ok() { 0 } else { 1 }),
        started.elapsed().as_millis(),
    );
    result.map(JsonResponse)
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
        assert!(matches!(req.format, Format::Text));
        assert!(!req.keep_tab);
        assert!(req.wait_secs.is_none());
    }
}
