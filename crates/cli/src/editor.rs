//! REPL input. P4-c ("all-in on ratatui") swapped the reedline line editor for
//! a single ratatui + tui-textarea inline viewport: the editor owns stdin only
//! while idle and hands the REPL loop exactly one submitted line per call, so
//! [`ReplEditor::read_line`] keeps the same contract the REPL was written
//! against. Rendering stays in the *normal* screen buffer (an inline viewport,
//! not the alternate screen) so native terminal scrollback survives, and the
//! real terminal cursor is parked on the input cell so the OS/terminal draws
//! CJK IME candidates in place (self-drawn candidates are not viable).
//!
//! Slash completion is a dropdown menu: typing `/` lists the matching commands
//! below the prompt (fish-style Tab still completes). A dropdown needs a viewport
//! whose height grows with the candidate list, which stock `Viewport::Inline`
//! cannot do without a full-screen clear; [`crate::viewport_term::CompanionTerminal`]
//! supplies a dynamic-height inline viewport (astrcodey/codex resize-reflow) for
//! exactly this, while the non-slash path keeps the stable 2-row viewport.

use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use tui_textarea::{CursorMove, TextArea};

use crate::mascot::Mascot;
use crate::theme::{glyphs, Theme};
use crate::viewport_term::CompanionTerminal;

/// REPL slash commands surfaced by completion and the "did you mean" hint.
/// Kept in sync with `print_repl_help` in main.rs; add new commands to both.
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show help"),
    ("/status", "Show session status"),
    ("/model [NAME]", "Show or switch the model"),
    ("/mode [NAME]", "Show or switch the permission mode"),
    (
        "/plan <GOAL>",
        "Plan first (writes gated to .heartflow/plans), then /plan approve",
    ),
    ("/compact", "Compact session history"),
    ("/pin", "Toggle never-compacted on the last message"),
    ("/save", "Persist the session now"),
    ("/clear", "Start a fresh session"),
    ("/sessions", "List saved sessions"),
    ("/search <Q>", "Full-text search saved conversation history"),
    ("/mcp", "List MCP servers and tools"),
    ("/expand [ID]", "Re-show a folded tool output"),
    (
        "/queue [pop|clear]",
        "Inspect/withdraw follow-ups queued during a turn",
    ),
    (
        "/guide <TASK>",
        "Assemble a prior-work/state/task draft to send",
    ),
    (
        "/init",
        "Scaffold a starting AGENTS.md in the current directory",
    ),
    ("/restart", "Re-launch with a fresh config/MCP load"),
    ("/exit", "Quit the REPL"),
];

/// Width of the prompt gutter (`› `). The input cell starts just past it so the
/// real cursor sits between the gutter and the first typed glyph. Sourced from
/// the shared theme so the echoed prompt and the live gutter never disagree.
const PROMPT_WIDTH: u16 = glyphs::PROMPT_WIDTH;
/// Fixed inline viewport: one input row + one hint/completion row.
const VIEWPORT_ROWS: u16 = 2;
/// Cap on retained history entries; the oldest are dropped first.
const HISTORY_MAX: usize = 500;
/// Max command suggestions the hint tracks (Tab cycles through them).
const MAX_COMPLETIONS: usize = 8;
/// Idle animation cadence for the mascot badge. A slow tick keeps the blink
/// natural while waking the loop rarely; ratatui's diff makes an unchanged
/// redraw near-free, so idle CPU stays negligible. 80ms (~12fps) tracks the
/// spring smoothly for blinks and the Done pop without spinning the loop hot.
const MASCOT_TICK: Duration = Duration::from_millis(80);
/// Minimum terminal width before the mascot reserves room on the input row; if
/// the window is narrower the badge is hidden rather than crowd the prompt.
const MASCOT_MIN_WIDTH: u16 = 40;

/// Closest known slash command to `input`, judged on its leading token, when one
/// is similar enough to be worth suggesting. Backs the "did you mean" hint so a
/// mistyped `/commnd` is caught instead of silently sent to the model as a turn.
#[must_use]
pub fn suggest_command(input: &str) -> Option<&'static str> {
    let typed = input.split_whitespace().next()?;
    // Ignore short or non-slash input: no useful similarity signal.
    if !typed.starts_with('/') || typed.chars().count() < 3 {
        return None;
    }
    let mut best: Option<(&'static str, f64)> = None;
    for (name, _) in COMMANDS {
        let head = name.split([' ', '<']).next().unwrap_or(name);
        let score = strsim::jaro_winkler(typed, head);
        if score >= 0.82 && best.is_none_or(|(_, top)| score > top) {
            best = Some((head, score));
        }
    }
    best.map(|(name, _)| name)
}

/// Commands whose leading token starts with `prefix` (the token currently being
/// typed), for the completion hint / Tab cycling.
#[must_use]
pub fn completion_candidates(prefix: &str) -> Vec<&'static str> {
    if !prefix.starts_with('/') || prefix.contains(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter_map(|(name, _)| {
            let head = name.split([' ', '<']).next().unwrap_or(name);
            head.starts_with(prefix).then_some(head)
        })
        .collect()
}

/// One-line description for a command head name, for the dropdown rows.
#[must_use]
fn command_desc(name: &str) -> &'static str {
    COMMANDS
        .iter()
        .find(|(full, _)| full.split([' ', '<']).next().unwrap_or(full) == name)
        .map_or("", |(_, desc)| *desc)
}

/// The scrollback lines a submitted input leaves behind: the first line is
/// prefixed with the prompt gutter, continuations are aligned under it. ASCII
/// gutter, matching the historical reedline prompt so transcripts look alike.
#[must_use]
pub fn echo_lines(text: &str) -> Vec<String> {
    let mut lines = text.split('\n');
    let mut out = Vec::new();
    if let Some(first) = lines.next() {
        out.push(format!("{}{first}", glyphs::PROMPT));
    }
    for rest in lines {
        out.push(format!("  {rest}"));
    }
    out
}

/// Persistent REPL history: an in-memory list backed by an append-only file so a
/// bad write can never wipe existing entries. Navigation is a cursor over the
/// list; the cursor resting past the end means "the draft you are typing".
#[derive(Debug)]
struct History {
    entries: Vec<String>,
    cursor: usize,
}

impl History {
    fn load(path: &Path) -> Self {
        let entries = std::fs::read_to_string(path)
            .map(|raw| decode_history(&raw, HISTORY_MAX))
            .unwrap_or_default();
        let cursor = entries.len();
        Self { entries, cursor }
    }

    /// Move to an older entry; returns it for the caller to load into the
    /// buffer. Clamps at the oldest entry.
    fn prev(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.cursor > 0 {
            self.cursor -= 1;
        }
        self.entries.get(self.cursor).map(String::as_str)
    }

    /// Move to a newer entry; `None` once past the end means back to the empty
    /// draft the user was typing.
    fn next(&mut self) -> Option<&str> {
        if self.cursor >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        self.entries.get(self.cursor).map(String::as_str)
    }

    fn reset_cursor(&mut self) {
        self.cursor = self.entries.len();
    }

    /// Record a submitted line: appended to the file (escaped onto one physical
    /// line) and pushed in memory, capped at `HISTORY_MAX`.
    fn push(&mut self, path: &Path, text: &str) {
        let trimmed = text.trim_end();
        if trimmed.is_empty() || self.entries.last().is_some_and(|last| last == trimmed) {
            self.reset_cursor();
            return;
        }
        if let Err(error) = append_history_line(path, trimmed) {
            // A history write failure must never break the session; the entry
            // still lives in memory for this run.
            tracing::warn!("history append failed: {error}");
        }
        self.entries.push(trimmed.to_string());
        if self.entries.len() > HISTORY_MAX {
            let excess = self.entries.len() - HISTORY_MAX;
            self.entries.drain(0..excess);
        }
        self.reset_cursor();
    }
}

/// Encode one entry onto a single physical line: backslash and newline are
/// escaped so multi-line submissions round-trip through the file.
#[must_use]
fn encode_history_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Decode a whole history file, one entry per physical line. Blank and legacy
/// reedline separator lines (`*`, `+`, `#...` headers) are skipped so importing
/// an old history file is harmless. Keeps the most recent `max` entries.
#[must_use]
fn decode_history(raw: &str, max: usize) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line == "*" || line == "+" || line.starts_with('#') {
            continue;
        }
        entries.push(decode_history_line(line));
    }
    if entries.len() > max {
        let excess = entries.len() - max;
        entries.drain(0..excess);
    }
    entries
}

fn decode_history_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') | None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

fn append_history_line(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", encode_history_line(text))?;
    file.flush()
}

/// How an editing session ended.
enum Exit {
    Submit(String),
    Quit,
}

pub struct ReplEditor {
    history_path: PathBuf,
    history: History,
}

impl ReplEditor {
    /// Build the editor over a history file. A missing/unreadable file degrades
    /// to empty in-memory history rather than blocking startup.
    #[must_use]
    pub fn new(history_path: &Path) -> Self {
        Self {
            history_path: history_path.to_path_buf(),
            history: History::load(history_path),
        }
    }

    /// Read one REPL line. `Ok(None)` means quit (Ctrl+D on an empty line);
    /// Ctrl+C clears the line rather than exiting. Non-TTY stdin (pipes, tests)
    /// falls back to plain line reads so scripted input keeps working.
    pub fn read_line(&mut self) -> io::Result<Option<String>> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return read_line_fallback();
        }
        match self.read_line_tui()? {
            Exit::Submit(text) => Ok(Some(text)),
            Exit::Quit => Ok(None),
        }
    }

    fn read_line_tui(&mut self) -> io::Result<Exit> {
        enable_raw_mode()?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = match CompanionTerminal::with_inline(backend, VIEWPORT_ROWS) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                return Err(error);
            }
        };

        let mut textarea = blank_textarea();
        let mut status: Option<String> = None;
        // Tab-cycling state: candidate list + which one is currently offered.
        let mut completions: Vec<&'static str> = Vec::new();
        let mut completion_idx: Option<usize> = None;
        let mut exit = Exit::Quit;

        let loop_result = self.edit_loop(
            &mut terminal,
            &mut textarea,
            &mut status,
            &mut completions,
            &mut completion_idx,
            &mut exit,
        );

        // Teardown: erase the viewport, restore cooked mode, then hand the
        // scrollback record of the submitted line back to the normal buffer.
        let _ = terminal.hide_cursor();
        let _ = terminal.clear();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show);
        loop_result?;

        if let Exit::Submit(ref text) = exit {
            if !text.trim().is_empty() {
                self.history.push(&self.history_path, text);
            }
            for line in echo_lines(text) {
                println!("{line}");
            }
        }
        Ok(exit)
    }

    /// The blocking key/edit loop for one line. Runs the terminal in raw mode;
    /// each iteration redraws, anchors the real cursor, then blocks on the next
    /// event. Returns once `exit` is set (submit or quit).
    #[allow(clippy::too_many_arguments)]
    fn edit_loop(
        &mut self,
        terminal: &mut CompanionTerminal<CrosstermBackend<io::Stdout>>,
        textarea: &mut TextArea<'static>,
        status: &mut Option<String>,
        completions: &mut Vec<&'static str>,
        completion_idx: &mut Option<usize>,
        exit: &mut Exit,
    ) -> io::Result<()> {
        let mut mascot = Mascot::new();
        loop {
            let text = textarea.lines().join("\n");
            let prefix = current_token_prefix(&text);
            let hint = build_hint(
                prefix.as_deref(),
                completions,
                completion_idx,
                status.take().as_deref(),
            );

            // The dropdown lives in rows below the fixed input+hint viewport; only
            // the slash path grows the height, so ordinary typing keeps it at 2 and
            // behaves exactly like the old stable inline viewport.
            let menu_rows = if prefix.is_some() {
                completions.len()
            } else {
                0
            };
            let height = VIEWPORT_ROWS.saturating_add(u16::try_from(menu_rows).unwrap_or(0));
            terminal.draw(height, |buf, area| {
                draw_frame(
                    buf,
                    area,
                    textarea,
                    &hint,
                    &mascot,
                    completions,
                    *completion_idx,
                )
            })?;

            // Poll instead of a blocking read so the mascot can tick while the
            // user is idle; on timeout we advance the animation and redraw
            // (ratatui diffs, so an unchanged frame emits nothing).
            if !event::poll(MASCOT_TICK)? {
                mascot.advance(MASCOT_TICK.as_secs_f64());
                continue;
            }
            let key = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => key,
                _ => continue,
            };
            if self.handle_key(key, textarea, completions, completion_idx, status, exit) {
                return Ok(());
            }
        }
    }

    /// Handle one key press; returns `true` when the edit session should end.
    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        textarea: &mut TextArea<'static>,
        completions: &mut Vec<&'static str>,
        completion_idx: &mut Option<usize>,
        status: &mut Option<String>,
        exit: &mut Exit,
    ) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    // Ctrl+C clears the draft (old reedline hint); interrupting a
                    // running turn is wired separately in the REPL.
                    *textarea = blank_textarea();
                    completions.clear();
                    *completion_idx = None;
                    self.history.reset_cursor();
                    *status = Some(String::from(
                        "^C line cleared. Ctrl+C interrupts a running turn; /exit or Ctrl+D quits.",
                    ));
                    false
                }
                KeyCode::Char('d') => {
                    if textarea.is_empty() {
                        *exit = Exit::Quit;
                        return true;
                    }
                    textarea.delete_next_char();
                    false
                }
                KeyCode::Char('j') => {
                    textarea.insert_newline();
                    *completion_idx = None;
                    false
                }
                _ => {
                    textarea.input(key);
                    *completion_idx = None;
                    false
                }
            }
        } else {
            match key.code {
                // Bare Enter sends; Shift/Alt+Enter inserts a line.
                KeyCode::Enter => {
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                    {
                        textarea.insert_newline();
                        false
                    } else if menu_accept_on_enter(
                        completions,
                        *completion_idx,
                        &textarea.lines().join("\n"),
                    )
                    .is_some()
                    {
                        // A dropdown row is highlighted and differs from what is
                        // typed: Enter completes it rather than sending a partial.
                        complete_menu(textarea, completions, completion_idx);
                        false
                    } else {
                        *exit = Exit::Submit(textarea.lines().join("\n"));
                        true
                    }
                }
                KeyCode::Tab => {
                    if let Some(candidate) = accept_completion(completions, completion_idx) {
                        *textarea = blank_textarea();
                        textarea.insert_str(format!("{candidate} "));
                    }
                    false
                }
                // Esc only closes the completion hint; it never cancels a turn
                // (turn cancellation is double-Ctrl+C, wired later).
                KeyCode::Esc => {
                    *completion_idx = None;
                    false
                }
                KeyCode::Up => {
                    if !completions.is_empty() {
                        cycle_completion(completions, completion_idx, true);
                    } else if textarea.cursor().0 == 0 {
                        if let Some(entry) = self.history.prev() {
                            *textarea = textarea_from(entry);
                        }
                    } else {
                        textarea.input(key);
                    }
                    false
                }
                KeyCode::Down => {
                    if !completions.is_empty() {
                        cycle_completion(completions, completion_idx, false);
                    } else if textarea.cursor().0 + 1 >= textarea.lines().len() {
                        *textarea = match self.history.next() {
                            Some(entry) => textarea_from(entry),
                            None => blank_textarea(),
                        };
                    } else {
                        textarea.input(key);
                    }
                    false
                }
                _ => {
                    textarea.input(key);
                    // Editing the token invalidates the offered completion so the
                    // hint recomputes next frame.
                    *completion_idx = None;
                    false
                }
            }
        }
    }
}

/// Render one frame into the viewport buffer: the `› ` gutter, the textarea in
/// the remaining input cells, the idle mascot badge parked at the far right of
/// the input row, the hint row beneath, and (when a `/` prefix is active) the
/// command dropdown below that. Returns the absolute terminal cursor position so
/// the caller can park the real cursor on the input cell (the IME anchor). Rect
/// math is done by hand to avoid coupling to the layout-algorithm API.
// The render-closure contract returns `Option` (`None` hides the cursor), even
// though this frame always parks it on the input cell.
#[allow(clippy::unnecessary_wraps)]
fn draw_frame(
    buf: &mut Buffer,
    area: Rect,
    textarea: &TextArea<'static>,
    hint: &str,
    mascot: &Mascot,
    completions: &[&'static str],
    completion_idx: Option<usize>,
) -> Option<Position> {
    let theme = Theme::current();
    let size = area;
    let gutter = Rect::new(size.x, size.y, PROMPT_WIDTH.min(size.width), 1);
    // The mascot owns a few columns at the right of the input row; hide it on
    // narrow terminals rather than crowd the prompt, and never let it eat the
    // whole line (cap the reserve against the available input width).
    let badge = mascot.badge();
    let badge_width = u16::try_from(badge.chars().count()).unwrap_or(0);
    let show_mascot = size.width >= MASCOT_MIN_WIDTH;
    let reserve = if show_mascot {
        (badge_width + 2).min(size.width.saturating_sub(PROMPT_WIDTH + 4))
    } else {
        0
    };
    let editor = Rect::new(
        size.x + PROMPT_WIDTH,
        size.y,
        size.width.saturating_sub(PROMPT_WIDTH + reserve),
        1,
    );
    let hint_area = Rect::new(size.x, size.y + 1, size.width, 1);
    Paragraph::new(Line::from(Span::styled(
        glyphs::PROMPT,
        Style::default().fg(theme.accent().ratatui()),
    )))
    .render(gutter, buf);
    textarea.render(editor, buf);
    if show_mascot {
        let badge_area = Rect::new(
            size.x + size.width.saturating_sub(badge_width),
            size.y,
            badge_width,
            1,
        );
        Paragraph::new(Line::from(Span::styled(
            badge,
            Style::default().fg(theme.muted().ratatui()),
        )))
        .render(badge_area, buf);
    }
    Paragraph::new(Line::from(Span::styled(
        hint.to_string(),
        Style::default().fg(theme.muted().ratatui()),
    )))
    .render(hint_area, buf);

    render_menu(buf, size, completions, completion_idx, theme);

    // Park the real terminal cursor on the input cell (the IME anchor).
    let (cursor_row, cursor_col) = textarea.cursor();
    let column = if cursor_row == 0 {
        u16::try_from(cursor_col).unwrap_or(u16::MAX)
    } else {
        0
    };
    let x = (size.x + PROMPT_WIDTH + column).min(size.x + size.width.saturating_sub(1));
    Some(Position::new(x, size.y))
}

/// Draw the slash command dropdown: one row per candidate, the selected row
/// marked and accented, each command's description in muted text. The rows are
/// reserved by [`ReplEditor::edit_loop`] via the viewport height, so this only
/// paints while the list is actually shown.
fn render_menu(
    buf: &mut Buffer,
    size: Rect,
    completions: &[&'static str],
    completion_idx: Option<usize>,
    theme: &Theme,
) {
    let menu_top = size.y + VIEWPORT_ROWS;
    let name_col = completions
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0)
        .saturating_add(2);
    for (i, name) in completions.iter().enumerate() {
        let Some(row_y) = menu_top.checked_add(u16::try_from(i).unwrap_or(u16::MAX)) else {
            break;
        };
        if row_y >= size.y + size.height {
            break; // viewport was clamped to the screen; drop overflow rows
        }
        let row = Rect::new(size.x, row_y, size.width, 1);
        let is_sel = completion_idx == Some(i);
        let name_style = if is_sel {
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Reset)
        };
        let pad = name_col.saturating_sub(name.chars().count());
        let line = Line::from(vec![
            Span::styled(if is_sel { "> " } else { "  " }, name_style),
            Span::styled((*name).to_string(), name_style),
            Span::styled(" ".repeat(pad), Style::default()),
            Span::styled(
                command_desc(name).to_string(),
                Style::default().fg(theme.muted().ratatui()),
            ),
        ]);
        Paragraph::new(line).render(row, buf);
    }
}

/// The slash token typed so far, but only while it is a bare leading command
/// being completed (no argument begun). `None` otherwise.
fn current_token_prefix(text: &str) -> Option<String> {
    if !text.starts_with('/') || text.contains(char::is_whitespace) {
        return None;
    }
    Some(text.to_string())
}

/// Build the hint row: a transient status message wins, then slash completion
/// feedback, else the idle key legend. Also refreshes the Tab-candidate set.
fn build_hint(
    prefix: Option<&str>,
    completions: &mut Vec<&'static str>,
    completion_idx: &mut Option<usize>,
    status: Option<&str>,
) -> String {
    if let Some(status) = status {
        return status.to_string();
    }
    if let Some(prefix) = prefix {
        // Recompute candidates only when the prefix stops matching the
        // current set, so repeated keystrokes stay stable.
        let still_valid = completions
            .first()
            .is_some_and(|first| first.starts_with(prefix));
        if !still_valid {
            *completions = completion_candidates(prefix);
            completions.truncate(MAX_COMPLETIONS);
            *completion_idx = None;
        }
        if completions.is_empty() {
            return format!("no command matches {prefix}");
        }
        let shown = completion_idx
            .and_then(|idx| completions.get(idx).copied())
            .unwrap_or_else(|| completions.first().copied().unwrap_or(prefix));
        let count = completions.len();
        let pos = completion_idx.map_or(1, |idx| idx + 1);
        format!("{shown}  ({pos}/{count}) · Tab completes")
    } else {
        // Leaving the slash prefix closes the menu: drop stale candidates so
        // the viewport height and dropdown rows follow the current input.
        completions.clear();
        *completion_idx = None;
        String::from(
            "Enter send · Alt+Enter newline · / commands · ↑/↓ select · Tab complete · Ctrl+D quit",
        )
    }
}

/// Complete the highlighted dropdown row into the buffer and close the menu
/// (Enter while a row is actively selected).
fn complete_menu(
    textarea: &mut TextArea<'static>,
    completions: &mut Vec<&'static str>,
    completion_idx: &mut Option<usize>,
) {
    if let Some(candidate) = accept_completion(completions, completion_idx) {
        *textarea = blank_textarea();
        textarea.insert_str(format!("{candidate} "));
    }
    completions.clear();
    *completion_idx = None;
}

/// Whether Enter should complete a highlighted dropdown row instead of
/// submitting: only when the menu is up, a row is actively highlighted (via
/// Up/Down/Tab), and that candidate is not already the whole typed token. A
/// `None` return means Enter submits the line exactly as typed.
#[must_use]
fn menu_accept_on_enter(
    completions: &[&'static str],
    completion_idx: Option<usize>,
    text: &str,
) -> Option<&'static str> {
    if !text.starts_with('/') || text.contains(char::is_whitespace) {
        return None;
    }
    let candidate = completion_idx.and_then(|i| completions.get(i).copied())?;
    (candidate != text).then_some(candidate)
}

/// Take the currently offered completion and advance the cycle to the next one.
fn accept_completion(
    completions: &[&'static str],
    completion_idx: &mut Option<usize>,
) -> Option<&'static str> {
    if completions.is_empty() {
        return None;
    }
    let idx = completion_idx.get_or_insert(0);
    let chosen = completions.get(*idx).copied();
    cycle_completion(completions, completion_idx, false);
    chosen
}

fn cycle_completion(
    completions: &[&'static str],
    completion_idx: &mut Option<usize>,
    backward: bool,
) {
    if completions.is_empty() {
        return;
    }
    let len = completions.len();
    let next = match *completion_idx {
        None => {
            if backward {
                len - 1
            } else {
                0
            }
        }
        Some(idx) => {
            if backward {
                (idx + len - 1) % len
            } else {
                (idx + 1) % len
            }
        }
    };
    *completion_idx = Some(next);
}

fn blank_textarea() -> TextArea<'static> {
    configure_textarea(TextArea::default())
}

fn textarea_from(text: &str) -> TextArea<'static> {
    let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut textarea = configure_textarea(TextArea::from(lines));
    textarea.move_cursor(CursorMove::Bottom);
    textarea.move_cursor(CursorMove::End);
    textarea
}

fn configure_textarea(mut textarea: TextArea<'static>) -> TextArea<'static> {
    // A single-row window scrolls to keep the cursor visible; drop the default
    // cursor-line highlight so the inline input reads like a plain prompt.
    textarea.set_cursor_line_style(Style::default());
    textarea.set_style(Style::default().fg(Color::Reset));
    textarea
}

/// Non-interactive path for piped stdin (tests, `echo ... | hf`): plain line
/// reads, matching the historical behavior exactly.
fn read_line_fallback() -> io::Result<Option<String>> {
    let mut stdout = io::stdout();
    write!(stdout, "{}", glyphs::PROMPT)?;
    stdout.flush()?;
    let mut buffer = String::new();
    if io::stdin().read_line(&mut buffer)? == 0 {
        return Ok(None);
    }
    while matches!(buffer.chars().last(), Some('\n' | '\r')) {
        buffer.pop();
    }
    Ok(Some(buffer))
}

#[cfg(test)]
mod tests {
    use super::{
        command_desc, completion_candidates, decode_history, echo_lines, encode_history_line,
        menu_accept_on_enter, suggest_command, History,
    };

    #[test]
    fn enter_completes_only_an_active_highlight() {
        let cands = completion_candidates("/comp");
        assert_eq!(cands, vec!["/compact"]);
        // No row highlighted yet: Enter submits the partial rather than completing.
        assert_eq!(menu_accept_on_enter(&cands, None, "/comp"), None);
        // A highlighted row that differs from the token: Enter completes it.
        assert_eq!(
            menu_accept_on_enter(&cands, Some(0), "/comp"),
            Some("/compact")
        );
        // The exact command already typed: Enter submits (nothing to complete).
        let exact = completion_candidates("/compact");
        assert_eq!(menu_accept_on_enter(&exact, Some(0), "/compact"), None);
        // Once an argument has begun the menu is closed, so Enter submits.
        assert_eq!(menu_accept_on_enter(&cands, Some(0), "/comp x"), None);
    }

    #[test]
    fn command_desc_resolves_heads_only() {
        assert_eq!(command_desc("/compact"), "Compact session history");
        assert_eq!(command_desc("/nope"), "");
    }

    #[test]
    #[allow(clippy::assert_is_empty)]
    fn completions_match_prefix_and_ignore_args() {
        assert!(completion_candidates("/comp").contains(&"/compact"));
        assert!(completion_candidates("/que").contains(&"/queue"));
        assert!(completion_candidates("/sessions").contains(&"/sessions"));
        // Once an argument has begun, command completion stops.
        assert!(completion_candidates("/model gpt").is_empty());
        assert!(completion_candidates("hello").is_empty());
    }

    #[test]
    fn suggest_catches_typos_only() {
        assert_eq!(suggest_command("/compct"), Some("/compact"));
        assert_eq!(suggest_command("/m"), None, "too short to judge");
        assert_eq!(suggest_command("just text"), None);
    }

    #[test]
    fn echo_prefixes_first_and_continuations() {
        assert_eq!(echo_lines("hi"), vec!["› hi".to_string()]);
        assert_eq!(
            echo_lines("a\nb"),
            vec!["› a".to_string(), "  b".to_string()]
        );
    }

    #[test]
    fn history_roundtrips_multiline_and_skips_legacy() {
        let raw = "# reedline header\nfirst entry\n*\nsecond \\n line\n\n";
        let entries = decode_history(raw, 500);
        assert_eq!(
            entries,
            vec!["first entry".to_string(), "second \n line".to_string()]
        );
        let tricky = "path C:\\temp\nsecond line";
        let one_line = encode_history_line(tricky);
        assert!(!one_line.contains('\n'), "entry must fit one physical line");
        assert_eq!(decode_history(&one_line, 500), vec![tricky.to_string()]);
    }

    #[test]
    fn history_navigation_walks_and_returns_to_draft() {
        let mut h = History {
            entries: vec!["one".into(), "two".into()],
            cursor: 2,
        };
        assert_eq!(h.prev(), Some("two"));
        assert_eq!(h.prev(), Some("one"));
        assert_eq!(h.prev(), Some("one"), "clamps at the oldest");
        assert_eq!(h.next(), Some("two"));
        assert_eq!(h.next(), None, "past newest means the draft");
        h.reset_cursor();
        assert_eq!(h.cursor, 2);
    }
}
