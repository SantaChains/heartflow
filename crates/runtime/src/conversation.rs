use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::compact::{
    compact_session, estimate_session_tokens, should_compact, truncate_chars, CompactionConfig,
    CompactionResult,
};
use crate::permissions::{PermissionOutcome, PermissionPolicy, PermissionPrompter};
use crate::schema::validate_tool_input;
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::usage::{TokenUsage, UsageTracker};

/// Provider-agnostic tool advertisement sent alongside each request.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiRequest {
    pub system_prompt: Vec<String>,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolSpec>,
}

/// Events that flow from a provider stream and the agent loop to the UI layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    Usage(TokenUsage),
    MessageStop,
    Error(String),
}

/// Live handle to one assistant message stream.
///
/// Dropping the handle closes the channel; `cancel` aborts the producer task.
pub struct TurnStream {
    rx: mpsc::Receiver<AgentEvent>,
    abort: AbortHandle,
}

impl TurnStream {
    #[must_use]
    pub fn new(rx: mpsc::Receiver<AgentEvent>, abort: AbortHandle) -> Self {
        Self { rx, abort }
    }

    /// Replay a fixed event sequence; used by tests and scripted providers.
    #[must_use]
    pub fn from_events(events: Vec<AgentEvent>) -> Self {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        let handle = tokio::spawn(async move {
            for event in events {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        Self {
            rx,
            abort: handle.abort_handle(),
        }
    }

    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.rx.recv().await
    }

    pub fn cancel(&self) {
        self.abort.abort();
    }
}

pub trait ApiClient: Send {
    /// Start streaming one assistant message for `request`.
    ///
    /// The returned stream must be producing events by the time it is polled;
    /// providers run their transport on a spawned task internally.
    fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError>;
}

pub trait ToolExecutor: Send + Sync + 'static {
    fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError>;

    /// Tool advertisements forwarded with each API request. Scripted and
    /// test executors default to none.
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// Unfinished tasks in the agent's plan ledger. When the model ends its
    /// message while this is non-zero, the loop nudges it to continue instead
    /// of ending the turn; executors without a plan ledger report zero.
    fn pending_tasks(&self) -> usize {
        0
    }

    /// Seed the plan ledger deterministically from a plan document, bypassing
    /// the model. `input` uses the same JSON shape as `todo_write`. Executors
    /// without a plan ledger reject the request.
    fn seed_plan(&self, _input: &str) -> Result<String, ToolError> {
        Err(ToolError::new("no plan ledger to seed"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub iterations: usize,
    pub continuations: usize,
    pub usage: TokenUsage,
}

/// Nudge injected as a user message when the model stops with unfinished
/// plan tasks; keeps the transcript API-consistent and resume-safe.
const CONTINUATION_PROMPT: &str = "Continue: your task list still has unfinished items. Work on the next task now and update the todo list as you go.";

/// Tool-result ceiling in characters: larger outputs are head+tail truncated
/// so a runaway command or MCP tool can never blow the model context.
const MAX_TOOL_OUTPUT_CHARS: usize = 32_000;
const TOOL_OUTPUT_HEAD: usize = 24_000;
const TOOL_OUTPUT_TAIL: usize = 6_000;

#[must_use]
fn truncate_tool_output(output: &str) -> String {
    let total = output.chars().count();
    if total <= MAX_TOOL_OUTPUT_CHARS {
        return output.to_string();
    }
    let head: String = output.chars().take(TOOL_OUTPUT_HEAD).collect();
    let tail: String = output.chars().skip(total - TOOL_OUTPUT_TAIL).collect();
    let omitted = total - TOOL_OUTPUT_HEAD - TOOL_OUTPUT_TAIL;
    format!(
        "{head}\n[output truncated: {total} chars total, {omitted} middle chars omitted]\n{tail}"
    )
}

/// Trailing messages whose tool-result bodies are replayed verbatim. Older
/// dumps (file reads, command logs, MCP payloads) are collapsed to a stub when
/// building the request, so a long or resumed session never re-feeds stale bulk
/// output. Roughly the last few turns stay intact.
const REPLAY_VERBATIM_TAIL: usize = 12;
/// Older error results carry signal, so a short head of the body survives.
const REPLAY_ERROR_HEAD_CHARS: usize = 400;

/// Project the durable transcript into the message list actually sent to the
/// provider. Structure and ordering are preserved (each `tool_use` stays paired
/// with its `tool_result`); only the *body* of a tool result older than the
/// verbatim tail is replaced with a compact marker. The session keeps full
/// output on disk; pinned messages are never rewritten.
#[must_use]
fn build_replay_messages(messages: &[ConversationMessage]) -> Vec<ConversationMessage> {
    let verbatim_from = messages.len().saturating_sub(REPLAY_VERBATIM_TAIL);
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if index >= verbatim_from
                || message.pinned
                || !message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            {
                return message.clone();
            }
            let blocks = message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        tool_name,
                        output,
                        is_error,
                    } => {
                        let stubbed = if *is_error {
                            let head = truncate_chars(output, REPLAY_ERROR_HEAD_CHARS);
                            format!("[earlier error result: {head}]")
                        } else {
                            let total = output.chars().count();
                            format!(
                                "[{tool_name} result omitted from replay: {total} chars; re-run the tool or read the file if it is still needed]"
                            )
                        };
                        ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            tool_name: tool_name.clone(),
                            output: stubbed,
                            is_error: *is_error,
                        }
                    }
                    other => other.clone(),
                })
                .collect();
            ConversationMessage {
                role: message.role,
                blocks,
                usage: message.usage,
                pinned: message.pinned,
            }
        })
        .collect()
}

pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: Arc<T>,
    permission_policy: PermissionPolicy,
    system_prompt: Vec<String>,
    max_iterations: usize,
    max_continuations: usize,
    usage_tracker: UsageTracker,
    /// Window-based pre-compaction policy applied inside `run_turn`. Disabled
    /// unless `context_window_tokens` is set (via `with_compaction`).
    compaction: CompactionConfig,
}

impl<C, T> ConversationRuntime<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    #[must_use]
    pub fn new(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
    ) -> Self {
        let usage_tracker = UsageTracker::from_session(&session);
        Self {
            session,
            api_client,
            tool_executor: Arc::new(tool_executor),
            permission_policy,
            system_prompt,
            max_iterations: 16,
            max_continuations: 3,
            usage_tracker,
            compaction: CompactionConfig::default(),
        }
    }

    /// Enable in-turn window pre-compaction: before every stream, the loop
    /// summarizes older messages once the session crosses half of
    /// `config.context_window_tokens`. A zero window leaves it disabled.
    #[must_use]
    pub fn with_compaction(mut self, config: CompactionConfig) -> Self {
        self.compaction = config;
        self
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    #[must_use]
    pub fn with_max_continuations(mut self, max_continuations: usize) -> Self {
        self.max_continuations = max_continuations;
        self
    }

    /// Run one user turn: stream assistant messages, execute requested tools
    /// concurrently (order preserved in the transcript), and repeat until the
    /// model stops calling tools.
    ///
    /// `notify` observes every event as it happens; `cancel` aborts the turn
    /// at the next await point while keeping the session API-consistent.
    pub async fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> Result<TurnSummary, RuntimeError> {
        self.session
            .messages
            .push(ConversationMessage::user_text(user_input.into()));

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut iterations = 0usize;
        let mut continuations = 0usize;

        loop {
            iterations += 1;
            if iterations > self.max_iterations {
                return Err(RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations",
                ));
            }

            // Hermes-style >50% pre-compaction: shrink the context before
            // spending a request on it, but only when a window is configured.
            if self.compaction.context_window_tokens > 0
                && should_compact(&self.session, self.compaction)
            {
                self.compact(self.compaction);
            }

            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: build_replay_messages(&self.session.messages),
                tools: self.tool_executor.specs(),
            };
            let mut stream = self.api_client.stream(request)?;

            let (blocks, usage, finished, cancelled, stream_error) =
                Self::consume_stream(&mut stream, notify, cancel).await;

            if let Some(message) = stream_error {
                return Err(RuntimeError::new(message));
            }
            if !finished {
                // Preserve streamed text so the transcript stays API-consistent,
                // then surface the interruption.
                if !blocks.is_empty() {
                    let message = ConversationMessage::assistant_with_usage(blocks, usage);
                    self.session.messages.push(message.clone());
                    assistant_messages.push(message);
                }
                let reason = if cancelled {
                    "turn cancelled"
                } else {
                    "assistant stream ended without a message stop event"
                };
                return Err(RuntimeError::new(reason));
            }
            if blocks.is_empty() {
                return Err(RuntimeError::new("assistant stream produced no content"));
            }

            if let Some(value) = usage {
                self.usage_tracker.record(value);
            }

            let pending_tool_uses = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();

            let assistant_message = ConversationMessage::assistant_with_usage(blocks, usage);
            self.session.messages.push(assistant_message.clone());
            assistant_messages.push(assistant_message);

            if pending_tool_uses.is_empty() {
                // Self-iteration: with unfinished plan tasks, nudge the model
                // to keep working instead of ending the turn early.
                if self.tool_executor.pending_tasks() > 0 && continuations < self.max_continuations
                {
                    continuations += 1;
                    self.session.messages.push(ConversationMessage::user_text(
                        CONTINUATION_PROMPT.to_string(),
                    ));
                    continue;
                }
                break;
            }

            let allowed =
                self.authorize_tools(&pending_tool_uses, &mut prompter, &mut tool_results, notify);
            let allowed = self.validate_tool_inputs(allowed, &mut tool_results, notify);
            if allowed.is_empty() {
                continue;
            }

            let interrupted = self
                .execute_tools(allowed, &mut tool_results, notify, cancel)
                .await;
            if interrupted {
                return Err(RuntimeError::new("turn cancelled"));
            }
        }

        Ok(TurnSummary {
            assistant_messages,
            tool_results,
            iterations,
            continuations,
            usage: self.usage_tracker.cumulative_usage(),
        })
    }

    async fn consume_stream(
        stream: &mut TurnStream,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> (
        Vec<ContentBlock>,
        Option<TokenUsage>,
        bool,
        bool,
        Option<String>,
    ) {
        let mut text = String::new();
        let mut blocks = Vec::new();
        let mut usage = None;
        let mut finished = false;
        let mut cancelled = false;
        let mut error = None;

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    stream.cancel();
                    cancelled = true;
                }
                event = stream.recv() => match event {
                    None => break,
                    Some(AgentEvent::TextDelta(delta)) => {
                        notify(&AgentEvent::TextDelta(delta.clone()));
                        text.push_str(&delta);
                    }
                    Some(AgentEvent::ThinkingDelta(delta)) => {
                        notify(&AgentEvent::ThinkingDelta(delta.clone()));
                    }
                    Some(AgentEvent::ToolUse { id, name, input }) => {
                        notify(&AgentEvent::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        });
                        flush_text_block(&mut text, &mut blocks);
                        blocks.push(ContentBlock::ToolUse { id, name, input });
                    }
                    Some(AgentEvent::Usage(value)) => {
                        notify(&AgentEvent::Usage(value));
                        usage = Some(value);
                    }
                    Some(AgentEvent::MessageStop) => {
                        finished = true;
                        break;
                    }
                    Some(AgentEvent::Error(message)) => {
                        error = Some(message);
                        break;
                    }
                    // ToolResult events are produced by the runtime itself.
                    Some(AgentEvent::ToolResult { .. }) => {}
                },
            }
            if cancelled {
                break;
            }
        }

        flush_text_block(&mut text, &mut blocks);
        (blocks, usage, finished, cancelled, error)
    }

    fn authorize_tools(
        &mut self,
        pending_tool_uses: &[(String, String, String)],
        prompter: &mut Option<&mut dyn PermissionPrompter>,
        tool_results: &mut Vec<ConversationMessage>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
    ) -> Vec<(String, String, String)> {
        let mut allowed = Vec::new();
        for (tool_use_id, tool_name, input) in pending_tool_uses {
            // Explicit deref reborrow: `as_deref_mut` funnels the borrow into
            // the invariant `&mut dyn` lifetime, while `&mut **p` yields a
            // fresh short borrow per iteration.
            let outcome = match prompter {
                Some(prompter) => {
                    self.permission_policy
                        .authorize(tool_name, input, Some(&mut **prompter))
                }
                None => self.permission_policy.authorize(tool_name, input, None),
            };
            match outcome {
                PermissionOutcome::Allow => {
                    allowed.push((tool_use_id.clone(), tool_name.clone(), input.clone()));
                }
                PermissionOutcome::Deny { reason } => {
                    notify(&AgentEvent::ToolResult {
                        id: tool_use_id.clone(),
                        name: tool_name.clone(),
                        output: reason.clone(),
                        is_error: true,
                    });
                    let message =
                        ConversationMessage::tool_result(tool_use_id, tool_name, reason, true);
                    self.session.messages.push(message.clone());
                    tool_results.push(message);
                }
            }
        }
        allowed
    }

    /// Drop tools whose arguments violate the advertised `input_schema`, before
    /// running any of them. A malformed call becomes an in-band error result so
    /// the model sees why and can correct itself, instead of reaching an
    /// executor that would misparse it. Tools with no (or a permissive) schema
    /// always pass — validation never blocks a call it cannot reason about.
    fn validate_tool_inputs(
        &mut self,
        allowed: Vec<(String, String, String)>,
        tool_results: &mut Vec<ConversationMessage>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
    ) -> Vec<(String, String, String)> {
        if allowed.is_empty() {
            return allowed;
        }
        let schemas: BTreeMap<String, serde_json::Value> = self
            .tool_executor
            .specs()
            .into_iter()
            .map(|spec| (spec.name, spec.input_schema))
            .collect();
        let mut valid = Vec::new();
        for (tool_use_id, tool_name, input) in allowed {
            if let Some(schema) = schemas.get(&tool_name) {
                if let Err(reason) = validate_tool_input(schema, &input) {
                    let output = format!("invalid tool input: {reason}");
                    notify(&AgentEvent::ToolResult {
                        id: tool_use_id.clone(),
                        name: tool_name.clone(),
                        output: output.clone(),
                        is_error: true,
                    });
                    let message =
                        ConversationMessage::tool_result(&tool_use_id, &tool_name, output, true);
                    self.session.messages.push(message.clone());
                    tool_results.push(message);
                    continue;
                }
            }
            valid.push((tool_use_id, tool_name, input));
        }
        valid
    }

    /// Execute allowed tools concurrently on the blocking pool, emitting
    /// `ToolResult` events as they complete while appending transcript entries
    /// in submission order. Returns true when cancelled mid-flight.
    async fn execute_tools(
        &mut self,
        allowed: Vec<(String, String, String)>,
        tool_results: &mut Vec<ConversationMessage>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> bool {
        let mut join_set = JoinSet::new();
        for (index, (_, tool_name, input)) in allowed.iter().enumerate() {
            let executor = Arc::clone(&self.tool_executor);
            let tool_name = tool_name.clone();
            let input = input.clone();
            join_set.spawn_blocking(move || {
                let result = executor.execute(&tool_name, &input);
                (index, result)
            });
        }

        let mut outcomes: Vec<Option<Result<String, ToolError>>> =
            (0..allowed.len()).map(|_| None).collect();
        let mut interrupted = false;
        while !join_set.is_empty() {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    interrupted = true;
                }
                joined = join_set.join_next() => match joined {
                    Some(Ok((index, result))) => outcomes[index] = Some(result),
                    Some(Err(error)) => {
                        if let Some(index) = outcomes.iter().position(Option::is_none) {
                            outcomes[index] =
                                Some(Err(ToolError::new(format!("tool task failed: {error}"))));
                        }
                    }
                    None => break,
                },
            }
            if interrupted {
                join_set.abort_all();
                break;
            }
        }

        for ((tool_use_id, tool_name, _), slot) in allowed.iter().zip(outcomes) {
            let (output, is_error) = match slot {
                Some(Ok(output)) => (truncate_tool_output(&output), false),
                Some(Err(error)) => (truncate_tool_output(&error.to_string()), true),
                None => (String::from("[interrupted by user]"), true),
            };
            notify(&AgentEvent::ToolResult {
                id: tool_use_id.clone(),
                name: tool_name.clone(),
                output: output.clone(),
                is_error,
            });
            let message =
                ConversationMessage::tool_result(tool_use_id, tool_name, output, is_error);
            self.session.messages.push(message.clone());
            tool_results.push(message);
        }
        interrupted
    }

    /// Compact the session in place, keeping a resumable system summary.
    pub fn compact(&mut self, config: CompactionConfig) -> CompactionResult {
        let result = compact_session(&self.session, config);
        self.session = result.compacted_session.clone();
        result
    }

    /// Toggle the pin flag on the most recent non-system message and return its
    /// new state (`Some(true)` now pinned / `Some(false)` now unpinned), or
    /// `None` when nothing pinnable exists yet. A pinned message is never folded
    /// into a compaction summary - it survives verbatim across every compaction.
    pub fn toggle_pin_latest(&mut self) -> Option<bool> {
        let message = self
            .session
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.role != MessageRole::System)?;
        message.pinned = !message.pinned;
        Some(message.pinned)
    }

    /// Number of messages currently pinned against compaction.
    #[must_use]
    pub fn pinned_count(&self) -> usize {
        self.session.messages.iter().filter(|m| m.pinned).count()
    }

    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_session_tokens(&self.session)
    }

    #[must_use]
    pub fn usage(&self) -> &UsageTracker {
        &self.usage_tracker
    }

    /// Tool advertisements currently forwarded with each API request; used by
    /// the UI to list native and MCP tools.
    #[must_use]
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tool_executor.specs()
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Borrow the shared tool executor. Lets an orchestrator read executor
    /// state (e.g. the todo ledger) without cloning or rebuilding the runtime.
    #[must_use]
    pub fn executor(&self) -> &T {
        &self.tool_executor
    }

    /// The active compaction policy. Lets the CLI reuse the same window and
    /// `preserve_recent_messages` for `/compact` and after-turn auto-compaction
    /// instead of re-reading config or env a second time.
    #[must_use]
    pub fn compaction(&self) -> CompactionConfig {
        self.compaction
    }

    /// Start the next task from a clean context: replace the session messages
    /// with `seeds` while keeping the connected client, tool executor, system
    /// prompt, permission policy and compaction config untouched (so MCP stays
    /// connected and cumulative usage is preserved). This is the fresh-context
    /// per-task reset the Hermes loop relies on.
    pub fn reset_for_task(&mut self, seeds: Vec<ConversationMessage>) {
        self.session.messages = seeds;
    }

    /// Swap the permission policy in place. Used to move a live session between
    /// planning (read-only + gated plan writes) and execution (workspace-write)
    /// without rebuilding the runtime, which would reconnect MCP servers and
    /// drop the shared todo ledger.
    pub fn set_permission_policy(&mut self, permission_policy: PermissionPolicy) {
        self.permission_policy = permission_policy;
    }

    /// Deterministically seed the todo ledger from an approved plan document so
    /// execution follows the checked-in task list rather than the model's
    /// re-reading of it.
    pub fn seed_plan(&self, input: &str) -> Result<String, ToolError> {
        self.tool_executor.seed_plan(input)
    }

    #[must_use]
    pub fn into_session(self) -> Session {
        self.session
    }
}

fn flush_text_block(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: std::mem::take(text),
        });
    }
}

type ToolHandler = Arc<dyn Fn(&str) -> Result<String, ToolError> + Send + Sync>;

#[derive(Clone, Default)]
pub struct StaticToolExecutor {
    handlers: BTreeMap<String, ToolHandler>,
}

impl StaticToolExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn register(
        mut self,
        tool_name: impl Into<String>,
        handler: impl Fn(&str) -> Result<String, ToolError> + Send + Sync + 'static,
    ) -> Self {
        self.handlers.insert(tool_name.into(), Arc::new(handler));
        self
    }
}

impl ToolExecutor for StaticToolExecutor {
    fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.handlers
            .get(tool_name)
            .ok_or_else(|| ToolError::new(format!("unknown tool: {tool_name}")))?(input)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentEvent, ApiClient, ApiRequest, ConversationRuntime, RuntimeError, StaticToolExecutor,
        ToolError, ToolExecutor, ToolSpec, TurnStream,
    };
    use crate::compact::CompactionConfig;
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPromptBuilder};
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
    use crate::usage::TokenUsage;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    fn noop_notify() -> impl FnMut(&AgentEvent) + Send {
        |_| {}
    }

    struct ScriptedApiClient {
        call_count: usize,
    }

    impl ApiClient for ScriptedApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
            self.call_count += 1;
            match self.call_count {
                1 => {
                    assert!(request
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::User));
                    Ok(TurnStream::from_events(vec![
                        AgentEvent::TextDelta("Let me calculate that.".to_string()),
                        AgentEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: "2,2".to_string(),
                        },
                        AgentEvent::Usage(TokenUsage {
                            input_tokens: 20,
                            output_tokens: 6,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 2,
                        }),
                        AgentEvent::MessageStop,
                    ]))
                }
                2 => {
                    let last_message = request
                        .messages
                        .last()
                        .expect("tool result should be present");
                    assert_eq!(last_message.role, MessageRole::Tool);
                    Ok(TurnStream::from_events(vec![
                        AgentEvent::TextDelta("The answer is 4.".to_string()),
                        AgentEvent::Usage(TokenUsage {
                            input_tokens: 24,
                            output_tokens: 4,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 3,
                        }),
                        AgentEvent::MessageStop,
                    ]))
                }
                _ => Err(RuntimeError::new("unexpected extra API call")),
            }
        }
    }

    struct PromptAllowOnce;

    impl PermissionPrompter for PromptAllowOnce {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "add");
            PermissionPromptDecision::Allow
        }
    }

    #[tokio::test]
    async fn runs_user_to_tool_to_result_loop_end_to_end_and_tracks_usage() {
        let api_client = ScriptedApiClient { call_count: 0 };
        let tool_executor = StaticToolExecutor::new().register("add", |input| {
            let total = input
                .split(',')
                .map(|part| part.parse::<i32>().expect("input must be valid integer"))
                .sum::<i32>();
            Ok(total.to_string())
        });
        let permission_policy = PermissionPolicy::new(PermissionMode::Prompt);
        let system_prompt = SystemPromptBuilder::new()
            .with_project_context(ProjectContext {
                cwd: PathBuf::from("/tmp/project"),
                current_date: "2026-03-31".to_string(),
                git_status: None,
                instruction_files: Vec::new(),
            })
            .with_os("linux", "6.8")
            .build();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
        );

        let mut events = Vec::new();
        let summary = runtime
            .run_turn(
                "what is 2 + 2?",
                Some(&mut PromptAllowOnce),
                &mut |event| events.push(event.clone()),
                &CancellationToken::new(),
            )
            .await
            .expect("conversation loop should succeed");

        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.assistant_messages.len(), 2);
        assert_eq!(summary.tool_results.len(), 1);
        assert_eq!(runtime.session().messages.len(), 4);
        assert_eq!(summary.usage.output_tokens, 10);
        assert!(matches!(
            runtime.session().messages[1].blocks[1],
            ContentBlock::ToolUse { .. }
        ));
        assert!(matches!(
            runtime.session().messages[2].blocks[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
        assert!(events.contains(&AgentEvent::ToolResult {
            id: "tool-1".to_string(),
            name: "add".to_string(),
            output: "4".to_string(),
            is_error: false,
        }));
    }

    #[tokio::test]
    async fn records_denied_tool_results_when_prompt_rejects() {
        struct RejectPrompter;
        impl PermissionPrompter for RejectPrompter {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }

        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(TurnStream::from_events(vec![
                        AgentEvent::TextDelta("I could not use the tool.".to_string()),
                        AgentEvent::MessageStop,
                    ]));
                }
                Ok(TurnStream::from_events(vec![
                    AgentEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: "secret".to_string(),
                    },
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Prompt),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn(
                "use the tool",
                Some(&mut RejectPrompter),
                &mut noop_notify(),
                &CancellationToken::new(),
            )
            .await
            .expect("conversation should continue after denied tool");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { is_error: true, output, .. } if output == "not now"
        ));
    }

    #[tokio::test]
    async fn reconstructs_usage_tracker_from_restored_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("done".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let mut session = Session::new();
        session
            .messages
            .push(crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "earlier".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                }),
            ));

        let runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );

        assert_eq!(runtime.usage().turns(), 1);
        assert_eq!(runtime.usage().cumulative_usage().total_tokens(), 21);
    }

    #[tokio::test]
    async fn compacts_session_after_turns() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("done".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );
        runtime
            .run_turn("a", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn a");
        runtime
            .run_turn("b", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn b");
        runtime
            .run_turn("c", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn c");

        let result = runtime.compact(CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
            ..CompactionConfig::default()
        });
        assert!(result.summary.contains("Conversation summary"));
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert_eq!(runtime.session().messages[0].role, MessageRole::System);
    }

    #[tokio::test]
    async fn premature_stream_end_preserves_partial_assistant() {
        struct TruncatedApiClient;
        impl ApiClient for TruncatedApiClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("partial".to_string()),
                    // MessageStop never arrives.
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TruncatedApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );

        let error = runtime
            .run_turn("hi", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect_err("truncated stream should fail");
        assert!(error.to_string().contains("message stop"));

        // User message plus the partial assistant message are preserved.
        assert_eq!(runtime.session().messages.len(), 2);
        assert_eq!(runtime.session().messages[0].role, MessageRole::User);
    }

    #[tokio::test]
    async fn cancellation_before_first_event_keeps_only_user_message() {
        struct StreamingApiClient;
        impl ApiClient for StreamingApiClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("never consumed".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            StreamingApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );

        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = runtime
            .run_turn("hi", None, &mut noop_notify(), &cancel)
            .await
            .expect_err("cancelled turn should fail");
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(runtime.session().messages.len(), 1);
        assert_eq!(runtime.session().messages[0].role, MessageRole::User);
    }

    #[tokio::test]
    async fn api_request_carries_executor_specs() {
        struct SpecExecutor;

        impl ToolExecutor for SpecExecutor {
            fn execute(&self, _tool_name: &str, _input: &str) -> Result<String, ToolError> {
                Err(ToolError::new("unused"))
            }

            fn specs(&self) -> Vec<ToolSpec> {
                vec![ToolSpec {
                    name: "probe".to_string(),
                    description: "test tool".to_string(),
                    input_schema: serde_json::json!({ "type": "object" }),
                }]
            }
        }

        struct AssertClient;

        impl ApiClient for AssertClient {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                assert_eq!(request.tools.len(), 1);
                assert_eq!(request.tools[0].name, "probe");
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("ok".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            AssertClient,
            SpecExecutor,
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );
        runtime
            .run_turn("hi", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn should succeed");
    }

    /// A tool whose advertised schema requires `path`.
    struct SchemaExecutor {
        ran: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl ToolExecutor for SchemaExecutor {
        fn execute(&self, _tool_name: &str, _input: &str) -> Result<String, ToolError> {
            self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("executed".to_string())
        }
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "writer".to_string(),
                description: "needs a path".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": { "type": "string" } }
                }),
            }]
        }
    }

    /// Streams one `writer` call with the given (invalid) input, then a reply.
    struct OneToolClient {
        input: &'static str,
        call_count: usize,
    }
    impl ApiClient for OneToolClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
            self.call_count += 1;
            if self.call_count == 1 {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::ToolUse {
                        id: "c1".to_string(),
                        name: "writer".to_string(),
                        input: self.input.to_string(),
                    },
                    AgentEvent::MessageStop,
                ]))
            } else {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("done".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }
    }

    #[tokio::test]
    async fn schema_invalid_tool_call_is_rejected_without_executing() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            OneToolClient {
                input: r#"{"nope":1}"#,
                call_count: 0,
            },
            SchemaExecutor {
                ran: std::sync::Arc::clone(&ran),
            },
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );
        let summary = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn survives a rejected call");
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "must not execute"
        );
        assert!(
            matches!(
                &summary.tool_results[0].blocks[0],
                ContentBlock::ToolResult { is_error: true, output, .. } if output.contains("invalid tool input")
            ),
            "actual: {:?}",
            summary.tool_results[0].blocks[0]
        );
    }

    #[tokio::test]
    async fn schema_valid_tool_call_executes() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            OneToolClient {
                input: r#"{"path":"a.txt"}"#,
                call_count: 0,
            },
            SchemaExecutor {
                ran: std::sync::Arc::clone(&ran),
            },
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );
        let summary = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn should succeed");
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    struct PlanExecutor {
        pending: usize,
    }

    impl ToolExecutor for PlanExecutor {
        fn execute(&self, _tool_name: &str, _input: &str) -> Result<String, ToolError> {
            Err(ToolError::new("unused"))
        }

        fn pending_tasks(&self) -> usize {
            self.pending
        }
    }

    /// Returns one plain text message per call, never calling tools.
    struct TextOnlyClient {
        call_count: usize,
    }

    impl ApiClient for TextOnlyClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
            self.call_count += 1;
            Ok(TurnStream::from_events(vec![
                AgentEvent::TextDelta(format!("reply {}", self.call_count)),
                AgentEvent::MessageStop,
            ]))
        }
    }

    #[tokio::test]
    async fn continues_turn_while_plan_tasks_remain() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TextOnlyClient { call_count: 0 },
            PlanExecutor { pending: 1 },
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("do it", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn should keep iterating until the plan is done");

        assert_eq!(summary.iterations, 4);
        assert_eq!(summary.continuations, 3);
        // user, reply 1, nudge, reply 2, nudge, reply 3, nudge, reply 4.
        assert_eq!(runtime.session().messages.len(), 8);
        assert_eq!(runtime.session().messages[2].role, MessageRole::User);
        let nudge = &runtime.session().messages[2].blocks[0];
        assert!(matches!(nudge, ContentBlock::Text { text } if text.contains("Continue")));
    }

    #[tokio::test]
    async fn continuation_cap_ends_the_turn() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TextOnlyClient { call_count: 0 },
            PlanExecutor { pending: 1 },
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        )
        .with_max_continuations(1);

        let summary = runtime
            .run_turn("do it", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn should stop at the continuation cap");

        // One nudge, then the turn ends even though tasks remain.
        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.continuations, 1);
        assert_eq!(runtime.session().messages.len(), 4);
    }

    #[tokio::test]
    async fn empty_plan_ledger_never_continues() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TextOnlyClient { call_count: 0 },
            PlanExecutor { pending: 0 },
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("do it", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn should end after one message");

        assert_eq!(summary.iterations, 1);
        assert_eq!(summary.continuations, 0);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[tokio::test]
    async fn oversized_tool_output_is_head_tail_truncated() {
        struct EagerClient {
            call_count: usize,
        }
        impl ApiClient for EagerClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                self.call_count += 1;
                if self.call_count == 1 {
                    Ok(TurnStream::from_events(vec![
                        AgentEvent::ToolUse {
                            id: "t1".to_string(),
                            name: "dump".to_string(),
                            input: "{}".to_string(),
                        },
                        AgentEvent::MessageStop,
                    ]))
                } else {
                    Ok(TurnStream::from_events(vec![
                        AgentEvent::TextDelta("done".to_string()),
                        AgentEvent::MessageStop,
                    ]))
                }
            }
        }
        let executor = StaticToolExecutor::new().register("dump", |_| Ok("x".repeat(100_000)));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            EagerClient { call_count: 0 },
            executor,
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn should succeed");

        let truncated = summary.tool_results[0].blocks[0].clone();
        let ContentBlock::ToolResult {
            output, is_error, ..
        } = truncated
        else {
            panic!("expected tool result");
        };
        assert!(!is_error);
        assert!(output.contains("output truncated"));
        assert!(output.chars().count() < 32_000);
    }

    #[tokio::test]
    async fn in_turn_window_compaction_shrinks_context() {
        struct TwoStepClient {
            call_count: usize,
        }
        impl ApiClient for TwoStepClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                self.call_count += 1;
                if self.call_count == 1 {
                    Ok(TurnStream::from_events(vec![
                        AgentEvent::ToolUse {
                            id: "b1".to_string(),
                            name: "big".to_string(),
                            input: "{}".to_string(),
                        },
                        AgentEvent::MessageStop,
                    ]))
                } else {
                    Ok(TurnStream::from_events(vec![
                        AgentEvent::TextDelta("done".to_string()),
                        AgentEvent::MessageStop,
                    ]))
                }
            }
        }
        let executor = StaticToolExecutor::new().register("big", |_| Ok("y".repeat(4000)));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TwoStepClient { call_count: 0 },
            executor,
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        )
        .with_compaction(CompactionConfig {
            preserve_recent_messages: 2,
            context_window_tokens: 100,
            ..CompactionConfig::default()
        });

        runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn should succeed");

        // The pre-stream check compacted the older messages into a leading
        // system summary before the second request was built.
        let messages = runtime.session().messages.clone();
        assert_eq!(messages[0].role, MessageRole::System);
        assert!(matches!(
            &messages[0].blocks[0],
            ContentBlock::Text { text } if text.contains("ran out of context")
        ));
        // The original "go" user turn was folded away, not kept verbatim.
        assert!(!messages
            .iter()
            .any(|message| message.blocks.iter().any(|block| matches!(
                block,
                ContentBlock::Text { text } if text == "go"
            ))));
    }

    #[tokio::test]
    async fn reset_for_task_keeps_config_and_replaces_messages() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("done".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );
        runtime
            .run_turn("first", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("first turn");
        assert_eq!(runtime.session().messages.len(), 2);

        runtime.reset_for_task(vec![ConversationMessage::user_text(
            "prior conclusion".to_string(),
        )]);

        // Only the seed remains; the next turn appends its kickoff on top.
        assert_eq!(runtime.session().messages.len(), 1);
        assert!(matches!(
            &runtime.session().messages[0].blocks[0],
            ContentBlock::Text { text } if text == "prior conclusion"
        ));
        runtime
            .run_turn(
                "second",
                None,
                &mut noop_notify(),
                &CancellationToken::new(),
            )
            .await
            .expect("second turn");
        // fresh context: seed + new user + assistant, none of the first turn.
        assert_eq!(runtime.session().messages.len(), 3);
        assert!(matches!(
            &runtime.session().messages[1].blocks[0],
            ContentBlock::Text { text } if text == "second"
        ));
    }

    #[test]
    fn truncate_tool_output_preserves_head_and_tail() {
        let long = format!("head{}tail", "m".repeat(40_000));
        let truncated = super::truncate_tool_output(&long);
        assert!(truncated.starts_with("head"));
        assert!(truncated.ends_with("tail"));
        assert!(truncated.contains("output truncated"));

        let short = String::from("tiny");
        assert_eq!(super::truncate_tool_output(&short), "tiny");
    }

    #[test]
    fn replay_stubs_old_tool_results_but_keeps_recent_and_pinned() {
        let bulk = "z".repeat(5_000);
        let mut messages = vec![
            // Old successful dump: should collapse to a size marker.
            ConversationMessage::tool_result("t-old", "bash", bulk.clone(), false),
            // Old pinned dump: survives verbatim even though it is old.
            ConversationMessage::tool_result("t-pin", "read_file", bulk.clone(), false)
                .with_pinned(true),
        ];
        // Pad past the verbatim tail so the first two entries fall out of it.
        for i in 0..12 {
            messages.push(ConversationMessage::user_text(format!("filler {i}")));
        }
        // A recent tool result (inside the tail) must stay verbatim.
        messages.push(ConversationMessage::tool_result(
            "t-new",
            "bash",
            bulk.clone(),
            false,
        ));

        let replay = super::build_replay_messages(&messages);

        let old = match &replay[0].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => panic!("expected tool result"),
        };
        assert!(old.contains("omitted from replay"));
        assert!(old.chars().count() < 200);

        let pinned = match &replay[1].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => panic!("expected tool result"),
        };
        assert_eq!(pinned, bulk, "pinned result must survive verbatim");

        let recent = match &replay[replay.len() - 1].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => panic!("expected tool result"),
        };
        assert_eq!(recent, bulk, "recent result must stay verbatim");

        // The durable transcript is never mutated by the projection.
        assert!(matches!(
            &messages[0].blocks[0],
            ContentBlock::ToolResult { output, .. } if output == &bulk
        ));
    }
}
