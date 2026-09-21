//! Markdown → inline-styled HTML, emitted straight from the event stream.
//!
//! Styles are written as each tag is opened rather than injected into finished
//! HTML afterward. That ordering is the point: a regex post-pass over generated
//! HTML is what produced the two bugs this converter replaces (a quote inside a
//! style value truncating the attribute, and `<hr />` becoming
//! `<hr / style="…">`). Emitting at generation time makes both unrepresentable —
//! there is no second pass to get wrong.

use std::fmt::Write as _;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::gdocs::{
    diagram::{self, Backend, Pending},
    highlight::{self, Highlighted, ThemeChoice},
    style,
};

#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    pub theme: ThemeChoice,
    pub diagrams: Backend,
    /// Directory that relative image paths resolve against — the source
    /// document's own directory. `None` disables local image embedding, which is
    /// what inline `markdown` input gets since it has no location on disk.
    pub base_dir: Option<std::path::PathBuf>,
}

/// What the conversion produced, including enough detail for the caller to
/// verify the result by inspection rather than trusting a success message.
#[derive(Debug, Default)]
pub struct Stats {
    pub code_blocks: usize,
    /// Grammar display names actually applied, deduped, in first-seen order.
    pub languages: Vec<String>,
    /// Fence tokens that had no grammar and fell back to plain text.
    pub unhighlighted_languages: Vec<String>,
    pub tables: usize,
    pub diagrams: usize,
    pub images: usize,
    pub warnings: Vec<String>,
}

impl Stats {
    fn note_language(&mut self, name: String) {
        if !self.languages.contains(&name) {
            self.languages.push(name);
        }
    }

    fn note_unhighlighted(&mut self, token: &str) {
        // An unlabeled fence isn't a missing grammar, so it isn't reportable.
        if token.is_empty() || self.unhighlighted_languages.iter().any(|t| t == token) {
            return;
        }
        self.unhighlighted_languages.push(token.to_string());
    }
}

pub struct Rendered {
    pub html: String,
    pub stats: Stats,
    /// Diagrams awaiting the async render pass, in document order. Empty unless
    /// `diagrams` is enabled — see [`crate::gdocs::finish`].
    pub pending: Vec<Pending>,
}

/// Accumulator for a code fence. Text events inside a fence are buffered raw —
/// syntect escapes them later, and escaping twice yields `&amp;lt;`.
struct CodeCtx {
    lang: String,
    buf: String,
}

pub fn render(markdown: &str, opts: &RenderOptions) -> Rendered {
    // Smart punctuation is deliberately off: curly quotes read nicely in prose
    // but would corrupt any code that leaked through the inline path.
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES,
    );

    let mut r = Renderer {
        out: String::with_capacity(markdown.len() * 3),
        aligns: Vec::new(),
        col: 0,
        in_head: false,
        code: None,
        stats: Stats::default(),
        opts: opts.clone(),
        pending: Vec::new(),
        base_dir: opts.base_dir.clone(),
    };

    let _ = write!(r.out, "<div style=\"{}\">", style::BODY);
    for event in parser {
        r.event(event);
    }
    r.out.push_str("</div>");

    Rendered {
        html: r.out,
        stats: r.stats,
        pending: r.pending,
    }
}

struct Renderer {
    out: String,
    /// Column alignments for the table currently being emitted.
    aligns: Vec<Alignment>,
    col: usize,
    in_head: bool,
    code: Option<CodeCtx>,
    stats: Stats,
    opts: RenderOptions,
    /// Diagrams deferred to the async pass, in document order.
    pending: Vec<Pending>,
    base_dir: Option<std::path::PathBuf>,
}

impl Renderer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),

            Event::Text(text) => match &mut self.code {
                Some(ctx) => ctx.buf.push_str(&text),
                None => style::escape_text(&text, &mut self.out),
            },

            Event::Code(code) => {
                let _ = write!(self.out, "<code style=\"{}\">", style::CODE);
                style::escape_text(&code, &mut self.out);
                self.out.push_str("</code>");
            }

            // Raw HTML passes through unstyled. Dropping it would lose content
            // silently; passing it through at least shows the author something
            // is there, even though Docs will paste it mostly unformatted.
            Event::Html(html) | Event::InlineHtml(html) => self.out.push_str(&html),

            Event::SoftBreak => self.out.push(' '),
            Event::HardBreak => self.out.push_str("<br>"),

            // Not self-closing: `<hr />` is what a regex post-pass mangles into
            // `<hr / style="…">`.
            Event::Rule => {
                let _ = write!(self.out, "<hr style=\"{}\">", style::HR);
            }

            // Docs won't render <input type=checkbox>, so use literal glyphs.
            Event::TaskListMarker(checked) => {
                self.out.push_str(if checked { "☑ " } else { "☐ " });
            }

            Event::FootnoteReference(label) => {
                self.out.push_str("<sup>");
                style::escape_text(&label, &mut self.out);
                self.out.push_str("</sup>");
            }

            // Math needs ENABLE_MATH, which we don't set, so these can't occur.
            // Handled explicitly (rather than via a catch-all) so that enabling
            // the option later is a visible decision instead of silent dropping.
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let _ = write!(self.out, "<code style=\"{}\">", style::CODE);
                style::escape_text(&text, &mut self.out);
                self.out.push_str("</code>");
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                let _ = write!(self.out, "<p style=\"{}\">", style::P);
            }
            Tag::Heading { level, .. } => {
                let _ = write!(
                    self.out,
                    "<{} style=\"{}\">",
                    heading_tag(level),
                    style::heading(level)
                );
            }
            Tag::BlockQuote(_) => {
                let _ = write!(self.out, "<blockquote style=\"{}\">", style::BLOCKQUOTE);
            }
            Tag::CodeBlock(kind) => {
                let lang = match &kind {
                    // The info string can carry attributes (` ```scala title=x `);
                    // only the first token is the language.
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeCtx {
                    lang,
                    buf: String::new(),
                });
            }
            Tag::List(Some(start)) => {
                let _ = write!(self.out, "<ol style=\"{}\"", style::LIST);
                if start != 1 {
                    let _ = write!(self.out, " start=\"{start}\"");
                }
                self.out.push('>');
            }
            Tag::List(None) => {
                let _ = write!(self.out, "<ul style=\"{}\">", style::LIST);
            }
            Tag::Item => {
                let _ = write!(self.out, "<li style=\"{}\">", style::LI);
            }
            Tag::Table(aligns) => {
                self.stats.tables += 1;
                self.aligns = aligns;
                let _ = write!(self.out, "<table style=\"{}\">", style::TABLE);
            }
            Tag::TableHead => {
                self.in_head = true;
                self.col = 0;
                self.out.push_str("<tr>");
            }
            Tag::TableRow => {
                self.col = 0;
                self.out.push_str("<tr>");
            }
            Tag::TableCell => {
                let base = if self.in_head { style::TH } else { style::TD };
                let name = if self.in_head { "th" } else { "td" };
                let _ = write!(self.out, "<{name} style=\"{base}");
                // The base style ends with `text-align:left`; a later declaration
                // in the same attribute wins, so appending is enough — no need to
                // rewrite the base string.
                match self.aligns.get(self.col) {
                    Some(Alignment::Center) => self.out.push_str(";text-align:center"),
                    Some(Alignment::Right) => self.out.push_str(";text-align:right"),
                    Some(Alignment::Left) | Some(Alignment::None) | None => {}
                }
                self.out.push_str("\">");
            }
            Tag::Emphasis => self.out.push_str("<em>"),
            Tag::Strong => self.out.push_str("<strong>"),
            Tag::Strikethrough => self.out.push_str("<s>"),
            Tag::Superscript => self.out.push_str("<sup>"),
            Tag::Subscript => self.out.push_str("<sub>"),
            Tag::Link { dest_url, .. } => {
                self.out.push_str("<a href=\"");
                style::escape_attr(&dest_url, &mut self.out);
                let _ = write!(self.out, "\" style=\"{}\">", style::LINK);
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                self.stats.images += 1;
                self.image(&dest_url, &title);
            }
            Tag::HtmlBlock => {}
            Tag::FootnoteDefinition(label) => {
                let _ = write!(self.out, "<p style=\"{}\"><sup>", style::P);
                style::escape_text(&label, &mut self.out);
                self.out.push_str("</sup> ");
            }
            Tag::DefinitionList => {
                let _ = write!(self.out, "<dl style=\"{}\">", style::LIST);
            }
            Tag::DefinitionListTitle => {
                let _ = write!(self.out, "<dt style=\"{}\"><strong>", style::LI);
            }
            Tag::DefinitionListDefinition => {
                let _ = write!(self.out, "<dd style=\"{}\">", style::LI);
            }
            // Frontmatter. Buffered as a code fence so the YAML doesn't land in
            // the document as loose prose; `end` drops it.
            Tag::MetadataBlock(_) => {
                self.code = Some(CodeCtx {
                    lang: String::new(),
                    buf: String::new(),
                });
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.out.push_str("</p>"),
            TagEnd::Heading(level) => {
                let _ = write!(self.out, "</{}>", heading_tag(level));
            }
            TagEnd::BlockQuote(_) => self.out.push_str("</blockquote>"),
            TagEnd::CodeBlock => self.flush_code(),
            TagEnd::List(true) => self.out.push_str("</ol>"),
            TagEnd::List(false) => self.out.push_str("</ul>"),
            TagEnd::Item => self.out.push_str("</li>"),
            TagEnd::Table => {
                self.out.push_str("</table>");
                self.aligns.clear();
            }
            TagEnd::TableHead => {
                self.out.push_str("</tr>");
                self.in_head = false;
            }
            TagEnd::TableRow => self.out.push_str("</tr>"),
            TagEnd::TableCell => {
                self.out
                    .push_str(if self.in_head { "</th>" } else { "</td>" });
                self.col += 1;
            }
            TagEnd::Emphasis => self.out.push_str("</em>"),
            TagEnd::Strong => self.out.push_str("</strong>"),
            TagEnd::Strikethrough => self.out.push_str("</s>"),
            TagEnd::Superscript => self.out.push_str("</sup>"),
            TagEnd::Subscript => self.out.push_str("</sub>"),
            TagEnd::Link => self.out.push_str("</a>"),
            TagEnd::Image => {}
            TagEnd::HtmlBlock => {}
            TagEnd::FootnoteDefinition => self.out.push_str("</p>"),
            TagEnd::DefinitionList => self.out.push_str("</dl>"),
            TagEnd::DefinitionListTitle => self.out.push_str("</strong></dt>"),
            TagEnd::DefinitionListDefinition => self.out.push_str("</dd>"),
            // Frontmatter is metadata, not content — discard the buffer.
            TagEnd::MetadataBlock(_) => {
                self.code = None;
            }
        }
    }

    /// Emit the buffered fence: diagram seam first, then highlighting.
    fn flush_code(&mut self) {
        let Some(ctx) = self.code.take() else {
            return;
        };
        self.stats.code_blocks += 1;

        if !diagram::is_diagram(&ctx.lang) {
            self.emit_code(&ctx);
            return;
        }
        self.stats.diagrams += 1;

        // Rendering needs Chrome, which is async; leave a placeholder for the
        // async pass to substitute. Keeping this function pure is what lets the
        // whole renderer be unit-tested without spawning anything.
        if diagram::should_render(self.opts.diagrams, &ctx.lang) {
            let placeholder = diagram::placeholder(self.pending.len());
            self.out.push_str(&placeholder);
            self.pending.push(Pending {
                placeholder,
                lang: ctx.lang.clone(),
                source: ctx.buf.clone(),
            });
            return;
        }

        // Not rendering: show the source and say why, so the diagram is never
        // silently missing.
        let reason = diagram::unsupported_reason(self.opts.diagrams, &ctx.lang);
        self.emit_code(&ctx);
        self.out.push_str(&diagram::note(&ctx.lang, reason));
        self.stats
            .warnings
            .push(format!("{} diagram not embedded: {reason}", ctx.lang));
    }

    fn emit_code(&mut self, ctx: &CodeCtx) {
        match highlight::render_block(&ctx.lang, &ctx.buf, self.opts.theme) {
            Highlighted::Colorized { html, language } => {
                self.out.push_str(&html);
                self.stats.note_language(language);
            }
            Highlighted::Plain { html } => {
                self.out.push_str(&html);
                self.stats.note_unhighlighted(&ctx.lang);
            }
        }
    }

    /// Embed a local image as a data URI, or degrade to a visible label.
    ///
    /// Remote URLs are deliberately not fetched: that would make conversion
    /// depend on the network and could leak a document's references to third
    /// parties. SVG is deferred to the async pass, since Docs cannot render it
    /// and it has to be rasterized first.
    fn image(&mut self, dest_url: &str, title: &str) {
        let label = if title.is_empty() { dest_url } else { title };

        let Some(path) = self.resolve_local(dest_url) else {
            self.degrade_image(
                label,
                format!("image not embedded (not a local file): {dest_url}"),
            );
            return;
        };
        if !path.exists() {
            self.degrade_image(label, format!("image not found: {}", path.display()));
            return;
        }

        let is_svg = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
        if is_svg {
            // Needs Chrome, so it goes through the same deferral as diagrams.
            if self.opts.diagrams == Backend::Chrome {
                let placeholder = diagram::placeholder(self.pending.len());
                self.out.push_str(&placeholder);
                self.pending.push(Pending {
                    placeholder,
                    lang: "svg".to_string(),
                    source: path.to_string_lossy().to_string(),
                });
            } else {
                self.degrade_image(
                    label,
                    format!(
                        "svg not embedded (needs \"diagrams\":\"chrome\" to rasterize): {dest_url}"
                    ),
                );
            }
            return;
        }

        match diagram::embed_raster(&path) {
            Ok(html) => self.out.push_str(&html),
            Err(reason) => self.degrade_image(label, format!("image not embedded: {reason}")),
        }
    }

    /// Resolve a Markdown image target to a readable local path, or `None` if it
    /// is remote or cannot be located.
    fn resolve_local(&self, dest_url: &str) -> Option<std::path::PathBuf> {
        if dest_url.contains("://") || dest_url.starts_with("data:") {
            return None;
        }
        let raw = dest_url.strip_prefix("file://").unwrap_or(dest_url);
        let path = std::path::Path::new(raw);
        if path.is_absolute() {
            return Some(path.to_path_buf());
        }
        // Relative paths only mean something if we know where the document lives.
        self.base_dir.as_ref().map(|base| base.join(path))
    }

    fn degrade_image(&mut self, label: &str, warning: String) {
        self.stats.warnings.push(warning);
        let _ = write!(self.out, "<span style=\"{}\">[image: ", style::NOTE);
        style::escape_text(label, &mut self.out);
        self.out.push_str("]</span>");
    }
}

fn heading_tag(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "h1",
        HeadingLevel::H2 => "h2",
        HeadingLevel::H3 => "h3",
        HeadingLevel::H4 => "h4",
        HeadingLevel::H5 => "h5",
        HeadingLevel::H6 => "h6",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html_of(md: &str) -> String {
        render(md, &RenderOptions::default()).html
    }

    #[test]
    fn wraps_in_a_styled_div() {
        let html = html_of("hi");
        assert!(html.starts_with(&format!("<div style=\"{}\">", style::BODY)));
        assert!(html.ends_with("</div>"));
    }

    #[test]
    fn headings_carry_explicit_sizes() {
        let html = html_of("# one\n\n## two\n\n###### six\n");
        assert!(html.contains(&format!("<h1 style=\"{}\">", style::H1)));
        assert!(html.contains(&format!("<h2 style=\"{}\">", style::H2)));
        assert!(html.contains(&format!("<h6 style=\"{}\">", style::H6)));
    }

    /// The `<hr / style="…">` bug from the previous implementation.
    #[test]
    fn hr_is_not_self_closing() {
        let html = html_of("a\n\n---\n\nb\n");
        assert!(html.contains(&format!("<hr style=\"{}\">", style::HR)));
        assert!(!html.contains("<hr /"), "got: {html}");
        assert!(!html.contains("/ style="), "got: {html}");
    }

    #[test]
    fn tables_get_collapsed_borders_and_header_cells() {
        let html = html_of("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains(&format!("<table style=\"{}\">", style::TABLE)));
        assert!(html.contains("<th style="));
        assert!(html.contains("<td style="));
        assert!(html.contains("border-collapse:collapse"));
        assert_eq!(
            render("| a |\n|---|\n| 1 |\n", &RenderOptions::default())
                .stats
                .tables,
            1
        );
    }

    #[test]
    fn table_alignment_appends_text_align() {
        let html = html_of("| l | c | r |\n|:--|:-:|--:|\n| 1 | 2 | 3 |\n");
        assert!(html.contains("text-align:center"), "got: {html}");
        assert!(html.contains("text-align:right"), "got: {html}");
        // Appended after the base `text-align:left`, so the later one wins.
        assert!(html.contains(";text-align:center\">"), "got: {html}");
    }

    #[test]
    fn inline_code_is_styled_and_escaped() {
        let html = html_of("use `a<b> & c`\n");
        assert!(html.contains(&format!("<code style=\"{}\">", style::CODE)));
        assert!(html.contains("a&lt;b&gt; &amp; c"), "got: {html}");
    }

    /// A fence emits `<pre>` only — never a nested `<code>` — so the
    /// double-background bug can't occur structurally.
    #[test]
    fn fenced_code_emits_pre_without_nested_code_tag() {
        let html = html_of("```scala\nval x = 1\n```\n");
        assert!(html.contains("<pre style="));
        assert!(!html.contains("<code"), "no <code> inside <pre>: {html}");
    }

    #[test]
    fn fence_info_with_attributes_uses_first_token() {
        let stats = render(
            "```scala title=Foo.scala\nval x = 1\n```\n",
            &RenderOptions::default(),
        )
        .stats;
        assert_eq!(stats.languages, vec!["Scala"], "should still resolve scala");
        assert!(stats.unhighlighted_languages.is_empty());
    }

    #[test]
    fn code_block_text_is_escaped_exactly_once() {
        let html = html_of("```\nif (a < b && c > d) {}\n```\n");
        assert!(html.contains("&lt;"), "got: {html}");
        assert!(html.contains("&amp;&amp;"), "got: {html}");
        assert!(!html.contains("&amp;lt;"), "double-escaped: {html}");
    }

    #[test]
    fn unknown_fence_language_is_reported() {
        let stats = render(
            "```gherkin\nGiven a thing\n```\n",
            &RenderOptions::default(),
        )
        .stats;
        assert_eq!(stats.code_blocks, 1);
        assert_eq!(stats.unhighlighted_languages, vec!["gherkin"]);
        assert!(stats.languages.is_empty());
    }

    /// An unlabeled fence has no missing grammar to report.
    #[test]
    fn unlabeled_fence_is_not_reported_as_unhighlighted() {
        let stats = render("```\nplain text\n```\n", &RenderOptions::default()).stats;
        assert_eq!(stats.code_blocks, 1);
        assert!(stats.unhighlighted_languages.is_empty());
        assert!(stats.languages.is_empty());
    }

    #[test]
    fn mermaid_falls_back_to_code_block_plus_note() {
        let out = render(
            "```mermaid\ngraph TD\n  A-->B\n```\n",
            &RenderOptions::default(),
        );
        assert!(
            out.html.contains("<pre style="),
            "source must remain visible"
        );
        assert!(out.html.contains("graph TD"));
        assert!(out.html.contains("insert it manually"), "got: {}", out.html);
        assert_eq!(out.stats.warnings.len(), 1);
        assert!(out.stats.warnings[0].contains("mermaid"));
    }

    #[test]
    fn links_are_styled_and_urls_escaped() {
        let html = html_of("[x](https://e.com/a?b=1&c=2)\n");
        assert!(html.contains(&format!("style=\"{}\">", style::LINK)));
        assert!(html.contains("b=1&amp;c=2"), "got: {html}");
    }

    #[test]
    fn images_degrade_to_a_labelled_note() {
        let out = render("![alt](./diagram.png)\n", &RenderOptions::default());
        assert!(out.html.contains("[image:"), "got: {}", out.html);
        assert_eq!(out.stats.images, 1);
        assert_eq!(out.stats.warnings.len(), 1);
    }

    /// With no `base_dir` there is nothing to resolve a relative path against,
    /// so the warning must say so rather than claim the file is missing.
    #[test]
    fn relative_image_without_a_base_dir_says_why() {
        let out = render("![a](./x.png)\n", &RenderOptions::default());
        assert!(
            out.stats.warnings[0].contains("not a local file"),
            "got: {:?}",
            out.stats.warnings
        );
    }

    #[test]
    fn remote_images_are_not_fetched() {
        let out = render(
            "![a](https://example.com/x.png)\n",
            &RenderOptions::default(),
        );
        assert!(out.html.contains("[image:"));
        assert!(out.stats.warnings[0].contains("not a local file"));
    }

    #[test]
    fn a_local_png_is_embedded_as_a_data_uri() {
        let dir = std::env::temp_dir().join(format!("gdocs-render-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pic.png"), b"\x89PNG\r\n\x1a\nfake").unwrap();
        let out = render(
            "![alt](pic.png)\n",
            &RenderOptions {
                base_dir: Some(dir.clone()),
                ..RenderOptions::default()
            },
        );
        assert!(
            out.html.contains("data:image/png;base64,"),
            "got: {}",
            out.html
        );
        assert!(out.stats.warnings.is_empty(), "{:?}", out.stats.warnings);
        let _ = std::fs::remove_file(dir.join("pic.png"));
    }

    #[test]
    fn a_missing_local_image_is_reported_as_missing() {
        let out = render(
            "![alt](nope.png)\n",
            &RenderOptions {
                base_dir: Some(std::env::temp_dir()),
                ..RenderOptions::default()
            },
        );
        assert!(
            out.stats.warnings[0].contains("not found"),
            "{:?}",
            out.stats.warnings
        );
    }

    /// With diagrams enabled, an svg is deferred to the async pass rather than
    /// embedded inline — Docs cannot render svg, so it must be rasterized.
    #[test]
    fn svg_is_deferred_when_rendering_is_enabled() {
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("gdocs-t.svg"), b"<svg/>").unwrap();
        let out = render(
            "![a](gdocs-t.svg)\n",
            &RenderOptions {
                diagrams: Backend::Chrome,
                base_dir: Some(dir.clone()),
                ..RenderOptions::default()
            },
        );
        assert_eq!(out.pending.len(), 1, "svg should be deferred");
        assert_eq!(out.pending[0].lang, "svg");
        let _ = std::fs::remove_file(dir.join("gdocs-t.svg"));
    }

    #[test]
    fn svg_without_rendering_enabled_explains_the_flag() {
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("gdocs-t2.svg"), b"<svg/>").unwrap();
        let out = render(
            "![a](gdocs-t2.svg)\n",
            &RenderOptions {
                base_dir: Some(dir.clone()),
                ..RenderOptions::default()
            },
        );
        assert!(out.pending.is_empty());
        assert!(
            out.stats.warnings[0].contains("chrome"),
            "{:?}",
            out.stats.warnings
        );
        let _ = std::fs::remove_file(dir.join("gdocs-t2.svg"));
    }

    /// With rendering enabled the fence becomes a placeholder for the async pass
    /// — and critically, no Chrome process is spawned by `render` itself.
    #[test]
    fn mermaid_is_deferred_when_rendering_is_enabled() {
        let out = render(
            "```mermaid\ngraph TD\n  A-->B\n```\n",
            &RenderOptions {
                diagrams: Backend::Chrome,
                ..RenderOptions::default()
            },
        );
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.pending[0].lang, "mermaid");
        assert!(out.pending[0].source.contains("graph TD"));
        assert!(
            out.html.contains(&out.pending[0].placeholder),
            "html must carry the placeholder: {}",
            out.html
        );
        assert_eq!(out.stats.diagrams, 1);
        // Nothing degraded yet — the async pass decides.
        assert!(out.stats.warnings.is_empty());
    }

    /// graphviz is recognized as a diagram but has no renderer, so even with
    /// rendering on it must degrade honestly rather than be deferred forever.
    #[test]
    fn unrenderable_diagram_language_degrades_even_when_enabled() {
        let out = render(
            "```dot\ndigraph {a->b}\n```\n",
            &RenderOptions {
                diagrams: Backend::Chrome,
                ..RenderOptions::default()
            },
        );
        assert!(out.pending.is_empty());
        assert!(out.html.contains("only mermaid"), "got: {}", out.html);
        assert_eq!(out.stats.warnings.len(), 1);
    }

    #[test]
    fn lists_blockquotes_and_task_items_render() {
        let html = html_of("- a\n- b\n\n1. one\n\n> quoted\n\n- [x] done\n- [ ] todo\n");
        assert!(html.contains(&format!("<ul style=\"{}\">", style::LIST)));
        assert!(html.contains("<ol style="));
        assert!(html.contains(&format!("<li style=\"{}\">", style::LI)));
        assert!(html.contains(&format!("<blockquote style=\"{}\">", style::BLOCKQUOTE)));
        assert!(html.contains("☑"), "got: {html}");
        assert!(html.contains("☐"), "got: {html}");
    }

    #[test]
    fn ordered_list_start_is_preserved() {
        let html = html_of("5. five\n6. six\n");
        assert!(html.contains("start=\"5\""), "got: {html}");
    }

    #[test]
    fn emphasis_strong_and_strikethrough_survive() {
        let html = html_of("*a* **b** ~~c~~\n");
        assert!(html.contains("<em>a</em>"));
        assert!(html.contains("<strong>b</strong>"));
        assert!(html.contains("<s>c</s>"));
    }

    #[test]
    fn prose_text_is_escaped() {
        let html = html_of("5 > 3 & \"quoted\" < 9\n");
        assert!(
            html.contains("5 &gt; 3 &amp; &quot;quoted&quot; &lt; 9"),
            "got: {html}"
        );
    }

    /// The structural guarantee against the previous implementation's two
    /// attribute-mangling bugs: every `style="…"` closes before its tag does.
    #[test]
    fn every_style_attribute_is_well_formed() {
        let html = html_of(
            "# h\n\ntext with `code` and [a link](https://e.com/?a=1&b=2)\n\n\
             | a | b |\n|:-:|--:|\n| 1 | 2 |\n\n```scala\ncase class A(b: String)\n```\n\n\
             > quote\n\n- item\n\n---\n\n```mermaid\ngraph TD\n```\n",
        );
        for tail in html.split("style=\"").skip(1) {
            let value = tail
                .split('"')
                .next()
                .expect("every style attribute must have a closing quote");
            assert!(
                !value.contains('>'),
                "style value closes its tag early: {value}"
            );
            assert!(
                !value.contains('<'),
                "style value contains a tag delimiter: {value}"
            );
        }
        // The failure signature the old verifier grepped for: a style attribute
        // whose closing quote is immediately followed by a letter.
        for (i, _) in html.char_indices() {
            if html[i..].starts_with("style=\"") {
                let rest = &html[i + 7..];
                if let Some(end) = rest.find('"') {
                    let after = rest[end + 1..].chars().next();
                    assert!(
                        !matches!(after, Some(c) if c.is_ascii_alphabetic()),
                        "broken attribute quoting near: {}",
                        &rest[..end.min(60)]
                    );
                }
            }
        }
    }

    #[test]
    fn raw_html_passes_through() {
        let html = html_of("<div data-x=\"1\">raw</div>\n");
        assert!(html.contains("<div data-x=\"1\">"), "got: {html}");
    }

    #[test]
    fn soft_and_hard_breaks_differ() {
        assert!(!html_of("a\nb\n").contains("<br>"));
        assert!(html_of("a  \nb\n").contains("<br>"));
    }

    #[test]
    fn empty_input_still_produces_a_wrapper() {
        let out = render("", &RenderOptions::default());
        assert_eq!(out.html, format!("<div style=\"{}\"></div>", style::BODY));
        assert_eq!(out.stats.code_blocks, 0);
    }

    /// Exercises the shape of the real fixture documents: many Scala fences, an
    /// unsupported gherkin fence, a diff fence, and tables.
    #[test]
    fn fixture_shaped_document_reports_accurate_stats() {
        let md = "# T\n\n```scala\nval a = 1\n```\n\n```gherkin\nGiven x\n```\n\n\
                  ```diff\n- a\n+ b\n```\n\n```\nbare\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let stats = render(md, &RenderOptions::default()).stats;
        assert_eq!(stats.code_blocks, 4);
        assert_eq!(stats.tables, 1);
        assert!(stats.languages.contains(&"Scala".to_string()));
        assert!(stats.languages.contains(&"Diff".to_string()));
        assert_eq!(stats.unhighlighted_languages, vec!["gherkin"]);
    }
}
