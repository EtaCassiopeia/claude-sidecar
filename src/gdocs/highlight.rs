//! Syntax highlighting as inline `<span style="color:#…">` tokens.
//!
//! Docs won't run CSS classes, so class-based highlighting is useless here —
//! every token needs its color inline. `IncludeBackground::No` is what keeps
//! syntect from also emitting a per-token `background-color`, which would fight
//! the `<pre>` shading and look striped.
//!
//! Grammars come from `two-face` (bat's set) rather than syntect's own defaults,
//! which don't include Scala.

use std::sync::OnceLock;

use syntect::{
    easy::HighlightLines,
    highlighting::{Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem},
    html::{styled_line_to_highlighted_html, IncludeBackground},
    parsing::{SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

use crate::gdocs::style;

/// ~10 MB of grammars; load once per process rather than per request.
static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEMES: OnceLock<EmbeddedLazyThemeSet> = OnceLock::new();
static GDOCS_THEME: OnceLock<Theme> = OnceLock::new();

fn syntaxes() -> &'static SyntaxSet {
    // `extra_newlines` pairs with `LinesWithEndings` below — the newline-
    // inclusive grammars need the line terminator to close line-scoped rules
    // (comments, unterminated strings) correctly.
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn themes() -> &'static EmbeddedLazyThemeSet {
    THEMES.get_or_init(two_face::theme::extra)
}

/// Which color scheme to highlight with. All are light: the paste target is a
/// white Doc page over a `#f5f5f5` code background.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Dark-on-white with strong contrast, and no background of its own.
    #[default]
    Github,
    /// Matches the palette of Google Docs' own "Code blocks" building block, for
    /// documents that mix pasted code with natively-inserted code blocks.
    Gdocs,
    /// Muted pastels; lower contrast over the code background.
    OceanLight,
    /// Tuned for a cream `#fdf6e3` page, so it reads washed out on white.
    SolarizedLight,
}

impl ThemeChoice {
    fn theme(self) -> &'static Theme {
        match self {
            ThemeChoice::Github => themes().get(EmbeddedThemeName::InspiredGithub),
            ThemeChoice::OceanLight => themes().get(EmbeddedThemeName::Base16OceanLight),
            ThemeChoice::SolarizedLight => themes().get(EmbeddedThemeName::SolarizedLight),
            ThemeChoice::Gdocs => GDOCS_THEME.get_or_init(gdocs_theme),
        }
    }
}

fn rgb(hex: u32) -> Color {
    Color {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a: 0xFF,
    }
}

/// The palette Google Docs' "Code blocks" building block uses.
///
/// Docs applies that highlighting client-side after insertion, so the widget
/// itself cannot be produced by a paste — but its colors can be reproduced, which
/// keeps pasted code visually consistent with code blocks inserted natively in
/// the same document. Values were read off a real Docs code block: 9pt Roboto
/// Mono with no background fill.
fn gdocs_theme() -> Theme {
    // Scope selectors are the Sublime/TextMate scope names syntect's grammars
    // emit; the first matching rule wins, so order is least- to most-specific.
    let rules: &[(&str, u32, bool)] = &[
        ("comment", 0x9E9E9E, true),
        ("string, constant.character", 0x188038, false),
        ("constant.numeric, constant.language", 0x1967D2, false),
        ("keyword, storage, keyword.control", 0xB80672, true),
        (
            "storage.type, support.type, entity.name.type",
            0x9334E6,
            false,
        ),
        ("entity.name.function, support.function", 0x1967D2, false),
        (
            "variable.annotation, meta.annotation, entity.other.attribute-name",
            0xC5221F,
            false,
        ),
        ("invalid", 0xC5221F, false),
    ];

    let scopes = rules
        .iter()
        .filter_map(|(selector, color, bold)| {
            // A malformed selector would silently drop one token class rather
            // than break the theme, so skip it and keep the rest.
            let scope = selector.parse::<ScopeSelectors>().ok()?;
            Some(ThemeItem {
                scope,
                style: StyleModifier {
                    foreground: Some(rgb(*color)),
                    background: None,
                    font_style: if *bold { Some(FontStyle::BOLD) } else { None },
                },
            })
        })
        .collect();

    Theme {
        name: Some("Google Docs code block".to_string()),
        author: None,
        settings: syntect::highlighting::ThemeSettings {
            // Anything the rules above don't match falls back to Docs' body slate.
            foreground: Some(rgb(0x37474F)),
            ..Default::default()
        },
        scopes,
    }
}

/// Outcome of highlighting one fence, so the caller can report which languages
/// actually got colorized and which fell back to plain text.
pub enum Highlighted {
    /// Colorized; carries the grammar's display name (e.g. `"Scala"`).
    Colorized { html: String, language: String },
    /// No grammar matched, or highlighting failed — plain shaded `<pre>`.
    Plain { html: String },
}

/// Resolve a fence language token to a grammar.
///
/// `find_syntax_by_token` already matches names, aliases, and extensions
/// case-insensitively — `scala`, `bash`, `sh`, `diff`, `yml` all resolve — so no
/// hand-maintained alias table is needed. Unknown tokens (e.g. `gherkin`) return
/// `None` and take the plain path.
fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let lang = lang.trim();
    if lang.is_empty() {
        return None;
    }
    let set = syntaxes();
    set.find_syntax_by_token(lang)
        .or_else(|| set.find_syntax_by_extension(lang))
}

/// Render one code fence as a shaded `<pre>`, colorized when the language is
/// recognized.
pub fn render_block(lang: &str, source: &str, theme: ThemeChoice) -> Highlighted {
    match find_syntax(lang) {
        Some(syntax) => match colorize(syntax, source, theme.theme()) {
            Some(inner) => Highlighted::Colorized {
                html: wrap_pre(&inner),
                language: syntax.name.clone(),
            },
            // A grammar matched but highlighting failed. Degrading to plain text
            // keeps the content intact; failing the request would lose the whole
            // document over one fence.
            None => Highlighted::Plain {
                html: plain_block(source),
            },
        },
        None => Highlighted::Plain {
            html: plain_block(source),
        },
    }
}

/// Escaped, unhighlighted code in the same shaded `<pre>`.
pub fn plain_block(source: &str) -> String {
    let mut inner = String::with_capacity(source.len() + 16);
    style::escape_text(source, &mut inner);
    wrap_pre(&inner)
}

fn wrap_pre(inner: &str) -> String {
    format!(
        "<pre style=\"{}\">{}</pre>",
        style::PRE,
        inner.trim_end_matches('\n')
    )
}

/// Returns `None` if syntect errors on any line, so the caller can fall back.
fn colorize(syntax: &SyntaxReference, source: &str, theme: &Theme) -> Option<String> {
    let set = syntaxes();
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut out = String::with_capacity(source.len() * 2);
    for line in LinesWithEndings::from(source) {
        let regions = highlighter.highlight_line(line, set).ok()?;
        // `IncludeBackground::No`: the <pre> owns the background. Including it
        // here would emit a per-token background-color and stripe the block.
        out.push_str(&styled_line_to_highlighted_html(&regions, IncludeBackground::No).ok()?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_languages_resolve() {
        for (token, expected) in [
            ("scala", "Scala"),
            ("diff", "Diff"),
            ("yml", "YAML"),
            ("rust", "Rust"),
        ] {
            let syntax = find_syntax(token).unwrap_or_else(|| panic!("{token} did not resolve"));
            assert_eq!(syntax.name, expected);
        }
        // bash's display name is "Bourne Again Shell (bash)"; `sh` aliases to it.
        assert_eq!(
            find_syntax("bash").map(|s| &s.name),
            find_syntax("sh").map(|s| &s.name)
        );
    }

    #[test]
    fn unknown_and_empty_languages_do_not_resolve() {
        // gherkin is not in the bundled set, and one of the real fixture
        // documents contains a gherkin fence — this path is exercised for real.
        assert!(find_syntax("gherkin").is_none());
        assert!(find_syntax("").is_none());
        assert!(find_syntax("   ").is_none());
    }

    #[test]
    fn scala_is_colorized_without_background() {
        let html = match render_block("scala", "case class Foo(a: Int)\n", ThemeChoice::Github) {
            Highlighted::Colorized { html, language } => {
                assert_eq!(language, "Scala");
                html
            }
            Highlighted::Plain { .. } => panic!("scala should colorize"),
        };
        assert!(html.contains("color:#"), "expected inline colors: {html}");
        // A per-token background would fight the <pre> shading.
        assert!(
            !html.contains("background-color"),
            "IncludeBackground::No should suppress backgrounds: {html}"
        );
        assert!(html.starts_with("<pre style="));
    }

    #[test]
    fn unknown_language_falls_back_to_escaped_plain_block() {
        let html = match render_block("gherkin", "Given a <thing> & more\n", ThemeChoice::Github) {
            Highlighted::Plain { html } => html,
            Highlighted::Colorized { .. } => panic!("gherkin has no grammar"),
        };
        assert!(html.contains("&lt;thing&gt;"), "must escape: {html}");
        assert!(html.contains("&amp;"));
        assert!(!html.contains("color:#"));
    }

    /// syntect escapes source text itself, so the renderer must not pre-escape
    /// fence content — doing both yields `&amp;lt;`.
    #[test]
    fn colorized_source_is_escaped_exactly_once() {
        let html = match render_block("scala", "val s = \"<&>\"\n", ThemeChoice::Github) {
            Highlighted::Colorized { html, .. } => html,
            Highlighted::Plain { .. } => panic!("scala should colorize"),
        };
        assert!(html.contains("&lt;"), "got: {html}");
        assert!(!html.contains("&amp;lt;"), "double-escaped: {html}");
    }

    #[test]
    fn all_themes_load_and_colorize() {
        for choice in [
            ThemeChoice::Github,
            ThemeChoice::Gdocs,
            ThemeChoice::OceanLight,
            ThemeChoice::SolarizedLight,
        ] {
            match render_block("scala", "val x = 1\n", choice) {
                Highlighted::Colorized { html, .. } => assert!(html.contains("color:#")),
                Highlighted::Plain { .. } => panic!("{choice:?} failed to colorize"),
            }
        }
    }

    /// The point of this theme is matching Docs' own code-block colors, so the
    /// specific values are the contract — not an implementation detail.
    #[test]
    fn gdocs_theme_uses_the_docs_palette() {
        let html = match render_block(
            "scala",
            "case class A(b: String) // note\nval s = \"txt\"\nval n = 42\n",
            ThemeChoice::Gdocs,
        ) {
            Highlighted::Colorized { html, .. } => html,
            Highlighted::Plain { .. } => panic!("scala should colorize"),
        };
        let lower = html.to_lowercase();
        assert!(lower.contains("#b80672"), "keywords magenta: {html}");
        assert!(lower.contains("#188038"), "strings green: {html}");
        assert!(lower.contains("#1967d2"), "numbers blue: {html}");
        // Docs' code blocks carry no fill of their own.
        assert!(!lower.contains("background-color"));
    }

    /// Unmatched tokens must land on Docs' body slate rather than pure black.
    #[test]
    fn gdocs_theme_defaults_unmatched_text_to_slate() {
        let theme = gdocs_theme();
        let fg = theme.settings.foreground.expect("a default foreground");
        assert_eq!((fg.r, fg.g, fg.b), (0x37, 0x47, 0x4F));
    }

    /// A typo in a scope selector would silently drop that token class; the
    /// filter_map tolerates it, so assert every rule actually parsed.
    #[test]
    fn every_gdocs_scope_selector_parses() {
        assert_eq!(
            gdocs_theme().scopes.len(),
            8,
            "a scope selector failed to parse and was skipped"
        );
    }

    /// Highlighted output lands inside a `style="…"` attribute's sibling text,
    /// but the spans carry their own attributes — none may contain a stray quote
    /// that would break the tag.
    #[test]
    fn highlighted_spans_have_well_formed_style_attributes() {
        let html = match render_block(
            "scala",
            "case class A(b: String) // c\n",
            ThemeChoice::Github,
        ) {
            Highlighted::Colorized { html, .. } => html,
            Highlighted::Plain { .. } => panic!("scala should colorize"),
        };
        for tail in html.split("style=\"").skip(1) {
            let value = tail.split('"').next().expect("style value must be closed");
            assert!(!value.contains('>'), "style value closes the tag: {value}");
        }
    }
}
