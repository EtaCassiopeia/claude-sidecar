//! Read a Google-native document by driving Google's own export endpoint
//! (`/export?format=…`) through the user's authenticated Chrome session.
//!
//! Why not scrape the DOM: Docs renders to a canvas, so `/browser/fetch` sees
//! only the UI chrome. Measured on a real 44 KB design doc, the DOM route
//! returned ~6 KB polluted with banner text ("Additional Updates to Google
//! Workspace File Sharing", "Don't show this again") while the export returned
//! the whole document as real Markdown — headings, tables, links.
//!
//! Why a same-origin XHR rather than a plain navigation: `export` responds with
//! `Content-Disposition: attachment`, so navigating to it downloads a file into
//! `~/Downloads` and leaves the tab blank. Issuing the request as a synchronous
//! XHR from a `docs.google.com` page keeps the bytes in the page, where the
//! bridge can read them, and writes nothing to disk.
//!
//! Security: the export path is assembled from a strictly validated `doc_id`
//! (`[A-Za-z0-9_-]{20,}`) and a `Format` enum, so no caller-supplied character
//! can escape the JavaScript string literal it lands in. See
//! [`tests::doc_id_rejects_javascript_breaking_characters`].

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::{process::Command, time::timeout};

use crate::{error::SidecarError, gdocs::drive::DocKind};

const OSASCRIPT: &str = "/usr/bin/osascript";

/// Cheapest same-origin page to host the XHR — no editor to boot, no document
/// to load.
const ORIGIN_PAGE: &str = "https://docs.google.com/robots.txt";

const DEFAULT_WAIT_SECS: u64 = 30;
const MAX_WAIT_SECS: u64 = 180;
/// Headroom over the load wait for Chrome startup and moving a large body
/// across the AppleScript boundary.
const SCRIPT_MARGIN_SECS: u64 = 20;

/// Drive ids are base64url-ish and comfortably longer than this; the floor
/// rejects junk like `abc` before it becomes a confusing 404.
const MIN_DOC_ID_LEN: usize = 16;
const MAX_DOC_ID_LEN: usize = 256;

/// Guards against pulling a pathological document through AppleScript, which
/// buffers the whole body in memory.
const MAX_CONTENT_BYTES: usize = 24 * 1024 * 1024;

/// Open a `docs.google.com` page, run the export request from inside it, and
/// close the tab. The URL and JS travel as argv items, never spliced into the
/// script body — the same rule the browser bridge follows.
const EXPORT_SCRIPT: &str = r#"on run argv
    set originUrl to item 1 of argv
    set ticksLeft to (item 2 of argv) as integer
    set exportJs to item 3 of argv
    tell application "Google Chrome"
        if (count of windows) = 0 then make new window
        tell front window to set theTab to make new tab with properties {URL:originUrl}
        repeat while (loading of theTab) and ticksLeft > 0
            delay 0.5
            set ticksLeft to ticksLeft - 1
        end repeat
        delay 0.3
        try
            set payload to execute theTab javascript exportJs
        on error errMsg number errNum
            close theTab
            error errMsg number errNum
        end try
        close theTab
        return payload
    end tell
end run"#;

/// Export format. Each maps to a `format=` value Google accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Google's own Markdown conversion — headings, tables, links preserved.
    #[default]
    Md,
    /// Plain text: no structure, but tolerant of anything.
    Txt,
    Html,
    /// Spreadsheets only, and only the first sheet.
    Csv,
    /// Spreadsheets only, first sheet.
    Tsv,
}

impl Format {
    fn as_str(self) -> &'static str {
        match self {
            Format::Md => "md",
            Format::Txt => "txt",
            Format::Html => "html",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
        }
    }

    /// Reject combinations Google does not serve, so the caller gets a clear
    /// 400 instead of an HTML error page parsed as their document.
    fn check_supported(self, kind: DocKind) -> Result<(), SidecarError> {
        let ok = match kind {
            DocKind::Document => matches!(self, Format::Md | Format::Txt | Format::Html),
            DocKind::Spreadsheet => {
                matches!(self, Format::Csv | Format::Tsv | Format::Html)
            }
            // Slides has no Markdown or plain-text export.
            DocKind::Presentation => matches!(self, Format::Txt | Format::Html),
        };
        if ok {
            return Ok(());
        }
        Err(SidecarError::InvalidRequest(format!(
            "format {:?} is not available for a {:?}; supported: {}",
            self.as_str(),
            kind,
            match kind {
                DocKind::Document => "md, txt, html",
                DocKind::Spreadsheet => "csv, tsv, html",
                DocKind::Presentation => "txt, html",
            }
        )))
    }

    /// The Markdown default is meaningless for a spreadsheet, so each kind gets
    /// the richest format it actually supports.
    pub fn default_for(kind: DocKind) -> Self {
        match kind {
            DocKind::Document => Format::Md,
            DocKind::Spreadsheet => Format::Csv,
            DocKind::Presentation => Format::Txt,
        }
    }
}

/// An exported document.
#[derive(Debug, Serialize)]
pub struct Export {
    pub doc_id: String,
    pub kind: DocKind,
    pub format: Format,
    /// Editor URL, for citing the source.
    pub url: String,
    pub content: String,
    pub bytes: usize,
    /// Title from the Drive index when the caller resolved one by name; the
    /// export response itself carries no title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// What the in-page JavaScript reports back.
#[derive(Deserialize)]
struct XhrResult {
    status: u16,
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    body: String,
}

/// Accept only ids that are safe to interpolate into a URL inside a JavaScript
/// string literal. This is the single check that makes the fixed-template
/// approach sound.
pub fn validate_doc_id(id: &str) -> Result<(), SidecarError> {
    if id.len() < MIN_DOC_ID_LEN || id.len() > MAX_DOC_ID_LEN {
        return Err(SidecarError::InvalidRequest(format!(
            "doc_id must be {MIN_DOC_ID_LEN}–{MAX_DOC_ID_LEN} characters (got {})",
            id.len()
        )));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(SidecarError::InvalidRequest(
            "doc_id must contain only letters, digits, underscore, and hyphen".into(),
        ));
    }
    Ok(())
}

/// Pull a `doc_id` and kind out of a Docs/Sheets/Slides URL.
///
/// The host is matched as an origin prefix, not a substring: otherwise
/// `https://evil.com/docs.google.com/document/d/…` would be accepted as a Docs
/// URL.
pub fn parse_url(url: &str) -> Result<(String, DocKind), SidecarError> {
    let rest = ["https://docs.google.com/", "http://docs.google.com/"]
        .iter()
        .find_map(|prefix| url.strip_prefix(prefix))
        .ok_or_else(|| {
            SidecarError::InvalidRequest(format!(
                "not a docs.google.com URL: {url} \
                 (expected https://docs.google.com/…)"
            ))
        })?;

    let mut parts = rest.split('/');
    let segment = parts.next().unwrap_or_default();
    // Account-scoped URLs insert /u/0/ before the type segment.
    let (segment, mut parts) = if segment == "u" {
        let mut skipped = parts.skip(1); // the account index
        let seg = skipped.next().unwrap_or_default().to_string();
        (seg, skipped.collect::<Vec<_>>().into_iter())
    } else {
        (segment.to_string(), parts.collect::<Vec<_>>().into_iter())
    };

    let kind = DocKind::from_url_segment(&segment).ok_or_else(|| {
        SidecarError::InvalidRequest(format!(
            "unsupported Google URL type {segment:?} — expected document, spreadsheets, or presentation"
        ))
    })?;
    if parts.next() != Some("d") {
        return Err(SidecarError::InvalidRequest(format!(
            "could not find a document id in {url}"
        )));
    }
    let doc_id = parts.next().unwrap_or_default().to_string();
    validate_doc_id(&doc_id)?;
    Ok((doc_id, kind))
}

/// Editor URL for a document — what a human would open.
pub fn editor_url(doc_id: &str, kind: DocKind) -> String {
    format!(
        "https://docs.google.com/{}/d/{doc_id}/edit",
        kind.url_segment()
    )
}

/// Build the in-page script. `doc_id` is validated and `format` is an enum, so
/// the interpolated path cannot contain a quote, backslash, or newline.
///
/// The request is deliberately synchronous: `execute javascript` returns the
/// value of the last expression, so a promise would yield nothing readable.
fn export_js(doc_id: &str, kind: DocKind, format: Format) -> String {
    format!(
        "(function(){{var p='/{segment}/d/{doc_id}/export?format={fmt}';\
         var x=new XMLHttpRequest();x.open('GET',p,false);\
         try{{x.send(null)}}catch(e){{return JSON.stringify({{status:0,content_type:'',body:String(e)}})}}\
         return JSON.stringify({{status:x.status,\
         content_type:(x.getResponseHeader('Content-Type')||''),\
         body:x.responseText}})}})()",
        segment = kind.url_segment(),
        fmt = format.as_str()
    )
}

/// Export a document, returning its content.
pub async fn fetch(
    doc_id: &str,
    kind: DocKind,
    format: Format,
    wait_secs: Option<u64>,
    title: Option<String>,
) -> Result<Export, SidecarError> {
    if !cfg!(target_os = "macos") {
        return Err(SidecarError::Browser(
            "reading Google Docs requires macOS (it drives Chrome via osascript)".into(),
        ));
    }
    validate_doc_id(doc_id)?;
    format.check_supported(kind)?;

    let wait = wait_secs.unwrap_or(DEFAULT_WAIT_SECS).min(MAX_WAIT_SECS);
    let ticks = (wait * 2).to_string();
    let js = export_js(doc_id, kind, format);

    let mut cmd = Command::new(OSASCRIPT);
    cmd.arg("-e")
        .arg(EXPORT_SCRIPT)
        .arg(ORIGIN_PAGE)
        .arg(&ticks)
        .arg(&js)
        .kill_on_drop(true);

    let budget = wait + SCRIPT_MARGIN_SECS;
    let output = timeout(Duration::from_secs(budget), cmd.output())
        .await
        .map_err(|_| SidecarError::Timeout { secs: budget })??;

    if !output.status.success() {
        return Err(browser_error(&String::from_utf8_lossy(&output.stderr)));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let result: XhrResult = serde_json::from_str(raw.trim()).map_err(|_| {
        let excerpt: String = raw.trim().chars().take(200).collect();
        SidecarError::Browser(format!("unexpected export output: {excerpt}"))
    })?;

    let content = interpret(result, doc_id)?;
    Ok(Export {
        doc_id: doc_id.to_string(),
        kind,
        format,
        url: editor_url(doc_id, kind),
        bytes: content.len(),
        content,
        title,
    })
}

/// Turn an XHR result into content, or an error that says what actually went
/// wrong. A sign-in redirect and a 404 both arrive as a *successful* HTTP
/// response body, so status alone is not enough — returning that body as the
/// document would hand the caller a login page and call it their doc.
fn interpret(result: XhrResult, doc_id: &str) -> Result<String, SidecarError> {
    match result.status {
        200 => {}
        0 => {
            return Err(SidecarError::Browser(format!(
                "the export request did not complete: {}",
                result.body
            )))
        }
        401 | 403 => {
            return Err(SidecarError::Browser(format!(
                "Chrome's Google session cannot read {doc_id} (HTTP {}). \
                 Open the document in Chrome and confirm you have access.",
                result.status
            )))
        }
        404 => {
            return Err(SidecarError::Browser(format!(
                "no document {doc_id} (HTTP 404) — check the id, or that the \
                 format is valid for this document type"
            )))
        }
        other => {
            return Err(SidecarError::Browser(format!(
                "export failed with HTTP {other}"
            )))
        }
    }

    if result.body.is_empty() {
        return Err(SidecarError::Browser(format!(
            "export of {doc_id} returned an empty body"
        )));
    }
    if result.body.len() > MAX_CONTENT_BYTES {
        return Err(SidecarError::InvalidRequest(format!(
            "exported document is {} bytes; the limit is {MAX_CONTENT_BYTES}",
            result.body.len()
        )));
    }
    if is_sign_in_page(&result.content_type, &result.body) {
        return Err(SidecarError::Browser(
            "Google returned a sign-in page instead of the document — \
             sign in to Google in Chrome, then retry"
                .into(),
        ));
    }
    Ok(result.body)
}

/// An unauthenticated export 200s with an HTML sign-in page. Requiring *both* an
/// HTML content type and sign-in markers keeps a legitimate `format=html` export
/// from being misread as a failure.
fn is_sign_in_page(content_type: &str, body: &str) -> bool {
    if !content_type.contains("text/html") {
        return false;
    }
    let head: String = body.chars().take(4000).collect::<String>().to_lowercase();
    head.contains("accounts.google.com")
        || head.contains("signinchooser")
        || head.contains("id=\"initialview\"")
}

fn browser_error(stderr: &str) -> SidecarError {
    let msg = stderr.trim();
    let lower = msg.to_lowercase();
    let hint = if lower.contains("javascript") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_drive_ids() {
        assert!(validate_doc_id("1HDS9lW9I81gXKUnTOunAkQt4KVCNx044liIaArjih80").is_ok());
        assert!(validate_doc_id("1DmqLFhnxcIzEb6bT9UVk7YPdHrSCrXyTxodNXWq_rkI").is_ok());
    }

    /// The whole fixed-template argument rests on this: nothing that could close
    /// the JS string literal or the URL may pass.
    #[test]
    fn doc_id_rejects_javascript_breaking_characters() {
        for bad in [
            "abc",                                  // too short
            "1HDS9lW9I81gXKUnTOunAkQt'+alert(1)+'", // quote escape
            "1HDS9lW9I81gXKUnTOunAkQt\\x27",        // backslash
            "1HDS9lW9I81gXKUnTOunAkQt\nnewline",    // newline
            "1HDS9lW9I81gXKUnTOunAkQt/../secret",   // path traversal
            "1HDS9lW9I81gXKUnTOunAkQt?x=1",         // query injection
            "1HDS9lW9I81gXKUnTOunAkQt#frag",
            "1HDS9lW9I81gXKUnTOunAkQt spaced",
        ] {
            assert!(
                validate_doc_id(bad).is_err(),
                "must reject {bad:?} — it could break out of the script"
            );
        }
    }

    /// Guards the invariant directly: an id that validates contributes no quote
    /// or backslash of its own, so the path stays inside one string literal.
    /// Comparing against a baseline id keeps this honest without hardcoding a
    /// count of the quotes the template itself contains.
    #[test]
    fn generated_js_keeps_the_id_inside_one_string_literal() {
        let id = "1HDS9lW9I81gXKUnTOunAkQt4KVCNx044liIaArjih80";
        let js = export_js(id, DocKind::Document, Format::Md);
        assert!(js.contains(&format!("/document/d/{id}/export?format=md")));

        // Same template, different-length id: the quote count must be identical,
        // because the id itself may contribute none.
        let baseline = export_js("1BBBBBBBBBBBBBBBB", DocKind::Document, Format::Md);
        assert_eq!(
            js.matches('\'').count(),
            baseline.matches('\'').count(),
            "the id changed the quote count, so it escaped its literal: {js}"
        );
        assert!(!js.contains('\\'), "no backslashes expected: {js}");
        // The path literal is closed by the quote we wrote, with nothing between
        // the id and it.
        assert!(
            js.contains(&format!("{id}/export?format=md';")),
            "got: {js}"
        );
    }

    #[test]
    fn spreadsheet_js_uses_the_plural_segment() {
        let js = export_js("1AAAAAAAAAAAAAAAAAAA", DocKind::Spreadsheet, Format::Csv);
        assert!(js.contains("/spreadsheets/d/"), "got: {js}");
        assert!(js.contains("format=csv"));
    }

    #[test]
    fn parses_editor_urls() {
        let (id, kind) = parse_url(
            "https://docs.google.com/document/d/1HDS9lW9I81gXKUnTOunAkQt4KVCNx044liIaArjih80/edit",
        )
        .unwrap();
        assert_eq!(id, "1HDS9lW9I81gXKUnTOunAkQt4KVCNx044liIaArjih80");
        assert_eq!(kind, DocKind::Document);
    }

    #[test]
    fn parses_account_scoped_and_sheet_urls() {
        let (id, kind) = parse_url(
            "https://docs.google.com/u/0/spreadsheets/d/1AAAAAAAAAAAAAAAAAAAA/edit#gid=0",
        )
        .unwrap();
        assert_eq!(id, "1AAAAAAAAAAAAAAAAAAAA");
        assert_eq!(kind, DocKind::Spreadsheet);
    }

    #[test]
    fn parses_urls_without_a_trailing_action() {
        let (id, kind) =
            parse_url("https://docs.google.com/presentation/d/1BBBBBBBBBBBBBBBBBBBB").unwrap();
        assert_eq!(id, "1BBBBBBBBBBBBBBBBBBBB");
        assert_eq!(kind, DocKind::Presentation);
    }

    #[test]
    fn rejects_non_docs_urls() {
        assert!(parse_url("https://example.com/document/d/1AAAAAAAAAAAAAAAAAAAA").is_err());
        assert!(parse_url("https://docs.google.com/drawings/d/1AAAAAAAAAAAAAAAAAAAA").is_err());
        assert!(parse_url("https://docs.google.com/document/1AAAAAAAAAAAAAAAAAAAA").is_err());
    }

    /// A host that merely *contains* the string must not pass as Docs, or a
    /// hostile link could point the export at another origin.
    #[test]
    fn rejects_lookalike_hosts() {
        for bad in [
            "https://evil.com/docs.google.com/document/d/1AAAAAAAAAAAAAAAAAAAA/edit",
            "https://docs.google.com.evil.com/document/d/1AAAAAAAAAAAAAAAAAAAA/edit",
            "https://notdocs.google.com/document/d/1AAAAAAAAAAAAAAAAAAAA/edit",
        ] {
            assert!(parse_url(bad).is_err(), "must reject lookalike host: {bad}");
        }
    }

    #[test]
    fn format_defaults_suit_each_kind() {
        assert_eq!(Format::default_for(DocKind::Document), Format::Md);
        assert_eq!(Format::default_for(DocKind::Spreadsheet), Format::Csv);
        assert_eq!(Format::default_for(DocKind::Presentation), Format::Txt);
    }

    #[test]
    fn unsupported_format_kind_pairs_rejected() {
        // Slides has no Markdown export; asking for it would 404 confusingly.
        assert!(Format::Md.check_supported(DocKind::Presentation).is_err());
        assert!(Format::Csv.check_supported(DocKind::Document).is_err());
        assert!(Format::Md.check_supported(DocKind::Document).is_ok());
        assert!(Format::Csv.check_supported(DocKind::Spreadsheet).is_ok());
        assert!(Format::Html.check_supported(DocKind::Presentation).is_ok());
    }

    fn xhr(status: u16, content_type: &str, body: &str) -> XhrResult {
        XhrResult {
            status,
            content_type: content_type.into(),
            body: body.into(),
        }
    }

    #[test]
    fn a_200_markdown_body_is_returned() {
        let content = interpret(xhr(200, "text/markdown", "# Title\n\ntext"), "1A").unwrap();
        assert_eq!(content, "# Title\n\ntext");
    }

    /// The failure that matters most: an unauthenticated export 200s with a
    /// login page, and returning it would look like a successful read.
    #[test]
    fn sign_in_page_is_an_error_not_content() {
        let body = "<html><head><title>Sign in</title></head>\
                    <body><form action=\"https://accounts.google.com/signin\">…</form></body></html>";
        let err = interpret(xhr(200, "text/html; charset=utf-8", body), "1A").unwrap_err();
        assert!(err.to_string().contains("sign-in page"), "got: {err}");
    }

    /// …but a legitimate `format=html` export must not be mistaken for one.
    #[test]
    fn genuine_html_export_is_not_flagged_as_sign_in() {
        let body = "<html><body><h1>Design</h1><p>Real content</p></body></html>";
        let content = interpret(xhr(200, "text/html", body), "1A").unwrap();
        assert!(content.contains("Real content"));
    }

    #[test]
    fn permission_and_missing_doc_errors_are_actionable() {
        let denied = interpret(xhr(403, "text/html", ""), "1XYZ").unwrap_err();
        assert!(denied.to_string().contains("1XYZ"), "got: {denied}");
        assert!(denied.to_string().contains("access"), "got: {denied}");

        let missing = interpret(xhr(404, "text/html", ""), "1XYZ").unwrap_err();
        assert!(missing.to_string().contains("404"), "got: {missing}");
    }

    #[test]
    fn empty_body_is_an_error() {
        let err = interpret(xhr(200, "text/markdown", ""), "1A").unwrap_err();
        assert!(err.to_string().contains("empty"), "got: {err}");
    }

    /// A network-level failure inside the page reports status 0 and carries the
    /// exception text; it must not be mistaken for an empty document.
    #[test]
    fn network_failure_reports_the_exception() {
        let err = interpret(xhr(0, "", "NetworkError: failed"), "1A").unwrap_err();
        assert!(err.to_string().contains("NetworkError"), "got: {err}");
    }

    #[test]
    fn editor_urls_are_built_per_kind() {
        assert_eq!(
            editor_url("1AAAAAAAAAAAAAAAAAAAA", DocKind::Spreadsheet),
            "https://docs.google.com/spreadsheets/d/1AAAAAAAAAAAAAAAAAAAA/edit"
        );
    }

    /// Nothing caller-supplied may reach the AppleScript body.
    #[test]
    fn script_does_not_interpolate_arguments() {
        assert!(EXPORT_SCRIPT.contains("item 1 of argv"));
        assert!(EXPORT_SCRIPT.contains("item 3 of argv"));
        assert!(!EXPORT_SCRIPT.contains("{}"));
    }
}
