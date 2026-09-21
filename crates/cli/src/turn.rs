//! Interactive turn rendering + driving (cli-thinning): the blocking REPL's
//! per-turn terminal renderer, the deterministic [`TurnOutcome`] the task loop
//! verifies against, and `run_turn_interactive` (stream events, Ctrl+C abort,
//! auto-save). Extracted verbatim from `main.rs` and re-exported crate-wide so
//! the REPL, `plan.rs` and `doctor` call sites and `super::` test paths keep
//! resolving. Input-prep helpers (`expand_attachments`, `fold_tool_output`) stay
//! in `main.rs` and are reached through the crate root.

use std::io::{self, Write};

use crossterm::style::Stylize;
use runtime::{
    truncate_chars, AgentEvent, ContentBlock, ConversationMessage, MessageRole, PermissionPrompter,
    TokenUsage,
};
use tokio_util::sync::CancellationToken;

use crate::render::{ColorTheme, Spinner, TerminalRenderer};
use crate::{
    expand_attachments, fold_tool_output, save_session_async, AgentRuntime, SessionShared,
    FOLD_TOOL_OUTPUT_LINES,
};

/// Aborts the wrapped task when the guard drops. tokio detaches a spawned task
/// once its `JoinHandle` is dropped, so an early `?` return between the spawn
/// and any explicit abort (e.g. a failing spinner `tick`) would orphan the
/// Ctrl+C listener for the rest of the process. Holding the handle in a Drop
/// guard makes the abort unconditional on every exit path.
struct AbortGuard(tokio::task::JoinHandle<()>);

impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Terminal-side rendering state for one interactive turn.
struct TurnRenderer {
    state: SessionShared,
    spinner: Spinner,
    theme: ColorTheme,
    renderer: TerminalRenderer,
    spinner_active: bool,
    saw_text: bool,
    last_usage: Option<TokenUsage>,
    /// Accumulated assistant text for the current message segment; rendered
    /// as finished markdown on `MessageStop`.
    assistant_text: String,
}

impl TurnRenderer {
    fn new(state: SessionShared) -> Self {
        let renderer = TerminalRenderer::new();
        let theme = *renderer.color_theme();
        Self {
            state,
            spinner: Spinner::new(),
            theme,
            renderer,
            spinner_active: true,
            saw_text: false,
            last_usage: None,
            assistant_text: String::new(),
        }
    }

    fn render(&mut self, event: &AgentEvent) {
        let mut out = io::stdout();
        match event {
            AgentEvent::TextDelta(delta) => {
                if self.spinner_active {
                    self.spinner.finish("Response", &self.theme, &mut out).ok();
                    self.spinner_active = false;
                }
                self.saw_text = true;
                self.assistant_text.push_str(delta.as_ref());
                print!("{}", delta.as_str().with(self.theme.muted()));
                out.flush().ok();
            }
            AgentEvent::ThinkingDelta(delta) => {
                print!("{}", delta.as_str().with(self.theme.muted()).italic());
                io::stdout().flush().ok();
            }
            AgentEvent::ToolUse { name, .. } => {
                if self.spinner_active {
                    self.spinner
                        .tick(&format!("Running `{name}`"), &self.theme, &mut out)
                        .ok();
                } else {
                    println!("{}", format!("· running `{name}`").with(self.theme.muted()));
                }
            }
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
                ..
            } => {
                let label = if *is_error {
                    format!("`{name}` failed")
                } else {
                    format!("`{name}` done")
                };
                if self.spinner_active {
                    self.spinner.finish(&label, &self.theme, &mut out).ok();
                    self.spinner_active = false;
                } else {
                    println!("{}", format!("· {label}").with(self.theme.muted()));
                }
                let markdown = fold_tool_output(&self.state, name, output, FOLD_TOOL_OUTPUT_LINES);
                writeln!(out, "{}", self.renderer.render_markdown(&markdown)).ok();
                out.flush().ok();
            }
            AgentEvent::Usage(usage) => {
                self.last_usage = Some(*usage);
            }
            AgentEvent::MessageStop => {
                if !self.assistant_text.is_empty() {
                    writeln!(out).ok();
                    writeln!(
                        out,
                        "{}",
                        self.renderer.render_markdown(&self.assistant_text)
                    )
                    .ok();
                    self.assistant_text.clear();
                    out.flush().ok();
                }
            }
            AgentEvent::Truncated(reason) => {
                if self.spinner_active {
                    self.spinner.finish("Truncated", &self.theme, &mut out).ok();
                    self.spinner_active = false;
                }
                writeln!(
                    out,
                    "{}",
                    format!("· response truncated by the provider ({reason})")
                        .with(self.theme.muted())
                )
                .ok();
                out.flush().ok();
            }
            AgentEvent::Error(_) => {}
        }
    }
}

/// Deterministic signals the task-loop verifier needs from one interactive
/// turn: whether the runtime errored, how many tool results came back as
/// errors, and a short tail of the assistant's final message for memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnOutcome {
    pub(crate) ok: bool,
    pub(crate) tool_errors: usize,
    pub(crate) conclusion: Option<String>,
}

/// Count tool results flagged as errors across a turn's transcript entries.
pub(crate) fn count_tool_errors(results: &[ConversationMessage]) -> usize {
    results
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
        .count()
}

/// The assistant's final text, truncated for a high-density memory line.
pub(crate) fn last_assistant_conclusion(messages: &[ConversationMessage]) -> Option<String> {
    let text = messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .map(|joined| joined.trim().to_string())
        .filter(|joined| !joined.is_empty())?;
    Some(truncate_chars(&text, 240))
}

/// Run one interactive turn: stream events to the terminal, abort on Ctrl+C,
/// and auto-save the session afterwards.
///
/// Returns a [`TurnOutcome`] the task loop verifies against; the failure is
/// rendered here so callers can keep running.
pub(crate) async fn run_turn_interactive(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    input_text: &str,
    prompter: Option<&mut dyn PermissionPrompter>,
) -> io::Result<TurnOutcome> {
    let cancel = CancellationToken::new();
    // Bound to a name (not `_`) so it lives until the function returns; the
    // guard then aborts the listener on every exit path, including the `?`
    // early-returns below that would otherwise skip an explicit abort.
    let _listener = AbortGuard(tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        }
    }));

    let mut turn = TurnRenderer::new(state.clone());
    let mut stdout = io::stdout();
    turn.spinner.tick("Thinking", &turn.theme, &mut stdout)?;

    let mut notify = |event: &AgentEvent| turn.render(event);
    let result = runtime
        .run_turn_with_blocks(
            expand_attachments(input_text),
            prompter,
            &mut notify,
            &cancel,
        )
        .await;

    let outcome = match result {
        Ok(summary) => {
            if turn.saw_text {
                writeln!(stdout)?;
            } else {
                turn.spinner.finish("Done", &turn.theme, &mut stdout)?;
            }
            TurnOutcome {
                ok: true,
                tool_errors: count_tool_errors(&summary.tool_results),
                conclusion: last_assistant_conclusion(&summary.assistant_messages),
            }
        }
        Err(error) => {
            let interrupted = error.to_string().contains("cancelled");
            if turn.spinner_active {
                if interrupted {
                    turn.spinner
                        .cancel("Interrupted", &turn.theme, &mut stdout)?;
                } else {
                    turn.spinner.fail("Turn failed", &turn.theme, &mut stdout)?;
                }
            } else {
                writeln!(stdout)?;
            }
            if interrupted {
                println!(
                    "{}",
                    "turn interrupted; the transcript stays consistent - give the next instruction or /exit to quit"
                        .dark_grey()
                );
            } else {
                println!("{}", format!("\u{2718} {error}").red());
            }
            if let Ok(path) = save_session_async(state, runtime.session()).await {
                println!(
                    "{}",
                    format!("· session saved to {}", path.display()).dark_grey()
                );
            }
            return Ok(TurnOutcome {
                ok: false,
                tool_errors: 0,
                conclusion: None,
            });
        }
    };

    if let Some(usage) = turn.last_usage {
        println!(
            "{}",
            format!(
                "[in {} / out {} / cache read {}]",
                usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
            )
            .dark_grey()
        );
    }
    if let Ok(path) = save_session_async(state, runtime.session()).await {
        println!("{}", format!("· saved {}", path.display()).dark_grey());
    }
    Ok(outcome)
}
