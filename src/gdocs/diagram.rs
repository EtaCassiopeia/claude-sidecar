//! Diagram and image rendering for the Docs paste.
//!
//! Docs accepts a base64 data-URI `<img>` on paste (verified empirically), so
//! diagrams can be fully self-contained — nothing needs hosting. It cannot render
//! SVG, so vector sources are rasterized first.
//!
//! Mermaid is rendered by driving headless Chrome over the real `mermaid.js`
//! rather than reimplementing the grammar: that covers every diagram type
//! (flowchart, sequence, gantt, state, class, ER) with nothing for us to keep in
//! sync as Mermaid evolves.
//!
//! Every failure path falls back to a visible code block plus a note. A diagram
//! can be ugly or absent-with-explanation, but it can never silently vanish.

use std::{path::PathBuf, time::Duration};

use base64::Engine as _;
use tokio::{process::Command, time::timeout};

use crate::gdocs::style;

const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

/// Vendored at build time so rendering never depends on a CDN being reachable.
const MERMAID_JS: &str = include_str!("../../assets/mermaid.min.js");

/// Chrome is asked to give up well before this; the outer bound catches a hung
/// process that ignored `--virtual-time-budget`.
const RENDER_TIMEOUT_SECS: u64 = 45;
/// Chrome's in-page budget for loading mermaid.js and laying out the diagram.
const VIRTUAL_TIME_BUDGET_MS: u32 = 10_000;
/// Retina scale: Docs displays the image at CSS size, so 2x keeps text crisp.
const DEVICE_SCALE: u32 = 2;
/// Slack added to the measured box so the last row of nodes isn't clipped.
///
/// The measured `getBoundingClientRect` height excludes the wrapper's own CSS
/// padding and any sub-pixel rounding in the SVG's bottom border, so a window
/// sized to the bare measurement cuts off the final node's edge.
const SIZE_PADDING_PX: u32 = 24;
/// Guards against a pathological diagram producing a giant screenshot.
const MAX_DIMENSION_PX: u32 = 4000;

/// Cap on an embedded image. Docs paste and the clipboard both get unhappy with
/// very large payloads, and base64 inflates by 4/3.
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Emit diagram source as a code block with a note. Default: rendering
    /// spawns Chrome, which is a surprising side effect to opt into silently.
    #[default]
    Off,
    /// Render Mermaid fences and rasterize SVG via headless Chrome.
    Chrome,
}

/// Fence languages treated as diagrams. Only `mermaid` can actually be rendered;
/// the others are recognized so they get an honest note instead of being silently
/// highlighted as if they were code the reader wanted.
const DIAGRAM_LANGUAGES: &[&str] = &["mermaid", "graphviz", "dot"];

pub fn is_diagram(lang: &str) -> bool {
    DIAGRAM_LANGUAGES.contains(&lang.trim().to_ascii_lowercase().as_str())
}

fn is_mermaid(lang: &str) -> bool {
    lang.trim().eq_ignore_ascii_case("mermaid")
}

/// A diagram fence awaiting rendering.
///
/// The renderer is synchronous but Chrome is not, so `render` emits a placeholder
/// for each diagram and the async pass replaces it. This keeps the event-stream
/// renderer pure and independently testable.
#[derive(Debug, Clone)]
pub struct Pending {
    pub placeholder: String,
    pub lang: String,
    pub source: String,
}

/// A unique, HTML-safe token standing in for a diagram until it is rendered.
pub fn placeholder(index: usize) -> String {
    format!("<!--gdocs-diagram-{index}-->")
}

/// Render one Mermaid diagram to an embeddable `<img>`.
///
/// `Err` carries a human-readable reason destined for the fallback note.
pub async fn render_mermaid(source: &str) -> Result<String, String> {
    if !cfg!(target_os = "macos") {
        return Err("diagram rendering requires macOS (it drives Chrome)".into());
    }
    if !std::path::Path::new(CHROME).exists() {
        return Err("Google Chrome is not installed at the expected path".into());
    }

    let dir = scratch_dir().map_err(|e| format!("could not create scratch dir: {e}"))?;
    let stem = uuid::Uuid::new_v4().to_string();
    let js = dir.join("mermaid.min.js");
    // Written once per process; the file is large, so skip the rewrite if present.
    if !js.exists() {
        std::fs::write(&js, MERMAID_JS).map_err(|e| format!("could not stage mermaid.js: {e}"))?;
    }
    let page = dir.join(format!("diagram-{stem}.html"));
    let png = dir.join(format!("diagram-{stem}.png"));
    std::fs::write(&page, diagram_page(source, &js))
        .map_err(|e| format!("could not write diagram page: {e}"))?;

    // Pass 1: let Mermaid lay the diagram out and report its own size, so the
    // screenshot can be cropped to the drawing instead of a mostly-blank window.
    let dom = run_chrome(&[
        "--dump-dom".to_string(),
        format!("file://{}", page.display()),
    ])
    .await?;
    let (w, h) = parse_size(&dom)?;

    // Pass 2: screenshot a window sized to the measured box.
    run_chrome(&[
        "--hide-scrollbars".to_string(),
        format!("--force-device-scale-factor={DEVICE_SCALE}"),
        format!("--window-size={w},{h}"),
        format!("--screenshot={}", png.display()),
        format!("file://{}", page.display()),
    ])
    .await?;

    let bytes = std::fs::read(&png).map_err(|e| format!("Chrome produced no screenshot: {e}"))?;
    // Clean up the intermediates; the caller only needs the data URI.
    let _ = std::fs::remove_file(&page);
    let _ = std::fs::remove_file(&png);

    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "rendered diagram is {} bytes, over the {MAX_IMAGE_BYTES} limit",
            bytes.len()
        ));
    }
    // Display at the CSS size measured in pass 1; the PNG itself is DEVICE_SCALE
    // times larger, which is what makes the text sharp.
    Ok(img_tag(&bytes, "image/png", Some((w, h)), "diagram"))
}

/// Rasterize an SVG file so Docs can display it — Docs cannot render SVG itself.
pub async fn rasterize_svg(path: &std::path::Path) -> Result<String, String> {
    let svg = std::fs::read_to_string(path).map_err(|e| format!("cannot read svg: {e}"))?;
    // An SVG is already a renderable document, so wrap it in the same measuring
    // page and reuse the two-pass path.
    render_mermaid_like(&svg).await
}

/// Shared two-pass render for content that is already SVG markup.
async fn render_mermaid_like(svg_markup: &str) -> Result<String, String> {
    if !std::path::Path::new(CHROME).exists() {
        return Err("Google Chrome is not installed at the expected path".into());
    }
    let dir = scratch_dir().map_err(|e| format!("could not create scratch dir: {e}"))?;
    let stem = uuid::Uuid::new_v4().to_string();
    let page = dir.join(format!("svg-{stem}.html"));
    let png = dir.join(format!("svg-{stem}.png"));
    std::fs::write(&page, svg_page(svg_markup))
        .map_err(|e| format!("could not write svg page: {e}"))?;

    let dom = run_chrome(&[
        "--dump-dom".to_string(),
        format!("file://{}", page.display()),
    ])
    .await?;
    let (w, h) = parse_size(&dom)?;
    run_chrome(&[
        "--hide-scrollbars".to_string(),
        format!("--force-device-scale-factor={DEVICE_SCALE}"),
        format!("--window-size={w},{h}"),
        format!("--screenshot={}", png.display()),
        format!("file://{}", page.display()),
    ])
    .await?;

    let bytes = std::fs::read(&png).map_err(|e| format!("Chrome produced no screenshot: {e}"))?;
    let _ = std::fs::remove_file(&page);
    let _ = std::fs::remove_file(&png);
    Ok(img_tag(&bytes, "image/png", Some((w, h)), "diagram"))
}

/// Embed an already-raster image file as a data URI.
pub fn embed_raster(path: &std::path::Path) -> Result<String, String> {
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some(other) => return Err(format!("unsupported image type .{other}")),
        None => return Err("image has no file extension".into()),
    };
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read image: {e}"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} bytes, over the {MAX_IMAGE_BYTES} limit",
            bytes.len()
        ));
    }
    Ok(img_tag(&bytes, mime, None, "image"))
}

fn img_tag(bytes: &[u8], mime: &str, size: Option<(u32, u32)>, alt: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let dims = match size {
        Some((w, h)) => format!(" width=\"{w}\" height=\"{h}\""),
        None => String::new(),
    };
    format!(
        "<p style=\"{}\"><img src=\"data:{mime};base64,{b64}\"{dims} alt=\"{alt}\"></p>",
        style::P
    )
}

/// Read the size (or error) the page reported.
///
/// The value is extracted from inside the `#gdocs-size` element specifically, not
/// by searching the whole DOM: `--dump-dom` includes the page's own `<script>`
/// source, so a bare search finds the sentinel *in the code that sets it* and
/// mistakes the script text for a result. The script therefore builds the
/// sentinel prefixes from fragments, and this reads only the element's content.
fn parse_size(dom: &str) -> Result<(u32, u32), String> {
    let marker = "id=\"gdocs-size\">";
    let start = dom
        .find(marker)
        .ok_or_else(|| "diagram page did not load (no size element)".to_string())?
        + marker.len();
    let content: String = dom[start..].chars().take_while(|c| *c != '<').collect();
    let content = content.trim();

    if let Some(msg) = content.strip_prefix("GDOCS-ERROR:") {
        return Err(format!("mermaid failed: {}", msg.trim()));
    }
    let spec = content
        .strip_prefix("GDOCS-SIZE:")
        .ok_or_else(|| format!("diagram never reported a size (got {content:?})"))?;
    let (w, h) = spec
        .trim()
        .split_once('x')
        .ok_or_else(|| format!("unparseable size: {spec}"))?;
    let w: u32 = w.trim().parse().map_err(|_| format!("bad width: {w}"))?;
    let h: u32 = h.trim().parse().map_err(|_| format!("bad height: {h}"))?;
    if w == 0 || h == 0 {
        return Err("diagram measured zero-sized".into());
    }
    Ok((
        (w + SIZE_PADDING_PX).min(MAX_DIMENSION_PX),
        (h + SIZE_PADDING_PX).min(MAX_DIMENSION_PX),
    ))
}

async fn run_chrome(args: &[String]) -> Result<String, String> {
    let mut cmd = Command::new(CHROME);
    cmd.args([
        "--headless",
        "--disable-gpu",
        "--no-sandbox",
        "--allow-file-access-from-files",
    ])
    .arg(format!("--virtual-time-budget={VIRTUAL_TIME_BUDGET_MS}"))
    .args(args)
    .kill_on_drop(true);

    let output = timeout(Duration::from_secs(RENDER_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| format!("Chrome timed out after {RENDER_TIMEOUT_SECS}s"))?
        .map_err(|e| format!("could not run Chrome: {e}"))?;

    // Chrome emits benign stderr noise (CVDisplayLink, task_policy_set) even on
    // success, so the exit status is the only signal worth trusting.
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let excerpt: String = err.trim().chars().take(200).collect();
        return Err(format!("Chrome exited non-zero: {excerpt}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn scratch_dir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("claude-sidecar-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The measuring/rendering page. Mermaid may emit a percentage width, so the
/// measured pixel width is pinned onto the SVG before the screenshot pass.
///
/// The `GDOCS-SIZE`/`GDOCS-ERROR` prefixes are concatenated at runtime rather
/// than written literally: `--dump-dom` returns the script source too, and a
/// literal would be found there by any DOM search.
fn diagram_page(source: &str, js: &std::path::Path) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8">
<style>html,body{{margin:0;padding:0;background:#fff}}#wrap{{display:inline-block;padding:4px}}
/* The size is read from the DOM by pass 1; it must never appear in pass 2's
   screenshot, and must not affect layout when measuring. */
#gdocs-size{{position:absolute;left:-9999px;top:0;visibility:hidden}}</style>
<script src="file://{js}"></script></head><body>
<div id="wrap"><pre class="mermaid">{source}</pre></div>
<div id="gdocs-size">pending</div>
<script>
var OK = 'GDOCS' + '-SIZE:', BAD = 'GDOCS' + '-ERROR:';
var el = document.getElementById('gdocs-size');
function fail(m) {{ el.textContent = BAD + String(m).slice(0, 300); }}
mermaid.initialize({{startOnLoad:false, theme:'neutral'}});
mermaid.run({{querySelector:'.mermaid'}}).then(function() {{
  var svg = document.querySelector('#wrap svg');
  if (!svg) {{ fail('no svg produced'); return; }}
  var r = svg.getBoundingClientRect();
  svg.setAttribute('width', Math.ceil(r.width));
  svg.style.maxWidth = 'none';
  // Measure the wrapper's outer edges, so its padding and the SVG's own
  // borders are inside the reported box rather than pushed out of frame.
  var wrap = document.getElementById('wrap').getBoundingClientRect();
  el.textContent = OK + Math.ceil(wrap.right) + 'x' + Math.ceil(wrap.bottom);
}}).catch(function(e) {{ fail(e && e.message ? e.message : e); }});
</script></body></html>"#,
        js = js.display(),
        source = escape_for_html_text(source),
    )
}

fn svg_page(svg_markup: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8">
<style>html,body{{margin:0;padding:0;background:#fff}}#wrap{{display:inline-block;padding:4px}}
#gdocs-size{{position:absolute;left:-9999px;top:0;visibility:hidden}}</style>
</head><body><div id="wrap">{svg_markup}</div>
<div id="gdocs-size">pending</div>
<script>
(function() {{
  var OK = 'GDOCS' + '-SIZE:', BAD = 'GDOCS' + '-ERROR:';
  var el = document.getElementById('gdocs-size');
  var svg = document.querySelector('#wrap svg');
  if (!svg) {{ el.textContent = BAD + 'file contains no svg element'; return; }}
  var r = svg.getBoundingClientRect();
  if (!r.width || !r.height) {{ el.textContent = BAD + 'svg has no intrinsic size'; return; }}
  var wrap = document.getElementById('wrap').getBoundingClientRect();
  el.textContent = OK + Math.ceil(wrap.right) + 'x' + Math.ceil(wrap.bottom);
}})();
</script></body></html>"#
    )
}

/// Escape diagram source for embedding in `<pre>` text. Mermaid syntax is full of
/// `-->` and `&`, which would otherwise be parsed as markup.
fn escape_for_html_text(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 16);
    style::escape_text(source, &mut out);
    out
}

/// The visible note that accompanies an unrendered diagram, so the reader knows
/// a figure belongs here rather than a code listing.
pub fn note(lang: &str, reason: &str) -> String {
    let mut out = format!("<p style=\"{}\">[", style::NOTE);
    style::escape_text(lang, &mut out);
    out.push_str(" diagram not embedded: ");
    style::escape_text(reason, &mut out);
    out.push_str(" — insert it manually]</p>");
    out
}

/// Whether this fence should be handed to the async render pass.
pub fn should_render(backend: Backend, lang: &str) -> bool {
    backend == Backend::Chrome && is_mermaid(lang)
}

/// Why a recognized diagram is not being rendered, for the fallback note.
pub fn unsupported_reason(backend: Backend, lang: &str) -> &'static str {
    match backend {
        Backend::Off => "diagram rendering is off (pass \"diagrams\":\"chrome\" to enable)",
        Backend::Chrome if !is_mermaid(lang) => "only mermaid diagrams can be rendered",
        Backend::Chrome => "rendering failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_diagram_languages() {
        assert!(is_diagram("mermaid"));
        assert!(is_diagram("Mermaid"));
        assert!(is_diagram("dot"));
        assert!(!is_diagram("scala"));
        assert!(!is_diagram(""));
    }

    #[test]
    fn only_mermaid_renders_and_only_when_enabled() {
        assert!(should_render(Backend::Chrome, "mermaid"));
        assert!(should_render(Backend::Chrome, "MERMAID"));
        // Recognized as a diagram but has no renderer.
        assert!(!should_render(Backend::Chrome, "graphviz"));
        // Off is the default, so nothing spawns Chrome unless asked.
        assert!(!should_render(Backend::Off, "mermaid"));
        assert!(!should_render(Backend::Chrome, "scala"));
    }

    #[test]
    fn off_backend_explains_how_to_enable() {
        assert!(unsupported_reason(Backend::Off, "mermaid").contains("chrome"));
        assert!(unsupported_reason(Backend::Chrome, "dot").contains("only mermaid"));
    }

    #[test]
    fn parses_a_reported_size_with_padding() {
        let (w, h) = parse_size("<div id=\"gdocs-size\">GDOCS-SIZE:300x441</div>").unwrap();
        assert_eq!((w, h), (300 + SIZE_PADDING_PX, 441 + SIZE_PADDING_PX));
    }

    #[test]
    fn surfaces_a_mermaid_syntax_error() {
        let err = parse_size("<div id=\"gdocs-size\">GDOCS-ERROR:Parse error on line 2</div>")
            .unwrap_err();
        assert!(err.contains("Parse error"), "got: {err}");
    }

    /// Regression: `--dump-dom` returns the page's own `<script>` source, so a
    /// naive whole-DOM search finds the sentinel in the code that *sets* it and
    /// reports a failure for a diagram that rendered fine. Only the element's
    /// content counts.
    #[test]
    fn ignores_sentinels_appearing_in_the_page_script() {
        let dom = r#"<html><body>
<div id="wrap"><svg>...</svg></div>
<div id="gdocs-size">GDOCS-SIZE:300x441</div>
<script>
var OK = 'GDOCS-SIZE:', BAD = 'GDOCS-ERROR:';
function fail(m) { el.textContent = BAD + m; }
</script></body></html>"#;
        let (w, h) = parse_size(dom).expect("script text must not be mistaken for a result");
        assert_eq!((w, h), (300 + SIZE_PADDING_PX, 441 + SIZE_PADDING_PX));
    }

    /// The generated page must not contain the literal sentinels outside the
    /// reporting element, or the bug above returns.
    #[test]
    fn generated_page_has_no_literal_sentinel_in_its_script() {
        let page = diagram_page("graph TD\n A-->B", std::path::Path::new("/tmp/m.js"));
        assert!(
            !page.contains("'GDOCS-SIZE:"),
            "literal sentinel in script would self-match"
        );
        assert!(!page.contains("'GDOCS-ERROR:"));
        assert!(page.contains("id=\"gdocs-size\""));
    }

    /// The reporting element shares the page with the diagram, so if it is
    /// visible it lands in the screenshot as stray text under the figure.
    #[test]
    fn size_element_is_hidden_from_the_screenshot() {
        for page in [
            diagram_page("graph TD\n A-->B", std::path::Path::new("/tmp/m.js")),
            svg_page("<svg width='10' height='10'/>"),
        ] {
            assert!(
                page.contains("#gdocs-size{position:absolute;left:-9999px"),
                "the size element must be positioned off-canvas"
            );
            assert!(page.contains("visibility:hidden"));
        }
    }

    #[test]
    fn missing_size_is_an_error_not_a_panic() {
        assert!(parse_size("<html>nothing here</html>").is_err());
        assert!(parse_size("<div id=\"gdocs-size\">pending</div>").is_err());
        assert!(parse_size("<div id=\"gdocs-size\">GDOCS-SIZE:not-a-size</div>").is_err());
        assert!(parse_size("<div id=\"gdocs-size\">GDOCS-SIZE:0x0</div>").is_err());
    }

    #[test]
    fn oversized_diagrams_are_clamped() {
        let (w, h) = parse_size("<div id=\"gdocs-size\">GDOCS-SIZE:99999x99999</div>").unwrap();
        assert_eq!((w, h), (MAX_DIMENSION_PX, MAX_DIMENSION_PX));
    }

    /// Mermaid arrows would be read as markup if the source were not escaped.
    #[test]
    fn diagram_source_is_escaped_into_the_page() {
        let page = diagram_page("graph TD\n A-->B & C", std::path::Path::new("/tmp/m.js"));
        assert!(page.contains("A--&gt;B &amp; C"), "got: {page}");
        assert!(!page.contains("A-->B &"), "raw source leaked into the page");
    }

    #[test]
    fn placeholders_are_unique_and_comment_shaped() {
        assert_ne!(placeholder(0), placeholder(1));
        assert!(placeholder(3).starts_with("<!--"));
    }

    #[test]
    fn embeds_a_png_as_a_data_uri() {
        let dir = std::env::temp_dir().join(format!("gdocs-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nfake").unwrap();
        let html = embed_raster(&path).unwrap();
        assert!(html.contains("data:image/png;base64,"), "got: {html}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_unsupported_and_extensionless_images() {
        let dir = std::env::temp_dir();
        assert!(embed_raster(&dir.join("x.tiff")).is_err());
        assert!(embed_raster(&dir.join("noext")).is_err());
    }

    #[test]
    fn img_tag_has_well_formed_attributes() {
        let html = img_tag(b"abc", "image/png", Some((10, 20)), "diagram");
        assert!(html.contains("width=\"10\" height=\"20\""));
        for tail in html.split("style=\"").skip(1) {
            let value = tail.split('"').next().unwrap();
            assert!(!value.contains('>'), "style closes the tag: {value}");
        }
    }
}
