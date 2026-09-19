//! REPL input: reedline-backed line editor with a slash-command completion
//! menu, persistent history, and a plain-line fallback for non-TTY sessions.

use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use reedline::{
    Completer, CompletionResult, DescriptionMenu, FileBackedHistory, Prompt, PromptEditMode,
    PromptHistorySearch, Reedline, ReedlineMenu, Signal, Span, Suggestion,
};

/// REPL slash commands surfaced by the completion menu while typing. Kept in
/// sync with `print_repl_help` in main.rs; add new commands to both.
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
        let head = (*name).split([' ', '<']).next().unwrap_or(*name);
        let score = strsim::jaro_winkler(typed, head);
        if score >= 0.82 && best.is_none_or(|(_, top)| score > top) {
            best = Some((head, score));
        }
    }
    best.map(|(name, _)| name)
}

struct SlashCompleter;

impl Completer for SlashCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let prefix = &line[..pos];
        if !prefix.starts_with('/') || prefix.contains(char::is_whitespace) {
            return CompletionResult::fresh(Vec::new());
        }
        let suggestions: Vec<Suggestion> = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, description)| Suggestion {
                value: (*name).to_owned(),
                description: Some((*description).to_owned()),
                span: Span { start: 0, end: pos },
                ..Suggestion::default()
            })
            .collect();
        CompletionResult::fresh(suggestions)
    }
}

struct ReplPrompt;

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("› ")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("search: ")
    }
}

pub struct ReplEditor {
    inner: Reedline,
}

impl ReplEditor {
    /// Build the editor with a slash-command menu and a history file. History
    /// failures degrade to in-memory history rather than blocking startup.
    #[must_use]
    pub fn new(history_path: &Path) -> Self {
        let history = FileBackedHistory::with_file(500, history_path.to_path_buf())
            .ok()
            .unwrap_or_default();
        let menu = DescriptionMenu::default()
            .with_columns(1)
            .with_description_rows(2);
        let inner = Reedline::create()
            .with_completer(Box::new(SlashCompleter))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
            .with_quick_completions(true)
            .with_partial_completions(true)
            .with_history(Box::new(history));
        Self { inner }
    }

    /// Read one REPL line. `Ok(None)` means quit (Ctrl+D); Ctrl+C starts a
    /// fresh line instead of exiting.
    pub fn read_line(&mut self) -> io::Result<Option<String>> {
        if !io::stdin().is_terminal() {
            return read_line_fallback();
        }
        loop {
            match self.inner.read_line(&ReplPrompt)? {
                Signal::Success(line) => return Ok(Some(line)),
                Signal::CtrlD => {
                    // Make the exit path explicit so a blank Ctrl+D is not
                    // mistaken for a hang.
                    println!("\nquit.");
                    return Ok(None);
                }
                // Ctrl+C on an idle line clears it rather than exiting; during a
                // running turn the same key interrupts the turn (see the REPL).
                Signal::CtrlC => {
                    println!(
                        "^C line cleared. Ctrl+C interrupts a running turn; /exit or Ctrl+D quits."
                    );
                }
                _ => {}
            }
        }
    }
}

fn read_line_fallback() -> io::Result<Option<String>> {
    let mut stdout = io::stdout();
    write!(stdout, "› ")?;
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
