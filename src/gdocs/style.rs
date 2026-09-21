//! Inline style table for Google Docs paste.
//!
//! Docs ignores `<style>` blocks entirely, so every rule has to travel as a
//! `style=` attribute on the element itself. These values were tuned against
//! real Docs pastes — Docs largely ignores semantic defaults, so sizes and
//! weights are set explicitly per element rather than inherited from `<h2>`.
//!
//! One hard constraint: **no double quote may appear inside a style value.**
//! `font-family:Consolas,"Courier New",monospace` written into `style="…"`
//! terminates the attribute early and mangles the tag. Use bare
//! `Consolas,monospace`. `styles_are_quote_free` enforces this.

use pulldown_cmark::HeadingLevel;

pub const BODY: &str = "font-family:Arial,sans-serif;font-size:11pt;line-height:1.45;color:#000";

pub const H1: &str = "font-size:20pt;font-weight:bold;margin:16pt 0 6pt";
pub const H2: &str = "font-size:15pt;font-weight:bold;margin:15pt 0 5pt";
pub const H3: &str = "font-size:12.5pt;font-weight:bold;margin:13pt 0 4pt";
pub const H4: &str = "font-size:11.5pt;font-weight:bold;margin:11pt 0 4pt";
pub const H5: &str = "font-size:11pt;font-weight:bold;margin:10pt 0 3pt";
pub const H6: &str = "font-size:10.5pt;font-weight:bold;margin:10pt 0 3pt;color:#333";

pub const P: &str = "margin:6pt 0";

/// Code blocks carry the shading. The fence path emits `<pre>` with no nested
/// `<code>`, so the double-background problem can't arise.
pub const PRE: &str = "font-family:Consolas,monospace;font-size:9.5pt;background:#f5f5f5;\
border:1px solid #ddd;padding:8px;white-space:pre-wrap;margin:8pt 0";

/// Inline code is never inside a `<pre>`, so it carries its own shading.
pub const CODE: &str = "font-family:Consolas,monospace;font-size:10pt;background:#f5f5f5";

/// `border-collapse` on the table *plus* explicit per-cell borders — either one
/// alone pastes borderless.
pub const TABLE: &str = "border-collapse:collapse;margin:8pt 0";
pub const TH: &str = "border:1px solid #999;padding:5px 8px;text-align:left;\
vertical-align:top;background:#efefef;font-weight:bold";
pub const TD: &str = "border:1px solid #999;padding:5px 8px;text-align:left;vertical-align:top";

pub const BLOCKQUOTE: &str =
    "border-left:3px solid #999;margin:8pt 0 8pt 12pt;padding-left:10pt;color:#333";

pub const LIST: &str = "margin:4pt 0;padding-left:24pt";
pub const LI: &str = "margin:2pt 0";

pub const HR: &str = "border:none;border-top:1px solid #ccc;margin:12pt 0";

pub const LINK: &str = "color:#1155cc;text-decoration:underline";

/// Shown next to a diagram fence that could not be rendered to an image.
pub const NOTE: &str = "font-size:9.5pt;font-style:italic;color:#666";

pub fn heading(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => H1,
        HeadingLevel::H2 => H2,
        HeadingLevel::H3 => H3,
        HeadingLevel::H4 => H4,
        HeadingLevel::H5 => H5,
        HeadingLevel::H6 => H6,
    }
}

/// Append `text` to `out` with HTML metacharacters escaped.
///
/// pulldown-cmark hands us **unescaped** text — `5 > 3 & x` arrives verbatim —
/// so escaping is our job. Note this must NOT be applied to code-fence content:
/// syntect escapes that itself, and escaping twice renders `&amp;lt;`.
pub fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Escape a URL for an `href="…"` attribute. Same escapes as text — the point
/// is keeping `"` and `<` from breaking out of the attribute.
pub fn escape_attr(value: &str, out: &mut String) {
    escape_text(value, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every style value that can reach a `style="…"` attribute. The prior
    /// implementation of this converter shipped a quote inside `font-family`
    /// and silently mangled every code block; this test is why that can't
    /// recur.
    const ALL_STYLES: &[&str] = &[
        BODY, H1, H2, H3, H4, H5, H6, P, PRE, CODE, TABLE, TH, TD, BLOCKQUOTE, LIST, LI, HR, LINK,
        NOTE,
    ];

    #[test]
    fn styles_are_quote_free() {
        for style in ALL_STYLES {
            assert!(
                !style.contains('"'),
                "style value contains a double quote, which terminates the attribute: {style}"
            );
        }
    }

    /// A `>` inside a style value would close the tag early.
    #[test]
    fn styles_have_no_angle_brackets() {
        for style in ALL_STYLES {
            assert!(
                !style.contains('<') && !style.contains('>'),
                "bad style: {style}"
            );
        }
    }

    #[test]
    fn escapes_html_metacharacters() {
        let mut out = String::new();
        escape_text(r#"5 > 3 & "x" < y"#, &mut out);
        assert_eq!(out, "5 &gt; 3 &amp; &quot;x&quot; &lt; y");
    }

    /// Already-escaped input escapes again — correct, since the input is literal
    /// text. A doc containing `&lt;` should paste as the visible text `&lt;`.
    #[test]
    fn escaping_is_not_idempotent_by_design() {
        let mut out = String::new();
        escape_text("&lt;", &mut out);
        assert_eq!(out, "&amp;lt;");
    }

    #[test]
    fn every_heading_level_has_a_style() {
        for level in [
            HeadingLevel::H1,
            HeadingLevel::H2,
            HeadingLevel::H3,
            HeadingLevel::H4,
            HeadingLevel::H5,
            HeadingLevel::H6,
        ] {
            assert!(heading(level).contains("font-size"));
        }
    }
}
