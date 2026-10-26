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
    auto_record_turn, truncate_chars, turn_reminder, AgentEvent, ContentBlock, ConversationMessage,
    MessageRole, MissionLedger, PermissionPrompter, TokenUsage,
};
use tokio_util::sync::CancellationToken;

use crate::render::{ColorTheme, Spinner, TerminalRenderer};
use crate::theme::glyphs;
use crate::{
    expand_attachments, fold_tool_output, mode_prompt_segment, save_session_async, AgentRuntime,
    SessionShared, FOLD_TOOL_OUTPUT_LINES,
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

/// Byte length of the settled portion of `pending` streamed markdown, if
/// any: everything up to (and including) the last blank line, provided the
/// code fences in that span are balanced (an open fence keeps the whole tail
/// live, since the blank line inside a fence is not a block boundary).
/// Returns `None` when nothing is settled yet.
#[must_use]
fn settle_ready(pending: &str) -> Option<usize> {
    let boundary = pending.rfind("\n\n")? + 2;
    let settled = &pending[..boundary];
    let fences = settled
        .lines()
        .filter(|line| line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~"))
        .count();
    (fences % 2 == 0).then_some(boundary)
}

/// Incremental block-wise markdown streamer (the known "double-write" fix):
/// deltas accumulate into `pending`; each time a block boundary (blank line,
/// balanced fences) appears, that portion is handed out once as settled text,
/// rendered immediately and never redrawn. The trailing live block stays
/// unprinted until it settles or the message stops — no cursor moves, no line
/// accounting, no whole-message reflow jump.
#[derive(Debug, Default)]
struct StreamingMarkdown {
    pending: String,
}

impl StreamingMarkdown {
    /// Absorb one delta; returns the newly settled text (render and print it
    /// once), if any.
    fn push_delta(&mut self, delta: &str) -> Option<String> {
        self.pending.push_str(delta);
        let cut = settle_ready(&self.pending)?;
        let settled = self.pending[..cut].to_string();
        self.pending.drain(..cut);
        Some(settled)
    }

    /// The still-live tail (render at `MessageStop`, or print raw when the
    /// turn fails mid-stream).
    #[must_use]
    fn live_tail(&self) -> &str {
        &self.pending
    }

    /// Drop the live tail after it has been handled (stop or incomplete).
    fn clear(&mut self) {
        self.pending.clear();
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
    /// Number of tools started in this turn; shown in the spinner label so
    /// multi-tool turns give a sense of progress (tool 1 of N).
    tool_step: usize,
    /// Accumulated assistant text for the current message segment; rendered
    /// as finished markdown on `MessageStop`.
    assistant_text: String,
    /// Block-wise streamer: settled blocks print once, the live tail waits.
    stream: StreamingMarkdown,
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
            tool_step: 0,
            assistant_text: String::new(),
            stream: StreamingMarkdown::default(),
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
                // Block-wise streaming (the double-write fix): each settled
                // block (blank-line boundary, balanced fences) is rendered and
                // printed exactly once — never followed by a whole-message
                // reflow. The live tail stays unprinted until it settles or
                // the message stops.
                if let Some(settled) = self.stream.push_delta(delta.as_ref()) {
                    print!("{}", self.renderer.render_markdown(&settled));
                    out.flush().ok();
                }
            }
            AgentEvent::ThinkingDelta(delta) => {
                print!("{}", delta.as_str().with(self.theme.muted()).italic());
                io::stdout().flush().ok();
            }
            AgentEvent::ToolUse { name, .. } => {
                self.tool_step += 1;
                if self.spinner_active {
                    self.spinner
                        .tick(
                            &format!("Tool {}: `{name}`", self.tool_step),
                            &self.theme,
                            &mut out,
                        )
                        .ok();
                } else {
                    println!(
                        "{}",
                        format!("· tool {}: `{name}`", self.tool_step).with(self.theme.muted())
                    );
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
                // Only the still-live tail renders here — settled blocks were
                // already printed as finished markdown during streaming, so
                // the message never appears twice and nothing jumps.
                let tail = self.stream.live_tail().to_string();
                self.stream.clear();
                if !tail.is_empty() {
                    print!("{}", self.renderer.render_markdown(&tail));
                    out.flush().ok();
                }
                self.assistant_text.clear();
            }
            AgentEvent::Truncated(reason) => {
                // Flush the live tail first: an iteration-limit stop can arrive
                // mid-paragraph, and the streamed-but-unsettled text must not
                // vanish behind the truncation note.
                let tail = self.stream.live_tail().to_string();
                self.stream.clear();
                if !tail.is_empty() {
                    print!("{}", self.renderer.render_markdown(&tail));
                    self.assistant_text.clear();
                }
                if self.spinner_active {
                    self.spinner.finish("Truncated", &self.theme, &mut out).ok();
                    self.spinner_active = false;
                }
                writeln!(
                    out,
                    "{}",
                    format!("· turn truncated ({reason})").with(self.theme.muted())
                )
                .ok();
                out.flush().ok();
            }
            // Steered only reaches a turn that armed a steer mailbox; the
            // blocking REPL cannot type mid-turn, so this arm stays dead —
            // kept exhaustive so adding the variant cannot slip by silently.
            AgentEvent::Steered(text) => {
                writeln!(
                    out,
                    "{}",
                    format!("· steered: {text}").with(self.theme.muted())
                )
                .ok();
            }
            AgentEvent::Error { .. } => {}
        }
    }

    /// Render any partially-accumulated assistant text as an incomplete
    /// fragment. Called when a turn fails before `MessageStop` arrives, so
    /// the user keeps visibility into what was generated instead of losing
    /// it behind a bare error line. The output is marked clearly as
    /// incomplete (muted + italic + prefix label) and is intentionally
    /// NOT re-rendered through the full markdown pipeline — a truncated
    /// block can break fences/tables, so raw text with a clear label is
    /// safer and faster.
    fn render_incomplete(&mut self) {
        // Settled blocks were already rendered on arrival; only the live tail
        // is left, printed raw (muted, clearly labelled) because a truncated
        // block can break fences/tables and the full markdown pipeline would
        // garble it.
        let tail = self.stream.live_tail().to_string();
        self.stream.clear();
        if tail.is_empty() {
            return;
        }
        let mut out = io::stdout();
        writeln!(out).ok();
        let label = "· incomplete response";
        writeln!(out, "{}", label.with(self.theme.muted()).italic()).ok();
        let trimmed = tail.trim_end_matches('\n');
        for line in trimmed.lines() {
            writeln!(out, "{}", format!("  {line}").with(self.theme.muted())).ok();
        }
        out.flush().ok();
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
    /// The operator cancelled the turn with Ctrl+C. Distinct from `ok == false`
    /// (a real error) so the loop autopilot can stop on interrupt instead of
    /// counting the cancel as a blocked claim.
    pub(crate) interrupted: bool,
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

/// The per-turn usage line: current turn tokens with the cache hit rate and
/// the session cumulative total, so cost trajectories are visible at a glance
/// (`cache 72%` = cache_read share of this turn's input).
#[must_use]
pub(crate) fn usage_line(current: &TokenUsage, session: &TokenUsage) -> String {
    let read = u64::from(current.cache_read_input_tokens);
    let input = u64::from(current.input_tokens);
    let hit = if input + read > 0 {
        read * 100 / (input + read)
    } else {
        0
    };
    format!(
        "[in {} / out {} / cache {}% · session {}]",
        current.input_tokens,
        current.output_tokens,
        hit,
        session.total_tokens()
    )
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

    // D1.3 mission reminder: project the on-disk ledger into this turn's
    // system prompt. The section is a per-turn projection — set, never
    // persisted — so a mission edited mid-turn is picked up next turn.
    let missions_root = crate::storage::sessions_dir().join("missions");
    let mission_section = turn_reminder(&MissionLedger::load(&missions_root).list());
    // C-1 mode segment: the session's permission mode carries its own contract
    // (plan token-frugal / spec research-first / loop autonomous drive).
    let mode_section = mode_prompt_segment(&crate::storage::lock_state(state).mode);
    let merged = match (mission_section, mode_section) {
        (Some(mission), Some(segment)) => Some(format!(
            "{mission}

{segment}"
        )),
        (Some(mission), None) => Some(mission),
        (None, Some(segment)) => Some(segment.to_string()),
        (None, None) => None,
    };
    runtime.set_turn_context(merged);
    let mut notify = |event: &AgentEvent| turn.render(event);
    let result = runtime
        .run_turn_with_blocks(
            expand_attachments(input_text),
            prompter,
            &mut notify,
            &cancel,
        )
        .await;
    runtime.set_turn_context(None);

    // Auto-account the finished turn against the single active mission
    // (§11.3). Success only: a failed turn is not productive work, so it
    // must not reset a blocked streak or advance the turn budget.
    if let Ok(summary) = &result {
        let tokens = u64::from(summary.usage.input_tokens)
            .saturating_add(u64::from(summary.usage.output_tokens));
        if let Some(attention) = auto_record_turn(&missions_root, tokens) {
            println!("{}", format!("· {attention}").with(turn.theme.muted()));
        }
    }

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
                interrupted: false,
            }
        }
        Err(error) => {
            let interrupted = error.to_string().contains("cancelled");
            turn.render_incomplete();
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
                        .with(turn.theme.muted())
                );
            } else {
                println!(
                    "{}",
                    format!("{} {error}", glyphs::FAILED).with(turn.theme.error())
                );
                // If the error carries a human-readable suggestion (common
                // failure modes like auth errors, rate limits, connection
                // issues), surface it as a muted hint so users know what to
                // try next without having to parse a raw status code.
                if let Some(hint) = error.hint() {
                    println!("{}", format!("  hint: {hint}").with(turn.theme.muted()));
                }
            }
            if let Ok(path) = save_session_async(state, runtime.session()).await {
                println!(
                    "{}",
                    format!("· session saved to {}", path.display()).with(turn.theme.muted())
                );
            }
            return Ok(TurnOutcome {
                ok: false,
                tool_errors: 0,
                conclusion: None,
                interrupted,
            });
        }
    };

    if let Some(usage) = turn.last_usage {
        let cumulative = runtime.usage().cumulative_usage();
        println!(
            "{}",
            usage_line(&usage, &cumulative).with(turn.theme.muted())
        );
    }
    // Silent-save discipline: a successful turn saves without a line (the
    // path is knowable via /status, /sessions and the exit resume hint); only
    // a failed save earns a warning, because that is the anomaly.
    if let Err(error) = save_session_async(state, runtime.session()).await {
        println!(
            "{}",
            format!("· session save failed: {error}").with(turn.theme.error())
        );
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::{settle_ready, StreamingMarkdown};

    #[test]
    fn settle_waits_for_a_blank_line_and_includes_it() {
        // No blank line yet: everything stays live.
        assert_eq!(settle_ready("hello wor"), None);
        assert_eq!(settle_ready("first line\nstill first"), None);
        // A blank line settles everything up to and including it.
        assert_eq!(settle_ready("para one\n\n"), Some("para one\n\n".len()));
        // Only the LAST boundary counts; later live text rides along unsettled.
        let pending = "para one\n\npara two still grow";
        assert_eq!(settle_ready(pending), Some("para one\n\n".len()));
    }

    #[test]
    fn an_open_fence_keeps_the_whole_tail_live() {
        // The blank line inside an unclosed fence is not a block boundary.
        let fenced = "intro\n\n```rust\nfn a() {}\n\nstill inside";
        assert_eq!(settle_ready(fenced), None);
        // Once the fence closes and a blank line follows, settling resumes.
        let closed = "intro\n\n```rust\nfn a() {}\n```\n\ntail";
        assert_eq!(
            settle_ready(closed),
            Some("intro\n\n```rust\nfn a() {}\n```\n\n".len())
        );
    }

    #[test]
    fn tilde_fences_are_boundaries_too_and_cjk_is_safe() {
        let pending = "段落一\n\n~~~\ncode\n";
        assert_eq!(settle_ready(pending), Some("段落一\n\n".len()));
    }

    #[test]
    fn streamer_hands_out_settled_blocks_in_order_and_keeps_the_tail() {
        let mut stream = StreamingMarkdown::default();
        // Grow a first paragraph across deltas: nothing settles until the
        // blank line arrives.
        assert_eq!(stream.push_delta("Hel"), None);
        assert_eq!(stream.push_delta("lo\n\n"), Some("Hello\n\n".to_string()));
        // A fenced block stays live across its internal blank line...
        assert_eq!(stream.push_delta("```sh\nls\n\nmore"), None);
        assert_eq!(stream.live_tail(), "```sh\nls\n\nmore");
        // ...and settles once closed plus a trailing blank line.
        assert_eq!(
            stream.push_delta("\n```\n\nafter"),
            Some("```sh\nls\n\nmore\n```\n\n".to_string())
        );
        assert_eq!(stream.live_tail(), "after");
        // Clear drops the tail (handled by stop/incomplete).
        stream.clear();
        assert_eq!(stream.live_tail(), "");
    }
}
