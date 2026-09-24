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
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
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
    ("/focus", "Toggle focus mode (hide mascot, minimal chrome)"),
    ("/save", "Persist the session now"),
    ("/clear", "Start a fresh session"),
    ("/sessions", "List saved sessions"),
    ("/open <N>", "Jump the live REPL back to saved session N"),
    (
        "/remember <T>",
        "Persist a durable pitfall/preference to MEMORY.md",
    ),
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

/// `/plan` sub-actions offered at the argument position. The plan goal itself is
/// free text, so only these fixed verbs are enumerable.
const PLAN_ACTIONS: &[(&str, &str)] = &[
    (
        "approve",
        "Load the approved plan into the task list and execute it",
    ),
    ("end", "Leave planning without executing"),
    ("status", "Show planning state and the plan file"),
];

/// `/queue` sub-actions offered at the argument position.
const QUEUE_ACTIONS: &[(&str, &str)] = &[
    ("pop", "Withdraw the last queued follow-up"),
    ("clear", "Drop every queued follow-up"),
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
/// Idle animation cadence for the mascot badge. A slow tick keeps the beat
/// natural while waking the loop rarely; ratatui's diff makes an unchanged
/// redraw near-free, so idle CPU stays negligible. 80ms (~12fps) tracks the
/// spring smoothly for beats and the Done pop without spinning the loop hot.
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

/// One dropdown row: the text Tab/Enter inserts (`label`) plus a muted second
/// column (`detail`) — a command's description, or a model's context window and
/// provenance. Owned (not `&'static str`) so dynamic values from the catalog
/// render alongside the fixed command heads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionItem {
    pub(crate) label: String,
    pub(crate) detail: String,
}

impl CompletionItem {
    pub(crate) fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
        }
    }
}

/// The per-turn completion universe the REPL hands the editor: the active
/// provider's catalog models, the permission modes, recent session ordinals, and
/// the currently-active model/mode (tagged `(current)` in the dropdown so the
/// live value is obvious). Plain, surface-agnostic data so [`complete`] stays a
/// pure function the TUI can reuse later and tests can drive without a terminal.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompletionContext {
    pub(crate) models: Vec<CompletionItem>,
    pub(crate) modes: Vec<CompletionItem>,
    pub(crate) sessions: Vec<CompletionItem>,
    pub(crate) current_model: Option<String>,
    pub(crate) current_mode: Option<String>,
}

/// Where completion applies in the current input: the command head itself, or
/// the single argument of a chosen command. Multi-line drafts and text past a
/// first argument (a `/search` query, a `/remember` note) yield no site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionSite<'a> {
    Command { prefix: &'a str },
    Argument { command: &'a str, arg: &'a str },
}

impl<'a> CompletionSite<'a> {
    /// The noun for the "no matches" hint, per command.
    fn noun(self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::Argument { command, .. } => match command {
                "/model" => "model",
                "/mode" => "mode",
                "/open" => "session",
                "/plan" | "/queue" => "action",
                _ => "match",
            },
        }
    }

    /// The token being completed: the command prefix or the partial argument.
    fn typed(self) -> &'a str {
        match self {
            Self::Command { prefix } => prefix,
            Self::Argument { arg, .. } => arg,
        }
    }
}

/// Parse `text` into a completion site, or `None` when completion does not apply
/// (no leading `/`, a multi-line draft, or more than one argument token).
fn completion_site(text: &str) -> Option<CompletionSite<'_>> {
    if !text.starts_with('/') || text.contains('\n') {
        return None;
    }
    match text.split_once(char::is_whitespace) {
        None => Some(CompletionSite::Command { prefix: text }),
        // A second whitespace run means free text (a search query, a note), not
        // an enumerable argument, so stop offering completions past the first.
        Some((command, rest)) => (!rest.contains(char::is_whitespace))
            .then_some(CompletionSite::Argument { command, arg: rest }),
    }
}

/// The dropdown candidates for `text`: command heads while the head is typed, or
/// the chosen command's argument pool filtered by the partial argument
/// (`/model` → catalog models, `/mode` → permission modes, `/open` → sessions,
/// `/plan` and `/queue` → their fixed sub-actions). Candidates are ranked by a
/// lightweight fuzzy score and the active model/mode is tagged `(current)`.
/// Pure and surface-agnostic; the REPL supplies `ctx` fresh each turn.
#[must_use]
pub(crate) fn complete(text: &str, ctx: &CompletionContext) -> Vec<CompletionItem> {
    match completion_site(text) {
        None => Vec::new(),
        Some(CompletionSite::Command { prefix }) => command_completions(prefix),
        Some(CompletionSite::Argument { command, arg }) => match command {
            "/model" => ranked_matches(
                ctx.models
                    .iter()
                    .map(|item| (item.label.as_str(), item.detail.as_str())),
                arg,
                ctx.current_model.as_deref(),
            ),
            "/mode" => ranked_matches(
                ctx.modes
                    .iter()
                    .map(|item| (item.label.as_str(), item.detail.as_str())),
                arg,
                ctx.current_mode.as_deref(),
            ),
            "/open" => ranked_matches(
                ctx.sessions
                    .iter()
                    .map(|item| (item.label.as_str(), item.detail.as_str())),
                arg,
                None,
            ),
            "/plan" => ranked_matches(PLAN_ACTIONS.iter().copied(), arg, None),
            "/queue" => ranked_matches(QUEUE_ACTIONS.iter().copied(), arg, None),
            _ => Vec::new(),
        },
    }
}

/// Command heads matching `prefix`, each paired with its description, ranked by
/// fuzzy score so a closer head sorts first.
fn command_completions(prefix: &str) -> Vec<CompletionItem> {
    ranked_matches(
        COMMANDS
            .iter()
            .map(|(name, desc)| (name.split([' ', '<']).next().unwrap_or(name), *desc)),
        prefix,
        None,
    )
}

/// Filter `(label, detail)` candidates to those fuzzy-matching `query`, tag the
/// `current` value's detail with `(current)`, and sort by descending score with
/// the original pool order breaking ties (so equal-quality matches keep the
/// catalog/help sequence rather than jittering).
fn ranked_matches<'a, I>(pool: I, query: &str, current: Option<&str>) -> Vec<CompletionItem>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut scored: Vec<(i64, usize, CompletionItem)> = pool
        .into_iter()
        .enumerate()
        .filter_map(|(index, (label, detail))| {
            fuzzy_score(label, query).map(|score| {
                let detail = if current == Some(label) {
                    if detail.is_empty() {
                        "(current)".to_string()
                    } else {
                        format!("{detail} · (current)")
                    }
                } else {
                    detail.to_string()
                };
                (score, index, CompletionItem::new(label, detail))
            })
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, item)| item).collect()
}

/// Lightweight fuzzy score, case-insensitive: `None` when `query` is not a
/// subsequence of `label`, otherwise a rank where an empty query matches every
/// label at 0, a query anchored at the label's head beats one whose first hit
/// sits deeper, and a longer contiguous run beats scattered letters. Pure so
/// `complete` stays surface-agnostic; ties fall back to pool order in
/// [`ranked_matches`], keeping the catalog/help sequence stable.
fn fuzzy_score(label: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let label_chars: Vec<char> = label.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut search_from = 0usize;
    let mut first_match: Option<usize> = None;
    let mut longest_run = 0i64;
    let mut run = 0i64;
    let mut prev_idx: Option<usize> = None;
    // Greedy left-to-right subsequence scan, recording where each query char
    // lands so an anchored prefix and a contiguous run can be rewarded.
    for query_char in query.chars().map(|c| c.to_ascii_lowercase()) {
        let matched =
            (search_from..label_chars.len()).find(|&idx| label_chars[idx] == query_char)?;
        search_from = matched + 1;
        if first_match.is_none() {
            first_match = Some(matched);
        }
        run = if prev_idx.is_some_and(|prev| prev + 1 == matched) {
            run + 1
        } else {
            1
        };
        longest_run = longest_run.max(run);
        prev_idx = Some(matched);
    }
    let mut score = 100 + longest_run * 10;
    match first_match {
        Some(0) => score += 100,
        Some(position) => score -= i64::try_from(position).unwrap_or(0),
        None => {}
    }
    Some(score)
}

/// The `[start, end)` candidate slice to render so `selected` stays visible
/// within `visible` rows, scrolling only when it would leave the window.
#[must_use]
fn menu_window(total: usize, visible: usize, selected: usize) -> (usize, usize) {
    if total == 0 || visible == 0 {
        return (0, 0);
    }
    let visible = visible.min(total);
    let mut start = 0;
    if selected >= visible {
        start = selected - visible + 1;
    }
    if start + visible > total {
        start = total - visible;
    }
    (start, start + visible)
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
/// Shared by the blocking REPL and the full-screen shell (both point it at
/// `~/.heartflow/history.txt`), so recall works identically on either surface.
#[derive(Debug)]
pub(crate) struct History {
    entries: Vec<String>,
    cursor: usize,
}

impl History {
    /// An empty, in-memory-only history (no backing file). Used where disk I/O
    /// must stay off (hermetic test construction) until a real path is set.
    pub(crate) fn empty() -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
        }
    }

    pub(crate) fn load(path: &Path) -> Self {
        let entries = std::fs::read_to_string(path)
            .map(|raw| decode_history(&raw, HISTORY_MAX))
            .unwrap_or_default();
        let cursor = entries.len();
        Self { entries, cursor }
    }

    /// Move to an older entry; returns it for the caller to load into the
    /// buffer. Clamps at the oldest entry.
    pub(crate) fn prev(&mut self) -> Option<&str> {
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
    pub(crate) fn next(&mut self) -> Option<&str> {
        if self.cursor >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        self.entries.get(self.cursor).map(String::as_str)
    }

    fn reset_cursor(&mut self) {
        self.cursor = self.entries.len();
    }

    /// Find the index of the next older entry (lower index) that starts with
    /// `prefix`, searching from `from_idx` (exclusive) going backward.
    pub(crate) fn search_prefix_backward(&self, prefix: &str, from_idx: usize) -> Option<usize> {
        if prefix.is_empty() {
            return None;
        }
        let start = from_idx.min(self.entries.len());
        (0..start)
            .rev()
            .find(|&i| self.entries[i].starts_with(prefix))
    }

    /// Find the index of the next newer entry (higher index) that starts with
    /// `prefix`, searching from `from_idx` (exclusive) going forward.
    pub(crate) fn search_prefix_forward(&self, prefix: &str, from_idx: usize) -> Option<usize> {
        if prefix.is_empty() {
            return None;
        }
        let start = from_idx.saturating_add(1);
        self.entries
            .iter()
            .enumerate()
            .skip(start)
            .find(|(_, entry)| entry.starts_with(prefix))
            .map(|(i, _)| i)
    }

    /// Get the entry at `idx`, if within bounds.
    pub(crate) fn entry_at(&self, idx: usize) -> Option<&str> {
        self.entries.get(idx).map(String::as_str)
    }

    /// Number of entries in the history.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Record a submitted line: appended to the file (escaped onto one physical
    /// line) and pushed in memory, capped at `HISTORY_MAX`. An empty `path`
    /// means in-memory only (hermetic construction), so no file is touched.
    pub(crate) fn push(&mut self, path: &Path, text: &str) {
        let trimmed = text.trim_end();
        if trimmed.is_empty() || self.entries.last().is_some_and(|last| last == trimmed) {
            self.reset_cursor();
            return;
        }
        if !path.as_os_str().is_empty() {
            if let Err(error) = append_history_line(path, trimmed) {
                // A history write failure must never break the session; the entry
                // still lives in memory for this run.
                tracing::warn!("history append failed: {error}");
            }
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
    /// The companion. Owned here (not per-line) so it survives across turns and
    /// remembers the last outcome; the REPL reports each turn via
    /// [`ReplEditor::note_turn`] and the badge reverts to idle on the next key.
    mascot: Mascot,
    /// When true, hide the mascot badge and reduce decorative chrome so the
    /// user can focus on the conversation. Toggled by `/focus`.
    focus_mode: bool,
    /// When true, skip idle mascot animations (no heartbeat / breathing)
    /// and show a static badge instead. Reduces visual flicker for users
    /// sensitive to motion or who prefer a calmer terminal.
    reduced_motion: bool,
}

impl ReplEditor {
    /// Build the editor over a history file. A missing/unreadable file degrades
    /// to empty in-memory history rather than blocking startup.
    #[must_use]
    pub fn new(history_path: &Path) -> Self {
        Self {
            history_path: history_path.to_path_buf(),
            history: History::load(history_path),
            mascot: Mascot::new(),
            focus_mode: false,
            reduced_motion: false,
        }
    }

    /// Toggle focus mode on/off. Returns the new state.
    pub fn toggle_focus(&mut self) -> bool {
        self.focus_mode = !self.focus_mode;
        self.focus_mode
    }

    /// Set reduced-motion mode. When true, the mascot stays static instead
    /// of idling with a heartbeat.
    pub fn set_reduced_motion(&mut self, enabled: bool) {
        self.reduced_motion = enabled;
    }

    /// Report the just-finished turn's outcome to the companion: `ok` picks the
    /// Done (success) vs Error badge, shown until the user types again. This is
    /// the only event->mood seam the REPL loop touches; how each mood *looks*
    /// is entirely [`crate::mascot`]'s concern.
    pub fn note_turn(&mut self, ok: bool) {
        self.mascot.note_turn(ok);
    }

    /// Read one REPL line. `Ok(None)` means quit (Ctrl+D on an empty line);
    /// Ctrl+C clears the line rather than exiting. Non-TTY stdin (pipes, tests)
    /// falls back to plain line reads so scripted input keeps working. `ctx`
    /// supplies this turn's argument-completion universe (models/modes/sessions).
    pub fn read_line(&mut self, ctx: &CompletionContext) -> io::Result<Option<String>> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return read_line_fallback();
        }
        match self.read_line_tui(ctx)? {
            Exit::Submit(text) => Ok(Some(text)),
            Exit::Quit => Ok(None),
        }
    }

    fn read_line_tui(&mut self, ctx: &CompletionContext) -> io::Result<Exit> {
        enable_raw_mode()?;
        // Bracketed paste makes the terminal hand over a pasted block as one
        // event, so an embedded newline stays a multi-line draft instead of
        // reading as a bare Enter that submits (the "paste auto-runs" bug).
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error);
        }
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
        let mut completions: Vec<CompletionItem> = Vec::new();
        let mut completion_idx: Option<usize> = None;
        // Prefix history search: (prefix, match_index) — shows a ghost suffix
        // inline when the user cycles history entries matching the typed prefix.
        let mut prefix_search: Option<(String, usize)> = None;
        let mut exit = Exit::Quit;

        let loop_result = self.edit_loop(
            &mut terminal,
            &mut textarea,
            &mut status,
            &mut completions,
            &mut completion_idx,
            &mut prefix_search,
            &mut exit,
            ctx,
        );

        // Teardown: erase the viewport, restore cooked mode, then hand the
        // scrollback record of the submitted line back to the normal buffer.
        let _ = terminal.hide_cursor();
        let _ = terminal.clear();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, Show);
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
        completions: &mut Vec<CompletionItem>,
        completion_idx: &mut Option<usize>,
        prefix_search: &mut Option<(String, usize)>,
        exit: &mut Exit,
        ctx: &CompletionContext,
    ) -> io::Result<()> {
        loop {
            let text = textarea.lines().join("\n");
            let hint = build_hint(
                &text,
                ctx,
                completions,
                completion_idx,
                status.take().as_deref(),
            );

            // The dropdown lives in rows below the fixed input+hint viewport; only
            // a completable position grows the height (capped at MAX_COMPLETIONS,
            // scrolling past that), so ordinary typing keeps the stable 2 rows and
            // behaves exactly like the old inline viewport.
            let menu_rows = if completion_site(&text).is_some() {
                completions.len().min(MAX_COMPLETIONS)
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
                    &self.mascot,
                    completions,
                    *completion_idx,
                    prefix_search.as_ref().and_then(|(prefix, idx)| {
                        self.history.entry_at(*idx).and_then(|entry| {
                            entry.strip_prefix(prefix.as_str())
                        })
                    }),
                    self.focus_mode,
                )
            })?;

            // Poll instead of a blocking read so the mascot can tick while the
            // user is idle. A tick that produces no visible change skips the
            // frame entirely: with the cursor parking deduped downstream, an
            // idle REPL emits zero escape sequences until the mascot actually
            // blinks — the root fix for the Windows Terminal cursor flicker.
            // When `reduced_motion` is on, skip the heartbeat loop and block
            // directly on input: no animation, no per-tick redraw cost.
            if !self.reduced_motion && !event::poll(MASCOT_TICK)? {
                let before = self.mascot.badge();
                self.mascot.advance(MASCOT_TICK.as_secs_f64());
                if self.mascot.badge() == before {
                    continue;
                }
            }
            let key = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => key,
                // A bracketed-paste block is inserted whole; its newlines stay
                // a multi-line draft rather than each firing a bare-Enter submit.
                Event::Paste(text) => {
                    self.mascot.resume_idle();
                    textarea.insert_str(text);
                    *completion_idx = None;
                    continue;
                }
                _ => continue,
            };
            // Any key means the user is engaging again: clear the lingering
            // Done/Error badge so the companion returns to its idle baseline.
            self.mascot.resume_idle();
            if self.handle_key(key, textarea, completions, completion_idx, prefix_search, status, exit) {
                return Ok(());
            }
        }
    }

    /// Handle one key press; returns `true` when the edit session should end.
    #[allow(clippy::too_many_arguments)]
    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        textarea: &mut TextArea<'static>,
        completions: &mut Vec<CompletionItem>,
        completion_idx: &mut Option<usize>,
        prefix_search: &mut Option<(String, usize)>,
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
                    *prefix_search = None;
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
                    *prefix_search = None;
                    false
                }
                KeyCode::Char('j') => {
                    textarea.insert_newline();
                    *completion_idx = None;
                    *prefix_search = None;
                    false
                }
                KeyCode::Char('u') => {
                    // Ctrl+U: clear the whole line silently (readline convention).
                    // Unlike Ctrl+C, no status hint — this is an intentional edit.
                    *textarea = blank_textarea();
                    completions.clear();
                    *completion_idx = None;
                    *prefix_search = None;
                    self.history.reset_cursor();
                    false
                }
                KeyCode::Char(']') => {
                    // Ctrl+]: prefix history search forward (older / next match).
                    // Grabs text before cursor as prefix, cycles through matches.
                    let (row, col) = textarea.cursor();
                    let line = textarea.lines().get(row).cloned().unwrap_or_default();
                    let prefix = line[..col].to_string();
                    if prefix.is_empty() {
                        *prefix_search = None;
                        return false;
                    }
                    let next_idx = match *prefix_search {
                        Some((ref p, idx)) if *p == prefix => {
                            // Continuing search: find next older match.
                            self.history.search_prefix_backward(&prefix, idx)
                        }
                        _ => {
                            // New search: start from most recent (end of list).
                            self.history
                                .search_prefix_backward(&prefix, self.history.len())
                        }
                    };
                    match next_idx {
                        Some(idx) => {
                            *prefix_search = Some((prefix, idx));
                        }
                        None => {
                            // No match — keep existing state if any, noop.
                        }
                    }
                    false
                }
                KeyCode::Char('[') => {
                    // Ctrl+[: prefix history search backward (newer / prev match).
                    // Only works when a prefix search is already active.
                    if let Some((ref prefix, idx)) = *prefix_search {
                        if let Some(prev_idx) =
                            self.history.search_prefix_forward(prefix, idx)
                        {
                            *prefix_search = Some((prefix.clone(), prev_idx));
                        }
                        // If no newer match, stay on current match (no wrap).
                    }
                    false
                }
                KeyCode::Left => {
                    move_word(textarea, true);
                    *completion_idx = None;
                    *prefix_search = None;
                    false
                }
                KeyCode::Right => {
                    move_word(textarea, false);
                    *completion_idx = None;
                    *prefix_search = None;
                    false
                }
                _ => {
                    textarea.input(key);
                    *completion_idx = None;
                    *prefix_search = None;
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
                    } else {
                        let text = textarea.lines().join("\n");
                        if menu_accept_on_enter(completions, *completion_idx, &text).is_some() {
                            // A dropdown row is highlighted and differs from what
                            // is typed: Enter completes it rather than sending a
                            // partial line.
                            complete_menu(textarea, completions, completion_idx, &text);
                            false
                        } else {
                            *exit = Exit::Submit(text);
                            true
                        }
                    }
                }
                KeyCode::Tab => {
                    let text = textarea.lines().join("\n");
                    if let Some(site) = completion_site(&text) {
                        if let Some(item) = accept_completion(completions, completion_idx) {
                            apply_completion(textarea, &item, site);
                        }
                    }
                    *prefix_search = None;
                    false
                }
                // Esc only closes the completion hint; it never cancels a turn
                // (turn cancellation is double-Ctrl+C, wired later).
                KeyCode::Esc => {
                    *completion_idx = None;
                    *prefix_search = None;
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
                    *prefix_search = None;
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
                    *prefix_search = None;
                    false
                }
                KeyCode::Right | KeyCode::End => {
                    // If a prefix search hint is active and cursor is at end of
                    // line, accept the hint by inserting the ghost suffix.
                    let accepted = if let Some((ref prefix, idx)) = *prefix_search {
                        let (row, col) = textarea.cursor();
                        let line = textarea.lines().get(row).cloned().unwrap_or_default();
                        if col == line.len() {
                            if let Some(entry) = self.history.entry_at(idx) {
                                if let Some(suffix) = entry.strip_prefix(prefix.as_str()) {
                                    // Only accept up to end of the first line of
                                    // the match (input is single-line here).
                                    let first_line_suffix = suffix
                                        .split_once('\n')
                                        .map(|(first, _)| first)
                                        .unwrap_or(suffix);
                                    textarea.insert_str(first_line_suffix);
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if accepted {
                        *prefix_search = None;
                        *completion_idx = None;
                    } else {
                        textarea.input(key);
                        *prefix_search = None;
                        *completion_idx = None;
                    }
                    false
                }
                _ => {
                    textarea.input(key);
                    // Editing the token invalidates the offered completion so the
                    // hint recomputes next frame.
                    *completion_idx = None;
                    *prefix_search = None;
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
#[allow(clippy::too_many_arguments, clippy::unnecessary_wraps)]
fn draw_frame(
    buf: &mut Buffer,
    area: Rect,
    textarea: &TextArea<'static>,
    hint: &str,
    mascot: &Mascot,
    completions: &[CompletionItem],
    completion_idx: Option<usize>,
    prefix_suffix: Option<&str>,
    focus_mode: bool,
) -> Option<Position> {
    let theme = Theme::current();
    let size = area;
    let gutter = Rect::new(size.x, size.y, PROMPT_WIDTH.min(size.width), 1);
    // The mascot owns a few columns at the right of the input row; hide it on
    // narrow terminals rather than crowd the prompt, and never let it eat the
    // whole line (cap the reserve against the available input width).
    let badge_rows = mascot.badge();
    let badge_width = badge_rows
        .first()
        .map_or(0, |r| u16::try_from(r.chars().count()).unwrap_or(0));
    let show_mascot = !focus_mode && size.width >= MASCOT_MIN_WIDTH && badge_width > 0;
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
    // The badge owns the right edge of both viewport rows; keep the hint narrow
    // enough that left-aligned hint text never runs under it.
    let hint_area = Rect::new(size.x, size.y + 1, size.width.saturating_sub(reserve), 1);
    Paragraph::new(Line::from(Span::styled(
        glyphs::PROMPT,
        Style::default().fg(theme.accent().ratatui()),
    )))
    .render(gutter, buf);
    textarea.render(editor, buf);

    // Inline prefix-history ghost text: draws the suffix of the matched history
    // entry right after the cursor in muted color. Only shown when the cursor is
    // on the first (input) row and there is room to display it.
    if let Some(suffix) = prefix_suffix {
        let (cursor_row, cursor_col) = textarea.cursor();
        if cursor_row == 0 {
            let start_x = editor.x + u16::try_from(cursor_col).unwrap_or(u16::MAX);
            let max_x = editor.x + editor.width;
            let ghost_style = Style::default().fg(theme.muted().ratatui()).add_modifier(Modifier::DIM);
            let mut x = start_x;
            for ch in suffix.chars() {
                if x >= max_x {
                    break;
                }
                let mut glyph = [0u8; 4];
                if let Some(cell) = buf.cell_mut(Position::new(x, editor.y)) {
                    cell.set_symbol(ch.encode_utf8(&mut glyph));
                    cell.set_style(ghost_style);
                }
                x += 1; // simplistic: assume ASCII-width; CJK would overflow, which is fine
            }
        }
    }
    if show_mascot {
        let fg = mascot.badge_color(&theme).ratatui();
        let badge_x = size.x + size.width.saturating_sub(badge_width);
        for (row, line) in badge_rows.iter().enumerate() {
            if row >= usize::from(VIEWPORT_ROWS) {
                break; // never paint past the viewport into the menu rows
            }
            let y = size.y + u16::try_from(row).unwrap_or(0);
            for (col, ch) in line.chars().enumerate() {
                let x = badge_x + u16::try_from(col).unwrap_or(0);
                if x < size.x + size.width {
                    let mut glyph = [0u8; 4];
                    if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                        cell.set_symbol(ch.encode_utf8(&mut glyph));
                        cell.set_fg(fg);
                    }
                }
            }
        }
    }
    Paragraph::new(Line::from(Span::styled(
        hint.to_string(),
        Style::default().fg(theme.muted().ratatui()),
    )))
    .render(hint_area, buf);

    render_menu(buf, size, completions, completion_idx, &theme);

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

/// Draw the completion dropdown as an aligned two-column table (label + muted
/// detail), one row per visible candidate. The selected row is marked with `>`
/// and accented. When candidates outnumber rows the window scrolls to keep the
/// selection visible (see [`menu_window`]). The rows are reserved by
/// [`ReplEditor::edit_loop`] via the viewport height, so this only paints while
/// the list is actually shown.
fn render_menu(
    buf: &mut Buffer,
    size: Rect,
    completions: &[CompletionItem],
    completion_idx: Option<usize>,
    theme: &Theme,
) {
    let menu_top = size.y + VIEWPORT_ROWS;
    let avail = size.height.saturating_sub(VIEWPORT_ROWS);
    if avail == 0 || completions.is_empty() {
        return;
    }
    let selected = completion_idx.unwrap_or(0);
    let (start, end) = menu_window(completions.len(), usize::from(avail), selected);
    let window = &completions[start..end];
    // Measure the label column across the whole list, not just the window, so
    // the detail column does not jitter as the selection scrolls.
    let label_col = completions
        .iter()
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or(0)
        .saturating_add(2);
    for (i, item) in window.iter().enumerate() {
        let Some(row_y) = menu_top.checked_add(u16::try_from(i).unwrap_or(u16::MAX)) else {
            break;
        };
        if row_y >= size.y + size.height {
            break; // viewport was clamped to the screen; drop overflow rows
        }
        let row = Rect::new(size.x, row_y, size.width, 1);
        let is_sel = completion_idx == Some(start + i);
        let label_style = if is_sel {
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Reset)
        };
        let pad = label_col.saturating_sub(item.label.chars().count());
        let line = Line::from(vec![
            Span::styled(if is_sel { "> " } else { "  " }, label_style),
            Span::styled(item.label.clone(), label_style),
            Span::styled(" ".repeat(pad), Style::default()),
            Span::styled(
                item.detail.clone(),
                Style::default().fg(theme.muted().ratatui()),
            ),
        ]);
        Paragraph::new(line).render(row, buf);
    }
}

/// Build the hint row: a transient status message wins, then completion feedback
/// for the current site, else the idle key legend. Also refreshes the candidate
/// set from [`complete`], preserving the highlight across a redraw so ↑/↓/Tab
/// cycling stays stable.
fn build_hint(
    text: &str,
    ctx: &CompletionContext,
    completions: &mut Vec<CompletionItem>,
    completion_idx: &mut Option<usize>,
    status: Option<&str>,
) -> String {
    if let Some(status) = status {
        return status.to_string();
    }
    let Some(site) = completion_site(text) else {
        // Leaving a completable position closes the menu: drop stale candidates
        // so the viewport height and dropdown rows follow the current input.
        completions.clear();
        *completion_idx = None;
        return String::from(
            "Enter send · Alt+Enter newline · / commands · ↑/↓ select · Tab complete · Ctrl+D quit",
        );
    };
    // Recompute only when the candidate set actually changed, so cycling the
    // highlight survives a redraw without snapping back to the top.
    let fresh = complete(text, ctx);
    if !same_labels(completions, &fresh) {
        *completions = fresh;
        *completion_idx = None;
    }
    if completions.is_empty() {
        return format!("no {} matches {}", site.noun(), site.typed());
    }
    let shown = completion_idx
        .and_then(|idx| completions.get(idx))
        .or_else(|| completions.first())
        .map_or_else(|| site.typed().to_string(), |item| item.label.clone());
    let count = completions.len();
    let pos = completion_idx.map_or(1, |idx| idx + 1);
    format!("{shown}  ({pos}/{count}) · Tab completes")
}

/// Whether two candidate lists offer the same labels in the same order (the
/// detail column may differ without invalidating the highlight).
fn same_labels(a: &[CompletionItem], b: &[CompletionItem]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(left, right)| left.label == right.label)
}

/// Complete the highlighted dropdown row into the buffer (Enter while a row is
/// actively selected), leaving the menu open so the next frame recomputes it for
/// the now-longer input (e.g. `/mod` → `/model ` → the model list).
fn complete_menu(
    textarea: &mut TextArea<'static>,
    completions: &[CompletionItem],
    completion_idx: &mut Option<usize>,
    text: &str,
) {
    if let Some(idx) = completion_idx.take() {
        if let (Some(item), Some(site)) = (completions.get(idx), completion_site(text)) {
            apply_completion(textarea, item, site);
        }
    }
    *completion_idx = None;
}

/// Rewrite the buffer for a chosen candidate: a command head becomes
/// `"<label> "` (the trailing space opens its argument menu); an argument
/// replaces only the partial arg, keeping the command (`/model de` → the id).
fn apply_completion(
    textarea: &mut TextArea<'static>,
    item: &CompletionItem,
    site: CompletionSite<'_>,
) {
    let text = match site {
        CompletionSite::Command { .. } => format!("{} ", item.label),
        CompletionSite::Argument { command, .. } => format!("{command} {}", item.label),
    };
    *textarea = blank_textarea();
    textarea.insert_str(text);
}

/// Whether Enter should complete a highlighted dropdown row instead of
/// submitting: only when a completable site is active, a row is highlighted (via
/// ↑/↓/Tab), and that candidate is not already the whole typed token. `None`
/// means Enter submits the line exactly as typed; otherwise the row to complete.
#[must_use]
fn menu_accept_on_enter(
    completions: &[CompletionItem],
    completion_idx: Option<usize>,
    text: &str,
) -> Option<usize> {
    let site = completion_site(text)?;
    let idx = completion_idx?;
    let item = completions.get(idx)?;
    (item.label != site.typed()).then_some(idx)
}

/// Take the currently offered completion and advance the cycle to the next one.
fn accept_completion(
    completions: &[CompletionItem],
    completion_idx: &mut Option<usize>,
) -> Option<CompletionItem> {
    if completions.is_empty() {
        return None;
    }
    let idx = *completion_idx.get_or_insert(0);
    let chosen = completions.get(idx).cloned();
    cycle_completion(completions, completion_idx, false);
    chosen
}

fn cycle_completion(
    completions: &[CompletionItem],
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

/// Character classification for subword-aware word motion. The categories are
/// deliberately coarse — enough to distinguish code identifiers (camelCase,
/// snake_case, kebab-case, digit transitions) from punctuation and whitespace,
/// plus a CJK bucket that advances two ideographs at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Lower,      // a-z
    Upper,      // A-Z
    Digit,      // 0-9
    Underscore, // _
    Hyphen,     // -
    Cjk,        // CJK ideograph (U+4E00..U+9FFF + common extensions)
    Whitespace, // space, tab, etc.
    Other,      // punctuation, symbols, everything else
}

fn char_class(c: char) -> CharClass {
    if c.is_ascii_lowercase() {
        CharClass::Lower
    } else if c.is_ascii_uppercase() {
        CharClass::Upper
    } else if c.is_ascii_digit() {
        CharClass::Digit
    } else if c == '_' {
        CharClass::Underscore
    } else if c == '-' {
        CharClass::Hyphen
    } else if c.is_whitespace() {
        CharClass::Whitespace
    } else if is_cjk_ideograph(c) {
        CharClass::Cjk
    } else {
        CharClass::Other
    }
}

/// Whether `c` is a CJK ideograph we treat as "one character" for the
/// two-step word motion rule. Covers the common Unified Ideographs block plus
/// Extension A; rare extensions are lumped into `Other` and behave like
/// punctuation (one at a time).
fn is_cjk_ideograph(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF   // Extension A
        | 0x4E00..=0x9FFF // Unified Ideographs
        | 0xF900..=0xFAFF // Compatibility Ideographs
    )
}

/// Byte index of the previous word-break position in `line`, given a byte
/// offset `col`. Always returns a value in `0..=col`.
///
/// Subword boundaries match what code editors do:
///   camelCaseWord → camel|Case|Word
///   snake_case    → snake|_|case
///   kebab-case    → kebab|-|case
///   foo123bar     → foo|123|bar
///   HTTPRequest   → HTTP|Request
///   CJK ideographs → 2 chars per jump
///   whitespace    → one jump per contiguous run
///   punctuation   → one jump per contiguous run
fn word_break_prev(line: &str, col: usize) -> usize {
    let col = col.min(line.len());
    if col == 0 {
        return 0;
    }

    let chars: Vec<(usize, char)> = line.char_indices().collect();
    // Map byte offset → char index (first char at or after col).
    let cur_char = match chars.binary_search_by_key(&col, |&(b, _)| b) {
        Ok(i) => i,
        Err(i) => i,
    };
    if cur_char == 0 {
        return 0;
    }

    let target_char = word_break_prev_char_idx(&chars, cur_char);
    chars[target_char].0
}

fn word_break_prev_char_idx(chars: &[(usize, char)], cur: usize) -> usize {
    if cur == 0 {
        return 0;
    }
    let mut pos = cur - 1; // start from char immediately before cursor

    // Skip trailing whitespace so we always land on a content character
    // before looking for the subword boundary. This matches readline:
    // "hello |world" → Ctrl+Left → "|hello world" (jump past the space
    // and land at the start of the previous word).
    if char_class(chars[pos].1) == CharClass::Whitespace {
        while pos > 0 && char_class(chars[pos - 1].1) == CharClass::Whitespace {
            pos -= 1;
        }
        // pos now points to the leftmost whitespace char. Move past it
        // to land on the last content char before the whitespace run.
        if pos == 0 {
            return 0;
        }
        pos -= 1;
        // pos now points to the rightmost non-whitespace char; fall through
        // to find the start of that subword.
    }

    // Find left boundary of the subword we landed on.
    match char_class(chars[pos].1) {
        CharClass::Cjk => {
            // Jump back by 2 CJK chars, or to the start of the CJK run.
            let mut steps = 1; // chars[pos] already counts as one
            while pos > 0 && steps < 2 {
                if char_class(chars[pos - 1].1) == CharClass::Cjk {
                    pos -= 1;
                    steps += 1;
                } else {
                    break;
                }
            }
            pos
        }
        CharClass::Lower => {
            // Scan left through the lowercase run.
            while pos > 0 && char_class(chars[pos - 1].1) == CharClass::Lower {
                pos -= 1;
            }
            // If preceded by a single uppercase → camelCase hump start.
            // Include that uppercase as the subword start.
            if pos > 0 && char_class(chars[pos - 1].1) == CharClass::Upper {
                pos -= 1;
                // Don't continue left into a longer Upper run — that's the
                // previous subword (e.g. "HTTP" in "HTTPRequest").
            }
            pos
        }
        CharClass::Upper => {
            // Scan left through the uppercase run.
            while pos > 0 && char_class(chars[pos - 1].1) == CharClass::Upper {
                pos -= 1;
            }
            pos
        }
        CharClass::Digit => {
            while pos > 0 && char_class(chars[pos - 1].1) == CharClass::Digit {
                pos -= 1;
            }
            pos
        }
        CharClass::Underscore | CharClass::Hyphen => {
            let sep = char_class(chars[pos].1);
            while pos > 0 && char_class(chars[pos - 1].1) == sep {
                pos -= 1;
            }
            pos
        }
        CharClass::Whitespace => {
            pos // unreachable after the skip above, here for exhaustiveness
        }
        CharClass::Other => {
            while pos > 0 && char_class(chars[pos - 1].1) == CharClass::Other {
                pos -= 1;
            }
            pos
        }
    }
}

/// Byte index of the next word-break position in `line`, given a byte
/// offset `col`. Always returns a value in `col..=line.len()`.
fn word_break_next(line: &str, col: usize) -> usize {
    let col = col.min(line.len());
    if col >= line.len() {
        return line.len();
    }

    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let cur_char = match chars.binary_search_by_key(&col, |&(b, _)| b) {
        Ok(i) => i,
        Err(i) => i,
    };
    if cur_char >= chars.len() {
        return line.len();
    }

    let target_char = word_break_next_char_idx(&chars, cur_char);
    if target_char >= chars.len() {
        line.len()
    } else {
        chars[target_char].0
    }
}

fn word_break_next_char_idx(chars: &[(usize, char)], cur: usize) -> usize {
    let len = chars.len();
    if cur >= len {
        return len;
    }
    let mut pos = cur;

    // Skip leading whitespace so we always land on a content character
    // before looking for the subword boundary. This matches readline:
    // "hello| world" → Ctrl+Right → "hello world|" (skip past the space
    // and land at the end of the next word).
    if char_class(chars[pos].1) == CharClass::Whitespace {
        while pos < len && char_class(chars[pos].1) == CharClass::Whitespace {
            pos += 1;
        }
        if pos == len {
            return len;
        }
        // pos now points to the first non-whitespace char; fall through
        // to find the end of that subword.
    }

    // Find right boundary of the subword we're at.
    match char_class(chars[pos].1) {
        CharClass::Cjk => {
            let mut steps = 0;
            while pos < len && steps < 2 {
                if char_class(chars[pos].1) == CharClass::Cjk {
                    pos += 1;
                    steps += 1;
                } else {
                    break;
                }
            }
            pos
        }
        CharClass::Upper => {
            let start = pos;
            // Consume the uppercase run first.
            while pos < len && char_class(chars[pos].1) == CharClass::Upper {
                pos += 1;
            }
            let upper_run_len = pos - start;
            if upper_run_len == 1 && pos < len && char_class(chars[pos].1) == CharClass::Lower {
                // Single uppercase + following lowercase = one camelCase hump.
                // Continue through the lowercase run: "camelCase" → "Case".
                while pos < len && char_class(chars[pos].1) == CharClass::Lower {
                    pos += 1;
                }
            } else if upper_run_len > 1 && pos < len && char_class(chars[pos].1) == CharClass::Lower {
                // Multi-uppercase followed by lowercase: "HTTPRequest" → "HTTP|Request".
                // Last uppercase starts the next hump — back up by one.
                pos -= 1;
            }
            pos
        }
        CharClass::Lower | CharClass::Digit => {
            let cls = char_class(chars[pos].1);
            while pos < len && char_class(chars[pos].1) == cls {
                pos += 1;
            }
            pos
        }
        CharClass::Underscore | CharClass::Hyphen => {
            let sep = char_class(chars[pos].1);
            while pos < len && char_class(chars[pos].1) == sep {
                pos += 1;
            }
            pos
        }
        CharClass::Whitespace => {
            pos // unreachable after the skip above, here for exhaustiveness
        }
        CharClass::Other => {
            while pos < len && char_class(chars[pos].1) == CharClass::Other {
                pos += 1;
            }
            pos
        }
    }
}

/// Move the textarea cursor by one word in `direction`. Operates on the
/// current line only (no cross-line jumps).
fn move_word(textarea: &mut TextArea<'static>, backward: bool) {
    let (row, col) = textarea.cursor();
    let Some(line) = textarea.lines().get(row).cloned() else {
        return;
    };
    let target_byte = if backward {
        word_break_prev(&line, col)
    } else {
        word_break_next(&line, col)
    };
    // Count char positions: tui-textarea's Left/Right moves by grapheme,
    // which for our purposes is close enough to char-counted movement.
    let cur_chars = line[..col].chars().count();
    let tgt_chars = line[..target_byte].chars().count();
    let delta = cur_chars.abs_diff(tgt_chars);
    let key_code = if backward { KeyCode::Left } else { KeyCode::Right };
    for _ in 0..delta {
        textarea.input(KeyEvent::new(key_code, KeyModifiers::NONE));
    }
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
        complete, decode_history, echo_lines, encode_history_line, menu_accept_on_enter,
        menu_window, suggest_command, CompletionContext, CompletionItem, History,
    };

    /// A context with a few models, the three modes, and two sessions, so the
    /// argument-position tests exercise every pool.
    fn ctx() -> CompletionContext {
        CompletionContext {
            models: vec![
                CompletionItem::new("deepseek-v4-flash", "1M ctx · seed"),
                CompletionItem::new("deepseek-v4-pro", "1M ctx · seed"),
                CompletionItem::new("kimi-k3", "seed"),
            ],
            modes: vec![
                CompletionItem::new("read-only", ""),
                CompletionItem::new("workspace-write", ""),
                CompletionItem::new("full", ""),
            ],
            sessions: vec![
                CompletionItem::new("1", "a.json"),
                CompletionItem::new("2", "b.json"),
            ],
            current_model: Some("deepseek-v4-flash".to_string()),
            current_mode: Some("workspace-write".to_string()),
        }
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|item| item.label.as_str()).collect()
    }

    #[test]
    fn command_position_completes_heads_with_details() {
        let cands = complete("/comp", &ctx());
        assert_eq!(labels(&cands), vec!["/compact"]);
        assert_eq!(cands[0].detail, "Compact session history");
        // A bare `/` lists every command; the head is what completes.
        assert!(complete("/", &ctx()).len() > 5);
        assert!(labels(&complete("/model", &ctx())).contains(&"/model"));
    }

    #[test]
    fn argument_position_completes_the_right_pool() {
        // `/model ` (trailing space) opens the model list, unfiltered.
        assert_eq!(
            labels(&complete("/model ", &ctx())),
            vec!["deepseek-v4-flash", "deepseek-v4-pro", "kimi-k3"]
        );
        // A partial arg narrows it; matching is case-insensitive.
        assert_eq!(
            labels(&complete("/model DEEP", &ctx())),
            vec!["deepseek-v4-flash", "deepseek-v4-pro"]
        );
        assert_eq!(labels(&complete("/mode ", &ctx())).len(), 3);
        assert_eq!(labels(&complete("/open ", &ctx())), vec!["1", "2"]);
        // The detail column carries the model's context/provenance.
        assert_eq!(complete("/model kimi", &ctx())[0].detail, "seed");
    }

    #[test]
    fn completion_stops_where_there_is_nothing_to_offer() {
        // No pool for `/search`; a second argument is free text; non-slash is a turn.
        assert!(complete("/search foo", &ctx()).is_empty());
        assert!(complete("/model a b", &ctx()).is_empty());
        assert!(complete("hello", &ctx()).is_empty());
        assert!(complete("/model zzz", &ctx()).is_empty());
        // A command with no matching head.
        assert!(complete("/zzz", &ctx()).is_empty());
    }

    #[test]
    fn enter_completes_only_an_active_highlight() {
        let cands = complete("/comp", &ctx());
        // No row highlighted yet: Enter submits the partial rather than completing.
        assert_eq!(menu_accept_on_enter(&cands, None, "/comp"), None);
        // A highlighted row that differs from the token: Enter completes it.
        assert_eq!(menu_accept_on_enter(&cands, Some(0), "/comp"), Some(0));
        // The exact command already typed: Enter submits (nothing to complete).
        let exact = complete("/compact", &ctx());
        assert_eq!(menu_accept_on_enter(&exact, Some(0), "/compact"), None);
        // Argument position: a highlighted model differing from the arg completes.
        let models = complete("/model deep", &ctx());
        assert_eq!(
            menu_accept_on_enter(&models, Some(1), "/model deep"),
            Some(1)
        );
        // Once the arg equals the highlighted label, Enter submits.
        let one = complete("/model deepseek-v4-pro", &ctx());
        assert_eq!(
            menu_accept_on_enter(&one, Some(0), "/model deepseek-v4-pro"),
            None
        );
    }

    #[test]
    fn menu_window_scrolls_to_follow_the_selection() {
        // Fewer candidates than rows: show them all from the top.
        assert_eq!(menu_window(3, 8, 0), (0, 3));
        // Selection inside the first window: no scroll.
        assert_eq!(menu_window(20, 8, 7), (0, 8));
        // Selection just past the window: scroll one.
        assert_eq!(menu_window(20, 8, 8), (1, 9));
        // Selection near the end: clamp so the last row stays visible.
        assert_eq!(menu_window(20, 8, 19), (12, 20));
        // Degenerate inputs.
        assert_eq!(menu_window(0, 8, 0), (0, 0));
        assert_eq!(menu_window(20, 0, 5), (0, 0));
    }

    #[test]
    fn suggest_catches_typos_only() {
        assert_eq!(suggest_command("/compct"), Some("/compact"));
        assert_eq!(suggest_command("/m"), None, "too short to judge");
        assert_eq!(suggest_command("just text"), None);
    }

    #[test]
    fn plan_and_queue_arguments_complete_their_actions() {
        // A bare `/plan ` lists its three fixed verbs in declared order.
        assert_eq!(
            labels(&complete("/plan ", &ctx())),
            vec!["approve", "end", "status"]
        );
        assert_eq!(labels(&complete("/plan app", &ctx())), vec!["approve"]);
        assert_eq!(labels(&complete("/queue ", &ctx())), vec!["pop", "clear"]);
        assert_eq!(labels(&complete("/queue c", &ctx())), vec!["clear"]);
        // A non-matching action query offers nothing.
        assert!(complete("/plan zzz", &ctx()).is_empty());
    }

    #[test]
    fn fuzzy_matching_reaches_non_prefix_letters() {
        // "pn" is a subsequence of /plan and /pin, neither anchored past `/`.
        let cands = complete("/pn", &ctx());
        let heads = labels(&cands);
        assert!(
            heads.contains(&"/plan"),
            "fuzzy should reach /plan: {heads:?}"
        );
        // A scattered model query still resolves to the deepseek ids.
        assert!(labels(&complete("/model dsk", &ctx())).contains(&"deepseek-v4-flash"));
    }

    #[test]
    fn current_model_and_mode_are_marked() {
        let models = complete("/model ", &ctx());
        let flash = models
            .iter()
            .find(|item| item.label == "deepseek-v4-flash")
            .expect("flash present");
        assert!(
            flash.detail.contains("(current)"),
            "active model must be tagged: {}",
            flash.detail
        );
        let pro = models
            .iter()
            .find(|item| item.label == "deepseek-v4-pro")
            .expect("pro present");
        assert!(!pro.detail.contains("(current)"));
        // An empty-detail mode still reads clearly when it is the active one.
        let modes = complete("/mode ", &ctx());
        let active = modes
            .iter()
            .find(|item| item.label == "workspace-write")
            .expect("mode present");
        assert_eq!(active.detail, "(current)");
    }

    #[test]
    fn open_and_remember_join_command_completion() {
        let cands = complete("/", &ctx());
        let all = labels(&cands);
        assert!(all.contains(&"/open"), "missing /open: {all:?}");
        assert!(all.contains(&"/remember"), "missing /remember: {all:?}");
        // /remember takes free text, so it never offers an argument pool.
        assert!(complete("/remember buy milk", &ctx()).is_empty());
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

    #[test]
    fn history_prefix_search_backward_finds_match() {
        let h = History {
            entries: vec!["ls -la".into(), "cd /tmp".into(), "ls foo".into()],
            cursor: 3,
        };
        // Search from end (most recent) for "ls" → finds "ls foo" at index 2
        assert_eq!(h.search_prefix_backward("ls", 3), Some(2));
        // From index 2, search further back → finds "ls -la" at index 0
        assert_eq!(h.search_prefix_backward("ls", 2), Some(0));
        // From index 0, no older match → None
        assert_eq!(h.search_prefix_backward("ls", 0), None);
    }

    #[test]
    fn history_prefix_search_forward_finds_match() {
        let h = History {
            entries: vec!["ls -la".into(), "cd /tmp".into(), "ls foo".into()],
            cursor: 3,
        };
        // Search forward from index 0 for "ls" → finds "ls foo" at index 2
        assert_eq!(h.search_prefix_forward("ls", 0), Some(2));
        // From index 2, no newer match → None
        assert_eq!(h.search_prefix_forward("ls", 2), None);
    }

    #[test]
    fn history_prefix_search_empty_prefix_returns_none() {
        let h = History {
            entries: vec!["hello".into()],
            cursor: 1,
        };
        assert_eq!(h.search_prefix_backward("", 1), None);
        assert_eq!(h.search_prefix_forward("", 0), None);
    }

    #[test]
    fn history_prefix_search_no_match_returns_none() {
        let h = History {
            entries: vec!["ls -la".into(), "cd /tmp".into()],
            cursor: 2,
        };
        assert_eq!(h.search_prefix_backward("git", 2), None);
        assert_eq!(h.search_prefix_forward("git", 0), None);
    }

    #[test]
    fn history_prefix_search_partial_match() {
        let h = History {
            entries: vec!["cargo build".into(), "cargo test".into(), "cargo run".into()],
            cursor: 3,
        };
        // "car" matches all three
        assert_eq!(h.search_prefix_backward("car", 3), Some(2));
        assert_eq!(h.search_prefix_backward("car", 2), Some(1));
        assert_eq!(h.search_prefix_backward("car", 1), Some(0));
        // "cargo t" matches only "cargo test"
        assert_eq!(h.search_prefix_backward("cargo t", 3), Some(1));
    }

    #[test]
    fn history_entry_at_and_len() {
        let h = History {
            entries: vec!["a".into(), "b".into(), "c".into()],
            cursor: 3,
        };
        assert_eq!(h.len(), 3);
        assert_eq!(h.entry_at(0), Some("a"));
        assert_eq!(h.entry_at(2), Some("c"));
        assert_eq!(h.entry_at(3), None);
    }

    // ── word motion (subword-aware + CJK 2-step) ──────────────────────

    use super::{word_break_next, word_break_prev};

    fn prev_at(line: &str, col_byte: usize) -> usize {
        word_break_prev(line, col_byte)
    }
    fn next_at(line: &str, col_byte: usize) -> usize {
        word_break_next(line, col_byte)
    }

    #[test]
    fn word_prev_at_start_stays_at_start() {
        assert_eq!(prev_at("hello world", 0), 0);
    }

    #[test]
    fn word_next_at_end_stays_at_end() {
        let s = "hello";
        assert_eq!(next_at(s, s.len()), s.len());
    }

    #[test]
    fn word_prev_simple_words() {
        let s = "hello world";
        // from end of "world" → start of "world"
        assert_eq!(&s[prev_at(s, s.len())..s.len()], "world");
        // from start of "world" → start of "hello"
        let world_start = s.find('w').unwrap();
        assert_eq!(prev_at(s, world_start), 0);
    }

    #[test]
    fn word_next_simple_words() {
        let s = "hello world";
        let first_end = next_at(s, 0);
        assert_eq!(&s[..first_end], "hello");
        // skip the space
        let second_start = first_end + 1;
        assert!(s[second_start..].starts_with('w'));
        let second_end = next_at(s, second_start);
        assert_eq!(&s[second_start..second_end], "world");
    }

    #[test]
    fn word_prev_camel_case() {
        let s = "camelCaseWord";
        // end → start of "Word"
        let pos = prev_at(s, s.len());
        assert_eq!(&s[pos..], "Word");
        // "Word" start → start of "Case"
        let pos2 = prev_at(s, pos);
        assert_eq!(&s[pos2..pos], "Case");
    }

    #[test]
    fn word_next_camel_case() {
        let s = "camelCaseWord";
        let pos = next_at(s, 0);
        assert_eq!(&s[..pos], "camel");
        let pos2 = next_at(s, pos);
        assert_eq!(&s[pos..pos2], "Case");
        let pos3 = next_at(s, pos2);
        assert_eq!(&s[pos2..pos3], "Word");
    }

    #[test]
    fn word_prev_snake_case() {
        let s = "snake_case_name";
        // end → start of "name"
        let pos = prev_at(s, s.len());
        assert_eq!(&s[pos..], "name");
        // "name" start → underscore (jump back into separator)
        let pos2 = prev_at(s, pos);
        assert_eq!(&s[pos2..pos], "_");
        // underscore → start of "case"
        let pos3 = prev_at(s, pos2);
        assert_eq!(&s[pos3..pos2], "case");
    }

    #[test]
    fn word_next_snake_case() {
        let s = "snake_case";
        let pos = next_at(s, 0);
        assert_eq!(&s[..pos], "snake");
        let pos2 = next_at(s, pos);
        assert_eq!(&s[pos..pos2], "_");
        let pos3 = next_at(s, pos2);
        assert_eq!(&s[pos2..pos3], "case");
    }

    #[test]
    fn word_prev_kebab_case() {
        let s = "kebab-case-thing";
        let pos = prev_at(s, s.len());
        assert_eq!(&s[pos..], "thing");
    }

    #[test]
    fn word_prev_digit_transition() {
        let s = "foo123bar";
        // end → start of "bar"
        let pos = prev_at(s, s.len());
        assert_eq!(&s[pos..], "bar");
        // "bar" → "123"
        let pos2 = prev_at(s, pos);
        assert_eq!(&s[pos2..pos], "123");
        // "123" → "foo"
        let pos3 = prev_at(s, pos2);
        assert_eq!(&s[pos3..pos2], "foo");
    }

    #[test]
    fn word_next_digit_transition() {
        let s = "foo123bar";
        let pos = next_at(s, 0);
        assert_eq!(&s[..pos], "foo");
        let pos2 = next_at(s, pos);
        assert_eq!(&s[pos..pos2], "123");
        let pos3 = next_at(s, pos2);
        assert_eq!(&s[pos2..pos3], "bar");
    }

    #[test]
    fn word_prev_pascal_upper_run() {
        // "HTTPRequest" → "HTTP" | "Request"
        let s = "HTTPRequest";
        let pos = prev_at(s, s.len());
        assert_eq!(&s[pos..], "Request");
        let pos2 = prev_at(s, pos);
        assert_eq!(&s[pos2..pos], "HTTP");
    }

    #[test]
    fn word_next_pascal_upper_run() {
        let s = "HTTPRequest";
        let pos = next_at(s, 0);
        assert_eq!(&s[..pos], "HTTP");
        let pos2 = next_at(s, pos);
        assert_eq!(&s[pos..pos2], "Request");
    }

    #[test]
    fn word_prev_cjk_two_chars() {
        let s = "你好世界";
        // end → position after "世界" start (2 chars back)
        let pos = prev_at(s, s.len());
        let suffix = &s[pos..];
        assert_eq!(suffix.chars().count(), 2);
        assert_eq!(suffix, "世界");
        // another jump → first two
        let pos2 = prev_at(s, pos);
        assert_eq!(&s[pos2..pos], "你好");
    }

    #[test]
    fn word_next_cjk_two_chars() {
        let s = "你好世界";
        let pos = next_at(s, 0);
        assert_eq!(&s[..pos], "你好");
        let pos2 = next_at(s, pos);
        assert_eq!(&s[pos..pos2], "世界");
        assert_eq!(pos2, s.len());
    }

    #[test]
    fn word_prev_cjk_mixed_with_ascii() {
        let s = "hello你好世界";
        let pos = prev_at(s, s.len());
        // last 2 CJK chars
        assert_eq!(&s[pos..], "世界");
        let pos2 = prev_at(s, pos);
        assert_eq!(&s[pos2..pos], "你好");
        let pos3 = prev_at(s, pos2);
        assert_eq!(&s[pos3..pos2], "hello");
    }

    #[test]
    fn word_prev_whitespace_run() {
        let s = "hello   world";
        let world_start = s.find('w').unwrap();
        // from start of "world" → start of "hello" (readline-style:
        // whitespace is skipped, not treated as a jump target)
        let pos = prev_at(s, world_start);
        assert_eq!(pos, 0);
        assert_eq!(&s[pos..pos + 5], "hello");
    }

    #[test]
    fn word_prev_punctuation_run() {
        let s = "hello...world";
        let world_start = s.find("world").unwrap();
        let pos = prev_at(s, world_start);
        assert_eq!(&s[pos..world_start], "...");
    }

    #[test]
    fn word_prev_empty_line() {
        assert_eq!(prev_at("", 0), 0);
    }

    #[test]
    fn word_next_empty_line() {
        assert_eq!(next_at("", 0), 0);
    }

    #[test]
    fn word_prev_single_char() {
        assert_eq!(prev_at("a", 1), 0);
    }

    #[test]
    fn word_next_single_char() {
        assert_eq!(next_at("a", 0), 1);
    }
}
