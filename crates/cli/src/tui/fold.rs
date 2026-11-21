use std::borrow::Cow;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::theme::Theme;

/// One transcript entry. Output entries (`Line`) are the answer and always
/// show; process entries (`Thinking`, `Tool`) are the machinery that produced
/// it and fold to a one-row marker so the real output stands out. Keeping
/// process bodies structured (rather than pre-flattened into `Line`s) is what
/// makes folding a pure view toggle that never loses text.
pub(super) enum Item {
    /// A finished output line: submission echo, notice, or flushed assistant text.
    Line(Line<'static>),
    /// Folded reasoning that preceded an answer. Process, not output, so it
    /// starts collapsed; `text` retains the full body so `expanded` can flip
    /// without re-fetching.
    Thinking { text: String, expanded: bool },
    /// A tool result. `output` retains the full body so `expanded` can flip
    /// without re-fetching; `expanded` seeds from the length heuristic.
    Tool {
        name: String,
        is_error: bool,
        output: Vec<String>,
        expanded: bool,
    },
}

impl Item {
    /// Process entries (reasoning, tool results) fold; output lines do not.
    pub(super) fn is_process(&self) -> bool {
        matches!(self, Item::Thinking { .. } | Item::Tool { .. })
    }

    /// Flip this entry's fold. Only process entries fold, so an output line is
    /// a no-op returning `false`.
    pub(super) fn toggle(&mut self) -> bool {
        match self {
            Item::Thinking { expanded, .. } | Item::Tool { expanded, .. } => {
                *expanded = !*expanded;
                true
            }
            Item::Line(_) => false,
        }
    }
}

/// Re-borrow a stored `'static` transcript line for one frame without cloning
/// its strings. The transcript owns its `Line<'static>`; the per-frame
/// projection only reads it, so borrowing keeps a redraw allocation-free
/// instead of deep-cloning every span (A3).
pub(super) fn reborrow_line<'a>(line: &'a Line<'static>) -> Line<'a> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|span| {
                let content: &str = &span.content;
                Span {
                    style: span.style,
                    content: Cow::Borrowed(content),
                }
            })
            .collect(),
    }
}

/// Flatten transcript items into owned display lines: a `Line` is one row; a
/// `Tool` is its marker row plus, when expanded, its indented body rows. The
/// result is `'static` so it can be memoized in [`App::render_cache`]; a redraw
/// then re-borrows only the visible window from that cache instead of
/// re-projecting the whole transcript every frame.
pub(super) fn render_items(items: &[Item]) -> Vec<Line<'static>> {
    let theme = Theme::current();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::Line(line) => out.push(line.clone()),
            Item::Thinking { text, expanded } => {
                let process = Style::default()
                    .fg(theme.muted().ratatui())
                    .add_modifier(Modifier::DIM | Modifier::ITALIC);
                let n = text.lines().count();
                let hint = match (*expanded, n) {
                    (_, 0) => String::new(),
                    (true, n) => format!("  ▾ {n} lines (Tab collapse)"),
                    (false, n) => format!("  ▸ {n} lines (Tab expand)"),
                };
                out.push(Line::from(Span::styled(
                    format!("· thinking{hint}"),
                    process,
                )));
                if *expanded {
                    for line in text.lines() {
                        out.push(Line::from(Span::styled(format!("    {line}"), process)));
                    }
                }
            }
            Item::Tool {
                name,
                is_error,
                output,
                expanded,
            } => {
                let head = if *is_error {
                    format!("✘ `{name}` failed")
                } else {
                    format!("· `{name}` done")
                };
                let hint = match (*expanded, output.len()) {
                    (_, 0) => String::new(),
                    (true, n) => format!("  ▾ {n} lines (Tab collapse)"),
                    (false, n) => format!("  ▸ {n} lines (Tab expand)"),
                };
                let style = if *is_error {
                    Style::default().fg(theme.error().ratatui())
                } else {
                    Style::default().fg(theme.muted().ratatui())
                };
                out.push(Line::from(Span::styled(format!("{head}{hint}"), style)));
                if *expanded {
                    for line in output {
                        out.push(Line::from(Span::styled(
                            format!("    {line}"),
                            Style::default()
                                .fg(theme.muted().ratatui())
                                .add_modifier(Modifier::DIM),
                        )));
                    }
                }
            }
        }
    }
    out
}

/// Composed-row height of one source line at `width`, measured with ratatui's
/// own `WordWrapper` (via `Paragraph::line_count`) so the scroll math matches
/// exactly what the transcript renderer draws. Zero width composes to nothing.
/// Measuring a single line at a time is what lets [`visible_window`] stay
/// O(visible rows) instead of composing the whole transcript every frame.
pub(super) fn wrapped_height(line: &Line, width: u16) -> usize {
    if width < 1 {
        return 0;
    }
    // No block on the measuring paragraph: `line_count` would otherwise add the
    // border rows we are not measuring here.
    Paragraph::new(Text::from(line.clone()))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Locate the visible window inside a variable-height line list, counting from
/// the bottom where the newest content lives.
///
/// Returns `(start_src, sub_offset)`: the source index to slice from, and the
/// number of composed rows to skip *within* that first source line. `height_at`
/// lazily yields each source line's composed height, so near the bottom this
/// touches only the visible rows; scrolling past the top clamps to `(0, 0)`.
/// Pure and allocation-free, so it is unit-testable without a terminal.
///
/// `scroll_up` is in composed rows: 0 pins the newest row to the viewport
/// bottom, each unit lifts the window one wrapped row.
pub(super) fn visible_window<F>(
    len: usize,
    height_at: F,
    body_height: usize,
    scroll_up: usize,
) -> (usize, usize)
where
    F: Fn(usize) -> usize,
{
    // Rows the window must span from the bottom: the visible body plus how
    // far we have scrolled up above it.
    let need = scroll_up.saturating_add(body_height);
    let mut acc = 0usize;
    for i in (0..len).rev() {
        // A blank source line still occupies one composed row, so the walk
        // never stalls on it.
        let h = height_at(i).max(1);
        if acc + h >= need {
            // This source line straddles the window's top edge; skip the rows
            // of it that sit above the viewport.
            return (i, (acc + h).saturating_sub(need));
        }
        acc += h;
    }
    // Scrolled further up than the transcript is tall: clamp to the very top.
    (0, 0)
}

/// A dimmed secondary line (notices, markers).
pub(super) fn muted_line(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default()
            .fg(Theme::current().muted().ratatui())
            .add_modifier(Modifier::DIM),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_lines_never_fold() {
        let mut item = Item::Line(Line::from("answer"));
        assert!(!item.is_process());
        assert!(!item.toggle());
        // The no-op must not mutate anything: the entry stays a plain line.
        assert!(matches!(item, Item::Line(_)));
    }

    #[test]
    fn process_entries_toggle_expanded_in_place() {
        let mut item = Item::Thinking {
            text: String::from("reasoning"),
            expanded: false,
        };
        assert!(item.is_process());
        assert!(item.toggle(), "a process entry folds");
        assert!(matches!(item, Item::Thinking { expanded: true, .. }));
        assert!(item.toggle(), "and toggles back");
        assert!(matches!(
            item,
            Item::Thinking {
                expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn tool_entries_toggle_expanded_in_place() {
        let mut item = Item::Tool {
            name: String::from("bash"),
            is_error: false,
            output: vec![String::from("out")],
            expanded: false,
        };
        assert!(item.is_process());
        assert!(item.toggle());
        assert!(matches!(item, Item::Tool { expanded: true, .. }));
    }

    #[test]
    fn visible_window_counts_from_the_bottom() {
        let heights = [1, 1, 1];
        let height_at = |i: usize| heights[i];
        // 0 scroll pins the newest two rows: the window starts at source 1.
        assert_eq!(visible_window(3, height_at, 2, 0), (1, 0));
        // Scrolled further up than the transcript is tall: clamp to the top.
        assert_eq!(visible_window(3, height_at, 3, 99), (0, 0));
    }

    #[test]
    fn visible_window_straddles_a_tall_source_line() {
        let heights = [3, 2];
        let height_at = |i: usize| heights[i];
        // need = 1 scroll row + 3 body rows = 4: source 1 gives 2, source 0
        // straddles the top edge, skipping the 1 row that sits above it.
        assert_eq!(visible_window(2, height_at, 3, 1), (0, 1));
    }

    #[test]
    fn visible_window_never_stalls_on_blank_lines() {
        // A blank source line still occupies one composed row (`max(1)`), so
        // the bottom-up walk reaches it instead of running past the top.
        let height_at = |_: usize| 0;
        assert_eq!(visible_window(2, height_at, 1, 0), (1, 0));
    }

    #[test]
    fn wrapped_height_degenerates_to_zero_at_zero_width() {
        let row = Line::from("hello world");
        assert_eq!(wrapped_height(&row, 0), 0);
        assert_eq!(wrapped_height(&Line::from(""), 1), 1);
        assert_eq!(wrapped_height(&Line::from("hi"), 4), 1);
    }
}
