//! `POST /gdocs/clipboard` — convert Markdown to Google-Docs-pasteable HTML and
//! load it onto the clipboard.
//!
//! Rendering is pure, so `set_clipboard: false` gives a full conversion with no
//! process spawn — that is the mode the tests use.

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{
    error::SidecarError,
    events::ActivityKind,
    gdocs::{self, Backend, RenderOptions, ThemeChoice},
    logger, AppState,
};

/// Loading the clipboard is the point of the endpoint, so it defaults on.
const DEFAULT_SET_CLIPBOARD: bool = true;

/// Generous for prose; the largest real fixture is ~30 KB.
const MAX_MARKDOWN_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct ConvertRequest {
    /// Absolute path to a Markdown file. Mutually exclusive with `markdown`.
    pub path: Option<String>,
    /// Inline Markdown source. Mutually exclusive with `path`.
    pub markdown: Option<String>,
    /// Load the result onto the clipboard. Defaults to true; `false` renders and
    /// writes the file only.
    pub set_clipboard: Option<bool>,
    /// Where to write the HTML. Defaults to a generated temp path.
    pub out_path: Option<String>,
    #[serde(default)]
    pub theme: ThemeChoice,
    #[serde(default)]
    pub diagrams: Backend,
}

#[derive(Debug, Serialize)]
pub struct ConvertResponse {
    /// The HTML is kept on disk: clipboards get overwritten, and this is what
    /// makes the output greppable.
    pub html_path: String,
    pub html_bytes: usize,
    pub clipboard_set: bool,
    /// Raw `clipboard info` output, e.g. `«class HTML», 48213`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clipboard_flavor: Option<String>,
    pub code_blocks: usize,
    /// Grammars actually applied, e.g. `["Scala", "Diff"]`.
    pub languages: Vec<String>,
    /// Fence languages with no grammar available, which fell back to plain text.
    pub unhighlighted_languages: Vec<String>,
    pub tables: usize,
    /// Diagram fences seen (rendered or degraded).
    pub diagrams: usize,
    /// Image references seen (embedded or degraded).
    pub images: usize,
    /// Content that could not be fully represented — unembedded diagrams and
    /// images. Empty means nothing degraded.
    pub warnings: Vec<String>,
}

pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<ConvertRequest>,
) -> Result<Json<ConvertResponse>, SidecarError> {
    let source = load_source(&req)?;
    let out_path = resolve_out_path(req.out_path.as_deref())?;
    let set_clipboard = req.set_clipboard.unwrap_or(DEFAULT_SET_CLIPBOARD);

    let target = out_path.to_string_lossy().to_string();
    logger::log_request(
        "POST",
        "/gdocs/clipboard",
        "gdocs",
        std::slice::from_ref(&target),
        None,
    );
    let started = Instant::now();
    let activity = state.events.start(
        ActivityKind::Gdocs,
        "gdocs",
        std::slice::from_ref(&target),
        None,
    );
    let result = convert(&source, &out_path, set_clipboard, &req).await;
    logger::log_completion(
        "/gdocs/clipboard",
        Some(if result.is_ok() { 0 } else { 1 }),
        started.elapsed().as_millis(),
    );
    match &result {
        Ok(_) => state.events.finish(activity, Some(0), None),
        Err(e) => state.events.finish(activity, None, Some(e.to_string())),
    }
    result.map(Json)
}

async fn convert(
    source: &str,
    out_path: &Path,
    set_clipboard: bool,
    req: &ConvertRequest,
) -> Result<ConvertResponse, SidecarError> {
    // Relative image paths resolve against the source document's directory;
    // inline `markdown` has no location, so local images stay unresolved.
    let base_dir = req
        .path
        .as_deref()
        .and_then(|p| Path::new(p).parent().map(Path::to_path_buf));

    let rendered = gdocs::convert(
        source,
        &RenderOptions {
            theme: req.theme,
            diagrams: req.diagrams,
            base_dir,
        },
    )
    .await;
    gdocs::write_html(out_path, &rendered.html)?;

    let clipboard_flavor = if set_clipboard {
        Some(gdocs::clipboard::set_html(out_path).await?)
    } else {
        None
    };

    let stats = rendered.stats;
    Ok(ConvertResponse {
        html_path: out_path.to_string_lossy().to_string(),
        html_bytes: rendered.html.len(),
        clipboard_set: clipboard_flavor.is_some(),
        clipboard_flavor,
        code_blocks: stats.code_blocks,
        languages: stats.languages,
        unhighlighted_languages: stats.unhighlighted_languages,
        tables: stats.tables,
        diagrams: stats.diagrams,
        images: stats.images,
        warnings: stats.warnings,
    })
}

/// Resolve the request to Markdown source. Every rejection here happens before
/// any process spawn or file write.
fn load_source(req: &ConvertRequest) -> Result<String, SidecarError> {
    let source = match (&req.path, &req.markdown) {
        (Some(_), Some(_)) => {
            return Err(SidecarError::InvalidRequest(
                "provide either path or markdown, not both".into(),
            ))
        }
        (None, None) => {
            return Err(SidecarError::InvalidRequest(
                "one of path or markdown is required".into(),
            ))
        }
        (Some(path), None) => {
            validate_path(path, "path")?;
            // A missing input file is the caller's mistake, so this is a 400
            // rather than the 500 that a bare io::Error would produce.
            std::fs::read_to_string(path).map_err(|e| {
                SidecarError::InvalidRequest(format!("cannot read markdown file {path}: {e}"))
            })?
        }
        (None, Some(markdown)) => markdown.clone(),
    };

    if source.trim().is_empty() {
        return Err(SidecarError::InvalidRequest(
            "markdown source is empty".into(),
        ));
    }
    if source.len() > MAX_MARKDOWN_BYTES {
        return Err(SidecarError::InvalidRequest(format!(
            "markdown is {} bytes; the limit is {MAX_MARKDOWN_BYTES}",
            source.len()
        )));
    }
    Ok(source)
}

fn resolve_out_path(out_path: Option<&str>) -> Result<PathBuf, SidecarError> {
    match out_path {
        None => Ok(gdocs::default_out_path()),
        Some(path) => {
            validate_path(path, "out_path")?;
            if !path.ends_with(".html") {
                return Err(SidecarError::InvalidRequest(
                    "out_path must end in .html".into(),
                ));
            }
            Ok(PathBuf::from(path))
        }
    }
}

fn validate_path(path: &str, field: &str) -> Result<(), SidecarError> {
    if !path.starts_with('/') {
        return Err(SidecarError::InvalidRequest(format!(
            "{field} must be absolute"
        )));
    }
    // Control characters would corrupt the osascript argv and the log line.
    if path.chars().any(char::is_control) {
        return Err(SidecarError::InvalidRequest(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: Option<&str>, markdown: Option<&str>) -> ConvertRequest {
        ConvertRequest {
            path: path.map(str::to_string),
            markdown: markdown.map(str::to_string),
            set_clipboard: Some(false),
            out_path: None,
            theme: ThemeChoice::default(),
            diagrams: Backend::default(),
        }
    }

    #[test]
    fn requires_exactly_one_input() {
        assert!(load_source(&req(None, None)).is_err());
        assert!(load_source(&req(Some("/a.md"), Some("# x"))).is_err());
        assert_eq!(load_source(&req(None, Some("# x"))).unwrap(), "# x");
    }

    #[test]
    fn rejects_relative_and_control_char_paths() {
        assert!(load_source(&req(Some("relative.md"), None)).is_err());
        assert!(load_source(&req(Some("/tmp/a\nb.md"), None)).is_err());
    }

    #[test]
    fn rejects_empty_markdown() {
        assert!(load_source(&req(None, Some("   \n  "))).is_err());
    }

    /// A missing file is a client error, not a server fault — otherwise a typo
    /// reads as a sidecar bug.
    #[test]
    fn missing_file_is_an_invalid_request_not_an_io_error() {
        let err = load_source(&req(Some("/nonexistent/nope.md"), None)).unwrap_err();
        assert!(matches!(err, SidecarError::InvalidRequest(_)));
    }

    #[test]
    fn out_path_must_be_absolute_html() {
        assert!(resolve_out_path(Some("out.html")).is_err());
        assert!(resolve_out_path(Some("/tmp/out.txt")).is_err());
        assert!(resolve_out_path(Some("/tmp/out.html")).is_ok());
        assert!(resolve_out_path(None).is_ok());
    }

    #[test]
    fn reads_markdown_from_an_absolute_path() {
        let path = std::env::temp_dir().join(format!("gdocs-route-{}.md", std::process::id()));
        std::fs::write(&path, "# from disk\n").unwrap();
        let source = load_source(&req(Some(path.to_str().unwrap()), None)).unwrap();
        assert_eq!(source, "# from disk\n");
        let _ = std::fs::remove_file(&path);
    }
}
