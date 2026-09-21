//! Markdown → Google-Docs-pasteable HTML.
//!
//! Pasting Markdown into Docs yields literal `##` and `**`; pasting HTML
//! preserves structure. Two constraints shape everything here, both established
//! empirically:
//!
//! 1. **Docs ignores `<style>` blocks.** Every rule must be an inline `style=`
//!    attribute, which is why highlighting is per-token `<span style="color:…">`
//!    rather than CSS classes.
//! 2. **The clipboard must carry the `«class HTML»` flavor.** `pbcopy` sets plain
//!    text, and Docs then pastes visible tags.
//!
//! [`render`] is pure and spawns nothing, so conversion is testable without
//! touching the clipboard; [`clipboard`] is the only part that runs a process.

pub mod clipboard;
pub mod diagram;
pub mod drive;
pub mod export;
pub mod highlight;
pub mod render;
pub mod style;

use std::path::{Path, PathBuf};

pub use diagram::Backend;
pub use highlight::ThemeChoice;
pub use render::{render, RenderOptions, Rendered, Stats};

use crate::error::SidecarError;

/// Render Markdown and resolve any deferred diagrams.
///
/// [`render`] is synchronous and pure; diagram rendering needs Chrome. This joins
/// the two: each placeholder is replaced by an `<img>`, or — on any failure — by
/// the diagram's source as a highlighted code block plus a visible note. A
/// diagram never silently disappears.
pub async fn convert(markdown: &str, opts: &RenderOptions) -> Rendered {
    let mut rendered = render(markdown, opts);
    if rendered.pending.is_empty() {
        return rendered;
    }

    let pending = std::mem::take(&mut rendered.pending);
    for item in pending {
        let result = if item.lang == "svg" {
            // `source` holds the resolved path for svg, not markup.
            diagram::rasterize_svg(Path::new(&item.source)).await
        } else {
            diagram::render_mermaid(&item.source).await
        };

        let replacement = match result {
            Ok(html) => html,
            Err(reason) => {
                rendered
                    .stats
                    .warnings
                    .push(format!("{} diagram not embedded: {reason}", item.lang));
                let fallback = match item.lang.as_str() {
                    // An svg failure has no source worth showing as code.
                    "svg" => String::new(),
                    _ => match highlight::render_block(&item.lang, &item.source, opts.theme) {
                        highlight::Highlighted::Colorized { html, .. }
                        | highlight::Highlighted::Plain { html } => html,
                    },
                };
                format!("{fallback}{}", diagram::note(&item.lang, &reason))
            }
        };
        rendered.html = rendered.html.replace(&item.placeholder, &replacement);
    }
    rendered
}

/// Write `html` to `path`, creating the parent directory when it's ours.
pub fn write_html(path: &Path, html: &str) -> Result<(), SidecarError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, html)?;
    Ok(())
}

/// A generated path for the HTML output.
///
/// The file is deliberately kept after the clipboard is set: clipboards get
/// overwritten, and having the file on disk is what makes the output greppable
/// instead of only verifiable by pasting.
pub fn default_out_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("claude-sidecar-{}", std::process::id()))
        .join(format!("gdocs-{}.html", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_paths_are_unique_and_html() {
        let a = default_out_path();
        let b = default_out_path();
        assert_ne!(a, b);
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("html"));
        assert!(a.parent() == b.parent(), "same per-process directory");
    }

    #[test]
    fn writes_html_creating_parent_directories() {
        let path = std::env::temp_dir()
            .join(format!("gdocs-test-{}", std::process::id()))
            .join("nested")
            .join("out.html");
        write_html(&path, "<div>x</div>").expect("write should succeed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "<div>x</div>");
        let _ = std::fs::remove_file(&path);
    }
}
