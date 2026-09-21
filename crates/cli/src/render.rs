use std::io::{self, Write};

use crossterm::cursor::{MoveToColumn, RestorePosition, SavePosition};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};
use syntect::highlighting::Theme as SyntaxTheme;

use crate::markdown;
use crate::mascot::Mascot;
use crate::theme::{glyphs, Theme as AppTheme};

/// Crossterm projection of the shared [`AppTheme`] palette, consumed by the line
/// renderer and [`Spinner`]. Colors are *derived*, never chosen here, so the
/// idle input editor (which reads [`AppTheme`] directly through ratatui) and
/// this streaming renderer stay in one color family. See [`crate::theme`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTheme {
    heading: Color,
    emphasis: Color,
    strong: Color,
    inline_code: Color,
    link: Color,
    quote: Color,
    muted: Color,
    spinner_active: Color,
    spinner_done: Color,
    spinner_failed: Color,
}

impl ColorTheme {
    /// Project the canonical [`AppTheme`] palette into crossterm colors.
    #[must_use]
    pub fn from(theme: &AppTheme) -> Self {
        Self {
            heading: theme.heading().crossterm(),
            emphasis: theme.emphasis().crossterm(),
            strong: theme.strong().crossterm(),
            inline_code: theme.inline_code().crossterm(),
            link: theme.link().crossterm(),
            quote: theme.quote().crossterm(),
            muted: theme.muted().crossterm(),
            spinner_active: theme.accent().crossterm(),
            spinner_done: theme.success().crossterm(),
            spinner_failed: theme.error().crossterm(),
        }
    }

    /// Heading / code-fence accent.
    #[must_use]
    pub fn heading(&self) -> Color {
        self.heading
    }
    /// Italic emphasis color.
    #[must_use]
    pub fn emphasis(&self) -> Color {
        self.emphasis
    }
    /// Bold strong color.
    #[must_use]
    pub fn strong(&self) -> Color {
        self.strong
    }
    /// Inline-code color.
    #[must_use]
    pub fn inline_code(&self) -> Color {
        self.inline_code
    }
    /// Link / image color.
    #[must_use]
    pub fn link(&self) -> Color {
        self.link
    }
    /// Block-quote bar and quoted-text color.
    #[must_use]
    pub fn quote(&self) -> Color {
        self.quote
    }
    /// Dimmed foreground for secondary/live text (deltas, running markers).
    #[must_use]
    pub fn muted(&self) -> Color {
        self.muted
    }
}

impl Default for ColorTheme {
    fn default() -> Self {
        Self::from(&AppTheme::current())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Spinner {
    frame_index: usize,
}

impl Spinner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        // The mascot's busy "scanning" face replaces the plain braille dot: it
        // alternates each event, so a run of tool calls reads as the figure
        // working rather than a generic spinner. Advances per event (no timer).
        let frame = Mascot::busy_face(self.frame_index);
        self.frame_index += 1;
        queue!(
            out,
            SavePosition,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_active),
            Print(format!("{frame} {label}")),
            ResetColor,
            RestorePosition
        )?;
        out.flush()
    }

    pub fn finish(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.frame_index = 0;
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_done),
            Print(format!("{} {label}\n", glyphs::DONE)),
            ResetColor
        )?;
        out.flush()
    }

    pub fn fail(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.frame_index = 0;
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_failed),
            Print(format!("{} {label}\n", glyphs::FAILED)),
            ResetColor
        )?;
        out.flush()
    }

    /// Clear the spinner line and print a neutral, non-alarming marker for a
    /// user-initiated interruption, so a Ctrl+C never looks like a failure.
    pub fn cancel(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.frame_index = 0;
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.muted),
            Print(format!("{} {label}\n", glyphs::CANCELLED)),
            ResetColor
        )?;
        out.flush()
    }
}

/// The blocking REPL's streaming renderer: a thin front over the shared
/// markdown IR ([`crate::markdown`]). It owns a snapshot of the palette
/// ([`ColorTheme`]) and the syntect highlight theme so a turn renders in one
/// consistent color family; markdown is parsed once and projected to ANSI.
#[derive(Debug)]
pub struct TerminalRenderer {
    syntax_theme: &'static SyntaxTheme,
    color_theme: ColorTheme,
}

impl Default for TerminalRenderer {
    fn default() -> Self {
        Self {
            syntax_theme: markdown::global_syntax_theme(),
            color_theme: ColorTheme::default(),
        }
    }
}

impl TerminalRenderer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn color_theme(&self) -> &ColorTheme {
        &self.color_theme
    }

    /// Render markdown to crossterm ANSI. Parses into the shared thin IR then
    /// projects; trailing whitespace is trimmed so a streamed message does not
    /// push extra blank lines into the scroll region.
    #[must_use]
    pub fn render_markdown(&self, markdown: &str) -> String {
        let nodes = markdown::parse(markdown);
        markdown::project_ansi(&nodes, &self.color_theme, self.syntax_theme)
            .trim_end()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{Spinner, TerminalRenderer};
    use unicode_width::UnicodeWidthStr;

    fn strip_ansi(input: &str) -> String {
        let mut output = String::new();
        let mut chars = input.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                output.push(ch);
            }
        }

        output
    }

    #[test]
    fn renders_markdown_with_styling_and_lists() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output = terminal_renderer
            .render_markdown("# Heading\n\nThis is **bold** and *italic*.\n\n- item\n\n`code`");

        assert!(markdown_output.contains("Heading"));
        assert!(markdown_output.contains("• item"));
        assert!(markdown_output.contains("code"));
        assert!(markdown_output.contains('\u{1b}'));
    }

    #[test]
    fn highlights_fenced_code_blocks() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.render_markdown("```rust\nfn hi() { println!(\"hi\"); }\n```");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("╭─ rust"));
        assert!(plain_text.contains("fn hi"));
        assert!(markdown_output.contains('\u{1b}'));
    }

    #[test]
    fn spinner_advances_frames() {
        let terminal_renderer = TerminalRenderer::new();
        let mut spinner = Spinner::new();
        let mut out = Vec::new();
        spinner
            .tick("Working", terminal_renderer.color_theme(), &mut out)
            .expect("tick succeeds");
        spinner
            .tick("Working", terminal_renderer.color_theme(), &mut out)
            .expect("tick succeeds");

        let output = String::from_utf8_lossy(&out);
        assert!(output.contains("Working"));
    }

    #[test]
    fn renders_markdown_table_as_an_aligned_grid() {
        let renderer = TerminalRenderer::new();
        let out = renderer.render_markdown("| key | v |\n|-----|---|\n| alpha | 1 |\n| β | 22 |\n");
        let plain = strip_ansi(&out);
        // A full box grid is drawn.
        assert!(plain.contains('┌') && plain.contains('┬') && plain.contains('┐'));
        assert!(plain.contains('├') && plain.contains('┼') && plain.contains('┤'));
        assert!(plain.contains('└') && plain.contains('┴') && plain.contains('┘'));
        // Header and body cells survive.
        assert!(plain.contains("key") && plain.contains("alpha") && plain.contains("22"));
        // Columns align: every grid line has the same visible width (the CJK
        // `β` cell is one column wide, so padding must account for it).
        let grid_lines: Vec<usize> = plain
            .lines()
            .filter(|line| line.contains('│') || line.contains('─'))
            .map(|line| line.width())
            .collect();
        assert!(
            grid_lines.windows(2).all(|w| w[0] == w[1]),
            "ragged grid: {grid_lines:?}"
        );
    }

    #[test]
    fn renders_link_as_an_osc8_hyperlink() {
        let renderer = TerminalRenderer::new();
        let out = renderer.render_markdown("see [the docs](https://example.com/x) now");
        // OSC-8 open carries the URL; the label is the visible text; OSC-8 close
        // terminates the link.
        assert!(
            out.contains("\u{1b}]8;;https://example.com/x\u{7}"),
            "missing OSC-8 open: {out:?}"
        );
        assert!(out.contains("the docs"), "label text is shown");
        assert!(out.contains("\u{1b}]8;;\u{7}"), "missing OSC-8 close");
        // The bare URL is no longer dumped as visible `[url]` text.
        assert!(!strip_ansi(&out).contains("[https://example.com/x]"));
    }

    // Plain-text goldens: these lock the newline/spacing layout the streaming
    // renderer has always produced, so the IR refactor cannot silently shift the
    // scroll-region output. Values are traced from the block/inline boundaries
    // (heading open emits a leading newline and its close a blank line, a list
    // item ends with one newline and the list another, a soft break is one
    // newline, a quote bar prefixes the quoted run).
    #[test]
    fn heading_layout_is_preserved() {
        let renderer = TerminalRenderer::new();
        assert_eq!(
            strip_ansi(&renderer.render_markdown("# Title")),
            "\n# Title"
        );
        assert_eq!(
            strip_ansi(&renderer.render_markdown("# H1\n\n## H2")),
            "\n# H1\n\n\n## H2"
        );
    }

    #[test]
    fn tight_list_layout_is_preserved() {
        let renderer = TerminalRenderer::new();
        assert_eq!(
            strip_ansi(&renderer.render_markdown("- a\n- b")),
            "• a\n• b"
        );
    }

    #[test]
    fn soft_break_becomes_a_single_newline() {
        let renderer = TerminalRenderer::new();
        assert_eq!(
            strip_ansi(&renderer.render_markdown("line one\nline two")),
            "line one\nline two"
        );
    }

    #[test]
    fn blockquote_prefixes_the_bar_and_keeps_spacing() {
        let renderer = TerminalRenderer::new();
        assert_eq!(
            strip_ansi(&renderer.render_markdown("> quoted\n\nafter")),
            "│ quoted\n\n\nafter"
        );
    }
}
