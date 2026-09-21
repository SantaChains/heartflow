//! Thin, backend-agnostic markdown IR plus its two projections.
//!
//! [`parse`] walks the pulldown-cmark event stream exactly once into an ordered
//! [`Vec<Node>`] — a *thin* IR that stays close to the event stream (block and
//! inline boundaries are markers; a fenced code block and a table collapse to a
//! single node because nothing is emitted between their open and close events).
//! Two projectors then read that one IR:
//!
//! - [`project_ansi`] replays the nodes into crossterm ANSI for the blocking
//!   REPL's streaming line renderer. It keeps the same style counters the
//!   pre-IR single-pass renderer used, so the byte output is unchanged.
//! - `project_ratatui` (added with the TUI markdown increment) maps the same
//!   nodes onto native ratatui `Span`s, so the full-screen shell renders real
//!   markdown instead of plain text.
//!
//! Parsing once and projecting twice is the point: the two terminal backends
//! share one document model and one [`crate::theme::Theme`] palette, so they can
//! no longer drift the way two independent walkers did.

use std::fmt::Write as FmtWrite;
use std::sync::OnceLock;

use crossterm::style::{Color, Stylize};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color as RtColor, Modifier, Style as RtStyle};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme as SyntaxTheme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};
use unicode_width::UnicodeWidthStr;

use crate::render::ColorTheme;
use crate::theme::{glyphs, Theme as AppTheme};

/// Inline emphasis/strong flags, resolved at parse time and baked onto a text
/// run. Link and block-quote context are *not* baked: they are carried by
/// [`Node::LinkStart`]/[`Node::QuoteStart`] markers so each projector applies
/// them with the same priority the streaming renderer always used
/// (link > strong > emphasis > quote).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextStyle {
    pub emphasis: bool,
    pub strong: bool,
}

/// One node of the thin markdown IR. The variant order mirrors the document,
/// so a projector walks the slice front-to-back exactly as the event stream
/// arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Heading open; `level` is 1..=6.
    HeadingStart {
        level: u8,
    },
    HeadingEnd,
    ParagraphStart,
    ParagraphEnd,
    /// Block-quote open. The ANSI projector emits the quote bar here and, to
    /// match the long-standing streaming-renderer behavior, keeps subsequent
    /// text quote-colored until the document ends.
    QuoteStart,
    QuoteEnd,
    ListStart,
    ListEnd,
    ItemStart,
    ItemEnd,
    /// A styled text run.
    Text {
        text: String,
        style: TextStyle,
    },
    /// Inline `` `code` ``.
    InlineCode {
        text: String,
    },
    /// OSC-8 hyperlink open with its target.
    LinkStart {
        url: String,
    },
    LinkEnd,
    /// An image, projected as a `[image:url]` marker.
    Image {
        url: String,
    },
    FootnoteRef {
        label: String,
    },
    TaskMarker {
        done: bool,
    },
    /// Inline or display math, emitted verbatim.
    Math {
        text: String,
    },
    /// Raw HTML passthrough.
    Html {
        text: String,
    },
    SoftBreak,
    HardBreak,
    Rule,
    /// A fenced or indented code block. `language` is empty for a bare fence and
    /// `"text"` for an indented block; `code` is the raw body (highlighting is a
    /// projection concern).
    CodeBlock {
        language: String,
        code: String,
    },
    /// A collected table as plain cell text (row 0 is the header). Cells are
    /// captured without styling so column-width math stays correct.
    Table {
        rows: Vec<Vec<String>>,
    },
}

/// Row-major accumulator for a markdown table, used while parsing. Cell bodies
/// are captured as plain text (no ANSI, no backticks) so widths measure right.
#[derive(Debug, Default)]
struct TableBuilder {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_cell: bool,
}

impl TableBuilder {
    fn start_row(&mut self) {
        self.current_row.clear();
    }

    fn start_cell(&mut self) {
        self.current_cell.clear();
        self.in_cell = true;
    }

    fn end_cell(&mut self) {
        if self.in_cell {
            self.current_row
                .push(std::mem::take(&mut self.current_cell));
            self.in_cell = false;
        }
    }

    /// Commit the current row. Also flushes a dangling cell so the header works
    /// whether or not the parser wraps it in an explicit `TableRow`.
    fn end_row(&mut self) {
        self.end_cell();
        let row = std::mem::take(&mut self.current_row);
        if !row.is_empty() {
            self.rows.push(row);
        }
    }
}

/// Parse `markdown` into the thin IR. One walk; both projections read the result.
#[must_use]
pub fn parse(markdown: &str) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut emphasis = 0usize;
    let mut strong = 0usize;
    let mut in_code_block = false;
    let mut code_buffer = String::new();
    let mut code_language = String::new();
    let mut table: Option<TableBuilder> = None;

    for event in Parser::new_ext(markdown, Options::all()) {
        // True while the parser is inside a table cell: text/code are diverted
        // into the builder so column widths can be measured before drawing.
        let in_cell = table.as_ref().is_some_and(|b| b.in_cell);
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                nodes.push(Node::HeadingStart { level: level as u8 });
            }
            Event::End(TagEnd::Heading(..)) => nodes.push(Node::HeadingEnd),
            Event::Start(Tag::Paragraph) => nodes.push(Node::ParagraphStart),
            Event::End(TagEnd::Paragraph) => nodes.push(Node::ParagraphEnd),
            Event::Start(Tag::BlockQuote(..)) => nodes.push(Node::QuoteStart),
            Event::End(TagEnd::BlockQuote(..)) => nodes.push(Node::QuoteEnd),
            Event::End(TagEnd::Item) => nodes.push(Node::ItemEnd),
            Event::SoftBreak => nodes.push(Node::SoftBreak),
            Event::HardBreak => nodes.push(Node::HardBreak),
            Event::Start(Tag::List(_)) => nodes.push(Node::ListStart),
            Event::End(TagEnd::List(..)) => nodes.push(Node::ListEnd),
            Event::Start(Tag::Item) => nodes.push(Node::ItemStart),
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                code_language = match kind {
                    CodeBlockKind::Indented => String::from("text"),
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                };
                code_buffer.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                nodes.push(Node::CodeBlock {
                    language: std::mem::take(&mut code_language),
                    code: std::mem::take(&mut code_buffer),
                });
                in_code_block = false;
            }
            Event::Start(Tag::Emphasis) => emphasis += 1,
            Event::End(TagEnd::Emphasis) => emphasis = emphasis.saturating_sub(1),
            Event::Start(Tag::Strong) => strong += 1,
            Event::End(TagEnd::Strong) => strong = strong.saturating_sub(1),
            Event::Code(code) => {
                if in_cell {
                    if let Some(builder) = table.as_mut() {
                        builder.current_cell.push_str(code.as_ref());
                    }
                } else {
                    nodes.push(Node::InlineCode {
                        text: code.to_string(),
                    });
                }
            }
            Event::Rule => nodes.push(Node::Rule),
            Event::Text(text) => {
                if in_cell {
                    if let Some(builder) = table.as_mut() {
                        builder.current_cell.push_str(text.as_ref());
                    }
                } else if in_code_block {
                    code_buffer.push_str(text.as_ref());
                } else {
                    nodes.push(Node::Text {
                        text: text.to_string(),
                        style: TextStyle {
                            emphasis: emphasis > 0,
                            strong: strong > 0,
                        },
                    });
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => nodes.push(Node::Html {
                text: html.to_string(),
            }),
            Event::FootnoteReference(reference) => nodes.push(Node::FootnoteRef {
                label: reference.to_string(),
            }),
            Event::TaskListMarker(done) => nodes.push(Node::TaskMarker { done }),
            Event::InlineMath(math) | Event::DisplayMath(math) => nodes.push(Node::Math {
                text: math.to_string(),
            }),
            // OSC-8 hyperlink: the label renders underlined in the link color
            // between the two markers. Inside a table cell the markers are
            // dropped (cell text is plain), mirroring the streaming renderer.
            Event::Start(Tag::Link { dest_url, .. }) => {
                if !in_cell {
                    nodes.push(Node::LinkStart {
                        url: dest_url.to_string(),
                    });
                }
            }
            Event::End(TagEnd::Link) => {
                if !in_cell {
                    nodes.push(Node::LinkEnd);
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => nodes.push(Node::Image {
                url: dest_url.to_string(),
            }),
            // -- Tables: collect rows, emit one node once the table ends ------
            Event::Start(Tag::Table(_)) => table = Some(TableBuilder::default()),
            Event::Start(Tag::TableRow) => {
                if let Some(builder) = table.as_mut() {
                    builder.start_row();
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(builder) = table.as_mut() {
                    builder.start_cell();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(builder) = table.as_mut() {
                    builder.end_cell();
                }
            }
            // Both row ends commit: the header may or may not be wrapped in an
            // explicit `TableRow` depending on the parser path.
            Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                if let Some(builder) = table.as_mut() {
                    builder.end_row();
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(builder) = table.take() {
                    nodes.push(Node::Table { rows: builder.rows });
                }
            }
            _ => {}
        }
    }
    nodes
}

/// Project the IR into crossterm ANSI for the blocking REPL's streaming line
/// renderer. Style priority matches the historical single-pass renderer exactly
/// (link > strong > emphasis > quote), and the quote color, once entered, is
/// retained for the rest of the document to keep the byte output unchanged.
#[must_use]
pub fn project_ansi(nodes: &[Node], theme: &ColorTheme, syntax_theme: &SyntaxTheme) -> String {
    let mut out = String::new();
    let mut link_depth = 0usize;
    let mut list_depth = 0usize;
    let mut quote_active = false;
    for node in nodes {
        match node {
            Node::HeadingStart { level } => {
                out.push('\n');
                let prefix = match level {
                    1 => "# ",
                    2 => "## ",
                    3 => "### ",
                    _ => "#### ",
                };
                let _ = write!(out, "{}", prefix.bold().with(theme.heading()));
            }
            Node::HeadingEnd | Node::ParagraphEnd => out.push_str("\n\n"),
            Node::ParagraphStart => {}
            Node::QuoteStart => {
                quote_active = true;
                let _ = write!(out, "{}", glyphs::QUOTE_BAR.with(theme.quote()));
            }
            Node::QuoteEnd | Node::ItemEnd | Node::SoftBreak | Node::HardBreak => {
                out.push('\n');
            }
            Node::ListStart => list_depth += 1,
            Node::ListEnd => {
                list_depth = list_depth.saturating_sub(1);
                out.push('\n');
            }
            Node::ItemStart => {
                out.push_str(&"  ".repeat(list_depth.saturating_sub(1)));
                out.push_str(glyphs::BULLET);
            }
            Node::Text { text, style } => {
                out.push_str(&style_text(
                    text,
                    *style,
                    link_depth > 0,
                    quote_active,
                    theme,
                ));
            }
            Node::InlineCode { text } => {
                let _ = write!(out, "{}", format!("`{text}`").with(theme.inline_code()));
            }
            Node::LinkStart { url } => {
                link_depth += 1;
                out.push_str("\u{1b}]8;;");
                out.push_str(url);
                out.push('\u{7}');
            }
            Node::LinkEnd => {
                link_depth = link_depth.saturating_sub(1);
                out.push_str("\u{1b}[0m\u{1b}]8;;\u{7}");
            }
            Node::Image { url } => {
                let _ = write!(out, "{}", format!("[image:{url}]").with(theme.link()));
            }
            Node::FootnoteRef { label } => {
                let _ = write!(out, "[{label}]");
            }
            Node::TaskMarker { done } => {
                out.push_str(if *done { "[x] " } else { "[ ] " });
            }
            Node::Math { text } | Node::Html { text } => out.push_str(text),
            Node::Rule => out.push_str(glyphs::RULE),
            Node::CodeBlock { language, code } => {
                if !language.is_empty() {
                    let _ = writeln!(
                        out,
                        "{}",
                        format!("{}{language}", glyphs::CODE_OPEN).with(theme.heading())
                    );
                }
                out.push_str(&highlight_code(code, language, syntax_theme));
                if !language.is_empty() {
                    let _ = write!(out, "{}", glyphs::CODE_CLOSE.with(theme.heading()));
                }
                out.push_str("\n\n");
            }
            Node::Table { rows } => render_table(rows, theme, &mut out),
        }
    }
    out
}

/// Resolve one text run's ANSI styling. Priority mirrors the historical
/// renderer: an active link wins, then strong, then emphasis, then the (sticky)
/// quote color, then plain.
fn style_text(
    text: &str,
    style: TextStyle,
    in_link: bool,
    quote_active: bool,
    theme: &ColorTheme,
) -> String {
    if in_link {
        format!("{}", text.underlined().with(theme.link()))
    } else if style.strong {
        format!("{}", text.bold().with(theme.strong()))
    } else if style.emphasis {
        format!("{}", text.italic().with(theme.emphasis()))
    } else if quote_active {
        format!("{}", text.with(theme.quote()))
    } else {
        text.to_string()
    }
}

/// Project the IR into ratatui [`Line`]s for the full-screen TUI transcript —
/// the second consumer of the one IR ([`project_ansi`] is the first), so both
/// terminal backends render the same document model through the same
/// [`AppTheme`] palette roles and cannot drift. Where the ANSI projector emits a
/// flat string, this emits one [`Line`] per row: an ANSI `\n` becomes a line
/// break, inline styles become [`Span`]s, and a fenced block is highlighted by
/// mapping syntect's foreground colors onto ratatui RGB.
///
/// Heading *text* stays plain with only the `#`-marker colored, mirroring
/// [`project_ansi`] exactly, so a future "prettier heading" change is made once
/// in the IR and lands on both backends together. Run once per flushed assistant
/// segment (never per frame): the transcript stores the owned `Line<'static>`
/// and each redraw re-borrows it, keeping the hot frame path allocation-free.
#[must_use]
pub fn project_ratatui(nodes: &[Node], theme: &AppTheme) -> Vec<Line<'static>> {
    let mut projector = RtProjector::new(*theme, global_syntax_theme());
    for node in nodes {
        projector.emit(node);
    }
    projector.finish()
}

/// Line-accumulating state for [`project_ratatui`]. Mirrors the counters
/// [`project_ansi`] keeps (link/list depth, sticky quote) so both projectors
/// resolve styles identically.
struct RtProjector {
    theme: AppTheme,
    syntax_theme: &'static SyntaxTheme,
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    link_depth: usize,
    list_depth: usize,
    quote_active: bool,
}

impl RtProjector {
    fn new(theme: AppTheme, syntax_theme: &'static SyntaxTheme) -> Self {
        Self {
            theme,
            syntax_theme,
            lines: Vec::new(),
            cur: Vec::new(),
            link_depth: 0,
            list_depth: 0,
            quote_active: false,
        }
    }

    /// Finish the current row (an ANSI `\n`); an empty row is preserved so
    /// blank-line spacing matches the ANSI projector.
    fn newline(&mut self) {
        let spans = std::mem::take(&mut self.cur);
        self.lines.push(Line::from(spans));
    }

    /// Append a styled run to the current row, ignoring empty text so a stray
    /// zero-width marker never allocates a span.
    fn span(&mut self, text: impl Into<String>, style: RtStyle) {
        let text = text.into();
        if !text.is_empty() {
            self.cur.push(Span::styled(text, style));
        }
    }

    fn heading_style(&self) -> RtStyle {
        RtStyle::default().fg(self.theme.heading().ratatui())
    }
    fn quote_style(&self) -> RtStyle {
        RtStyle::default().fg(self.theme.quote().ratatui())
    }
    fn inline_code_style(&self) -> RtStyle {
        RtStyle::default().fg(self.theme.inline_code().ratatui())
    }
    fn link_style(&self) -> RtStyle {
        RtStyle::default()
            .fg(self.theme.link().ratatui())
            .add_modifier(Modifier::UNDERLINED)
    }

    /// Resolve a text run's style with the same priority as [`style_text`]:
    /// link > strong > emphasis > (sticky) quote > plain.
    fn text_style(&self, style: TextStyle) -> RtStyle {
        if self.link_depth > 0 {
            self.link_style()
        } else if style.strong {
            RtStyle::default()
                .fg(self.theme.strong().ratatui())
                .add_modifier(Modifier::BOLD)
        } else if style.emphasis {
            RtStyle::default()
                .fg(self.theme.emphasis().ratatui())
                .add_modifier(Modifier::ITALIC)
        } else if self.quote_active {
            self.quote_style()
        } else {
            RtStyle::default()
        }
    }

    fn emit(&mut self, node: &Node) {
        match node {
            Node::HeadingStart { level } => {
                self.newline();
                let prefix = match level {
                    1 => "# ",
                    2 => "## ",
                    3 => "### ",
                    _ => "#### ",
                };
                self.span(prefix, self.heading_style().add_modifier(Modifier::BOLD));
            }
            Node::HeadingEnd | Node::ParagraphEnd => {
                self.newline();
                self.newline();
            }
            Node::ParagraphStart => {}
            Node::QuoteStart => {
                self.quote_active = true;
                self.span(glyphs::QUOTE_BAR, self.quote_style());
            }
            Node::QuoteEnd | Node::ItemEnd | Node::SoftBreak | Node::HardBreak => self.newline(),
            Node::ListStart => self.list_depth += 1,
            Node::ListEnd => {
                self.list_depth = self.list_depth.saturating_sub(1);
                self.newline();
            }
            Node::ItemStart => {
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                self.span(format!("{indent}{}", glyphs::BULLET), RtStyle::default());
            }
            Node::Text { text, style } => {
                let resolved = self.text_style(*style);
                self.span(text.clone(), resolved);
            }
            Node::InlineCode { text } => {
                let styled = self.inline_code_style();
                self.span(format!("`{text}`"), styled);
            }
            // ratatui `Span`s carry no OSC-8 target, so a link renders as its
            // underlined label (the URL is not shown as text, matching the ANSI
            // projector's visible output); depth still drives `text_style`.
            Node::LinkStart { .. } => self.link_depth += 1,
            Node::LinkEnd => self.link_depth = self.link_depth.saturating_sub(1),
            Node::Image { url } => {
                let styled = self.link_style();
                self.span(format!("[image:{url}]"), styled);
            }
            Node::FootnoteRef { label } => self.span(format!("[{label}]"), RtStyle::default()),
            Node::TaskMarker { done } => {
                self.span(if *done { "[x] " } else { "[ ] " }, RtStyle::default());
            }
            Node::Math { text } | Node::Html { text } => {
                self.span(text.clone(), RtStyle::default());
            }
            Node::Rule => {
                let muted = RtStyle::default().fg(self.theme.muted().ratatui());
                self.span(glyphs::RULE.trim_end_matches('\n'), muted);
                self.newline();
            }
            Node::CodeBlock { language, code } => self.code_block(language, code),
            Node::Table { rows } => self.table(rows),
        }
    }

    fn code_block(&mut self, language: &str, code: &str) {
        if !language.is_empty() {
            let head = self.heading_style();
            self.span(format!("{}{language}", glyphs::CODE_OPEN), head);
        }
        if !self.cur.is_empty() {
            self.newline();
        }
        self.lines
            .extend(highlight_code_ratatui(code, language, self.syntax_theme));
        if !language.is_empty() {
            let head = self.heading_style();
            self.span(glyphs::CODE_CLOSE, head);
            self.newline();
        }
        self.newline();
    }

    fn table(&mut self, rows: &[Vec<String>]) {
        if !self.cur.is_empty() {
            self.newline();
        }
        self.lines.extend(render_table_ratatui(rows, &self.theme));
        self.newline();
    }

    /// Flush a dangling row, then drop trailing blank rows — the [`Line`]
    /// equivalent of the ANSI projector's `trim_end`.
    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.cur.is_empty() {
            self.newline();
        }
        while self
            .lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
        {
            self.lines.pop();
        }
        self.lines
    }
}

/// Highlight a code block into ratatui [`Line`]s (one per source line). Past
/// [`MAX_SYNTECT_BYTES`] the body is emitted verbatim, bounding the render-path
/// cost exactly as the ANSI [`highlight_code`] does.
fn highlight_code_ratatui(
    code: &str,
    language: &str,
    syntax_theme: &SyntaxTheme,
) -> Vec<Line<'static>> {
    if code.len() > MAX_SYNTECT_BYTES {
        return code
            .lines()
            .map(|line| Line::from(Span::raw(line.to_string())))
            .collect();
    }
    let syntax_set = global_syntax_set();
    let syntax = syntax_set
        .find_syntax_by_token(language)
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, syntax_theme);
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(code) {
        let mut spans = Vec::new();
        match highlighter.highlight_line(line, syntax_set) {
            Ok(ranges) => {
                for (style, text) in ranges {
                    let text = text.trim_end_matches(['\n', '\r']);
                    if !text.is_empty() {
                        spans.push(Span::styled(text.to_string(), syntect_to_ratatui(&style)));
                    }
                }
            }
            Err(_) => spans.push(Span::raw(line.trim_end_matches(['\n', '\r']).to_string())),
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Map a syntect foreground color onto a ratatui RGB style.
fn syntect_to_ratatui(style: &syntect::highlighting::Style) -> RtStyle {
    let fg = style.foreground;
    RtStyle::default().fg(RtColor::Rgb(fg.r, fg.g, fg.b))
}

/// Draw collected table rows as a light box grid of ratatui [`Line`]s — the
/// structural twin of [`render_table`], with the same CJK-aware column widths
/// and bolded header row.
fn render_table_ratatui(rows: &[Vec<String>], theme: &AppTheme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if rows.is_empty() {
        return out;
    }
    let cols = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    if cols == 0 {
        return out;
    }
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < cols {
                widths[index] = widths[index].max(cell.width());
            }
        }
    }
    let border = RtStyle::default().fg(theme.muted().ratatui());
    let head = RtStyle::default()
        .fg(theme.heading().ratatui())
        .add_modifier(Modifier::BOLD);
    let last = rows.len().saturating_sub(1);
    out.push(table_rule_ratatui(
        &widths,
        glyphs::TBL_TL,
        glyphs::TBL_TOP_T,
        glyphs::TBL_TR,
        border,
    ));
    for (index, row) in rows.iter().enumerate() {
        out.push(table_row_ratatui(row, &widths, index == 0, border, head));
        if index == 0 && last > 0 {
            out.push(table_rule_ratatui(
                &widths,
                glyphs::TBL_ML,
                glyphs::TBL_CROSS,
                glyphs::TBL_MR,
                border,
            ));
        }
    }
    out.push(table_rule_ratatui(
        &widths,
        glyphs::TBL_BL,
        glyphs::TBL_BOT_T,
        glyphs::TBL_BR,
        border,
    ));
    out
}

/// One horizontal rule of a ratatui table grid (`┌─┬─┐` / `├─┼─┤` / `└─┴─┘`).
fn table_rule_ratatui(
    widths: &[usize],
    left: &str,
    mid: &str,
    right: &str,
    style: RtStyle,
) -> Line<'static> {
    let mut line = String::from(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            line.push_str(mid);
        }
        line.push_str(&glyphs::TBL_H.repeat(*width + 2));
    }
    line.push_str(right);
    Line::from(Span::styled(line, style))
}

/// One ratatui table row: each cell padded to its column width and wrapped in
/// the vertical divider; header cells are bolded in the heading color.
fn table_row_ratatui(
    cells: &[String],
    widths: &[usize],
    header: bool,
    border: RtStyle,
    head: RtStyle,
) -> Line<'static> {
    let mut spans = vec![Span::styled(glyphs::TBL_V, border)];
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        let pad = width.saturating_sub(cell.width());
        let body = if header { head } else { RtStyle::default() };
        spans.push(Span::styled(" ", body));
        spans.push(Span::styled(cell.to_string(), body));
        spans.push(Span::styled(" ".repeat(pad + 1), body));
        spans.push(Span::styled(glyphs::TBL_V, border));
    }
    Line::from(spans)
}

/// Upper bound on the code-block size syntect will highlight. Highlighting is a
/// regex tokenizer run on the render path, so an oversized block is emitted raw
/// instead of risking a frame stall (the non-blocking guard for A4). Tunable; a
/// future `[settings]` key may override it.
pub const MAX_SYNTECT_BYTES: usize = 32 * 1024;

/// Process-wide syntax definitions, loaded once (~10ms) instead of per turn.
pub fn global_syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Process-wide highlight theme, loaded once instead of per turn.
pub fn global_syntax_theme() -> &'static SyntaxTheme {
    static THEME: OnceLock<SyntaxTheme> = OnceLock::new();
    THEME.get_or_init(|| {
        ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_default()
    })
}

/// Highlight `code` as `language` into 24-bit ANSI. Past [`MAX_SYNTECT_BYTES`]
/// the body is returned verbatim so a pathological block cannot stall the
/// render loop; an unknown language falls back to plain text.
#[must_use]
pub fn highlight_code(code: &str, language: &str, syntax_theme: &SyntaxTheme) -> String {
    if code.len() > MAX_SYNTECT_BYTES {
        return code.to_string();
    }
    let syntax_set = global_syntax_set();
    let syntax = syntax_set
        .find_syntax_by_token(language)
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, syntax_theme);
    let mut colored = String::new();
    for line in LinesWithEndings::from(code) {
        match highlighter.highlight_line(line, syntax_set) {
            Ok(ranges) => colored.push_str(&as_24_bit_terminal_escaped(&ranges[..], false)),
            Err(_) => colored.push_str(line),
        }
    }
    colored
}

/// Draw collected table rows as a light box grid. Column widths are the max
/// display width (CJK-aware) of each cell plus one space of padding either side;
/// the first row is treated as the header and bolded.
fn render_table(rows: &[Vec<String>], theme: &ColorTheme, out: &mut String) {
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < cols {
                widths[index] = widths[index].max(cell.width());
            }
        }
    }
    let border = theme.muted();
    let head = theme.heading();
    let last = rows.len().saturating_sub(1);

    out.push('\n');
    out.push_str(&table_rule(
        &widths,
        glyphs::TBL_TL,
        glyphs::TBL_TOP_T,
        glyphs::TBL_TR,
        border,
    ));
    for (index, row) in rows.iter().enumerate() {
        out.push_str(&table_row(row, &widths, index == 0, border, head));
        if index == 0 && last > 0 {
            out.push_str(&table_rule(
                &widths,
                glyphs::TBL_ML,
                glyphs::TBL_CROSS,
                glyphs::TBL_MR,
                border,
            ));
        }
    }
    out.push_str(&table_rule(
        &widths,
        glyphs::TBL_BL,
        glyphs::TBL_BOT_T,
        glyphs::TBL_BR,
        border,
    ));
    out.push('\n');
}

/// One horizontal rule of a table grid (`┌─┬─┐` / `├─┼─┤` / `└─┴─┘`).
/// Each span is the column width plus two (one space of cell padding per side).
fn table_rule(widths: &[usize], left: &str, mid: &str, right: &str, color: Color) -> String {
    let mut line = String::new();
    line.push_str(&format!("{}", left.with(color)));
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            line.push_str(&format!("{}", mid.with(color)));
        }
        line.push_str(&format!("{}", glyphs::TBL_H.repeat(*width + 2).with(color)));
    }
    line.push_str(&format!("{}\n", right.with(color)));
    line
}

/// One table row: each cell padded to its column width and wrapped in the
/// vertical divider. Header cells are bolded in the heading color; padding is
/// computed from the plain-text width so the ANSI styling never shifts columns.
fn table_row(
    cells: &[String],
    widths: &[usize],
    header: bool,
    border: Color,
    head: Color,
) -> String {
    let mut line = String::new();
    line.push_str(&format!("{}", glyphs::TBL_V.with(border)));
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        let pad = width.saturating_sub(cell.width());
        let styled = format!("{}", cell.bold().with(head));
        let text = if header { &styled } else { cell };
        line.push(' ');
        line.push_str(text);
        line.push_str(&" ".repeat(pad + 1));
        line.push_str(&format!("{}", glyphs::TBL_V.with(border)));
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::{
        global_syntax_theme, highlight_code, parse, project_ratatui, AppTheme, Node, TextStyle,
        MAX_SYNTECT_BYTES,
    };
    use ratatui::style::Modifier;
    use ratatui::text::Line;

    /// The visible text of a projected row (spans concatenated), so tests assert
    /// layout without coupling to palette RGB.
    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The IR keeps document order and collapses a fenced code block to one
    /// node carrying its language and raw body.
    #[test]
    fn parses_a_fenced_code_block_into_one_node() {
        let nodes = parse("```rust\nfn main() {}\n```");
        let code = nodes
            .iter()
            .find_map(|node| match node {
                Node::CodeBlock { language, code } => Some((language.clone(), code.clone())),
                _ => None,
            })
            .expect("a code block node");
        assert_eq!(code.0, "rust");
        assert!(code.1.contains("fn main()"));
    }

    /// Emphasis/strong are baked onto the text run at parse time; the link and
    /// quote context stay as markers for the projector.
    #[test]
    fn bakes_inline_styles_onto_text_runs() {
        let nodes = parse("**bold** and *italic*");
        let styles: Vec<TextStyle> = nodes
            .iter()
            .filter_map(|node| match node {
                Node::Text { style, .. } => Some(*style),
                _ => None,
            })
            .collect();
        assert!(styles.iter().any(|s| s.strong && !s.emphasis), "bold run");
        assert!(styles.iter().any(|s| s.emphasis && !s.strong), "italic run");
    }

    /// A table collapses to a single node of plain cell rows (header first),
    /// with inline code contributing only its text so width math stays right.
    #[test]
    fn collects_a_table_into_plain_rows() {
        let nodes = parse("| key | v |\n|-----|---|\n| `a` | 1 |\n");
        let rows = nodes
            .iter()
            .find_map(|node| match node {
                Node::Table { rows } => Some(rows.clone()),
                _ => None,
            })
            .expect("a table node");
        assert_eq!(rows[0], vec!["key".to_string(), "v".to_string()]);
        // Inline code in a cell contributes bare text (no backticks).
        assert_eq!(rows[1], vec!["a".to_string(), "1".to_string()]);
    }

    /// A link becomes an open/close marker pair wrapping its label text (the
    /// pair sits inside the enclosing paragraph, so locate it by position
    /// rather than assuming it opens the document).
    #[test]
    fn wraps_link_labels_in_markers() {
        let nodes = parse("[docs](https://example.com)");
        let open = nodes.iter().position(
            |node| matches!(node, Node::LinkStart { url } if url == "https://example.com"),
        );
        let close = nodes.iter().position(|node| matches!(node, Node::LinkEnd));
        let (Some(open), Some(close)) = (open, close) else {
            panic!("expected a LinkStart/LinkEnd pair");
        };
        assert!(open < close, "open marker precedes close");
        assert!(
            nodes[open + 1..close]
                .iter()
                .any(|node| matches!(node, Node::Text { text, .. } if text == "docs")),
            "label text sits between the markers"
        );
    }

    /// Beyond the cap the body comes back verbatim (no syntect ANSI), bounding
    /// the render-path cost; a small block is still highlighted.
    #[test]
    fn oversized_code_blocks_skip_highlighting() {
        let syntax_theme = global_syntax_theme();
        let huge = "x".repeat(MAX_SYNTECT_BYTES + 1);
        assert_eq!(highlight_code(&huge, "rust", syntax_theme), huge);
        let small = highlight_code("fn main() { let x = 1; }", "rust", syntax_theme);
        assert!(
            small.contains('\u{1b}'),
            "small block should carry syntect ANSI"
        );
    }

    /// The ratatui projector mirrors the ANSI layout: a heading is a leading
    /// blank row then its `#`-marker row, trailing blanks trimmed, and the
    /// marker span carries the bold heading role.
    #[test]
    fn projects_a_heading_to_ratatui_rows() {
        let nodes = parse("# Title");
        let lines = project_ratatui(&nodes, &AppTheme::default());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec![String::new(), "# Title".to_string()]);
        assert!(lines[1]
            .spans
            .first()
            .is_some_and(|span| span.style.add_modifier.contains(Modifier::BOLD)));
    }

    /// A plain paragraph projects to a single row (no markdown noise), so the
    /// common assistant reply looks exactly as it did before markdown rendering.
    #[test]
    fn projects_plain_text_to_a_single_row() {
        let nodes = parse("Hello world");
        let lines = project_ratatui(&nodes, &AppTheme::default());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec!["Hello world".to_string()]);
    }

    /// A tight list keeps its bullet glyph, one row per item.
    #[test]
    fn projects_a_list_with_bullets() {
        let nodes = parse("- a\n- b");
        let lines = project_ratatui(&nodes, &AppTheme::default());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec!["• a".to_string(), "• b".to_string()]);
    }

    /// A fenced block is bracketed by the open mark (carrying its language) and
    /// the close mark, with the highlighted body between them.
    #[test]
    fn projects_a_fenced_code_block_with_fence_marks() {
        let nodes = parse("```rust\nfn main() {}\n```");
        let lines = project_ratatui(&nodes, &AppTheme::default());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|text| text.starts_with("╭─ rust")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|text| text == "╰─"), "{texts:?}");
        assert!(
            texts.iter().any(|text| text.contains("fn main")),
            "{texts:?}"
        );
    }
}
