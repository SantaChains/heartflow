use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::compact::{
    compact_session_in_place, estimate_tokens_from, should_compact_with_estimate, truncate_chars,
    CompactionConfig, CompactionResult, TokenCalibration,
};
use crate::permissions::{PermissionOutcome, PermissionPolicy, PermissionPrompter};
use crate::schema::validate_tool_input;
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::usage::{TokenUsage, UsageTracker};

/// Provider-agnostic tool advertisement sent alongside each request.
///
/// Serializable so a recorded request can be written to disk verbatim (see the
/// provider cassette): replay compares the recorded request against the live
/// one, which turns prompt/tool assembly drift into a test failure rather than
/// a silent divergence.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Serializable for the same reason as [`ToolSpec`]: a cassette stores the whole
/// request so replay can detect that the prompt the loop now builds differs from
/// the one that was recorded.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApiRequest {
    pub system_prompt: Vec<String>,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolSpec>,
}

/// Events that flow from a provider stream and the agent loop to the UI layer.
///
/// Serializable so a provider stream can be recorded to disk and replayed
/// offline (`TurnStream::from_events` is the replay half). This is what makes a
/// real turn reproducible without a network or a provider key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// The provider ended the turn early (`response.incomplete`, e.g. it ran
    /// into `max_output_tokens`). Whatever streamed so far is kept and the turn
    /// still finishes normally; the notice only warns that the answer is partial.
    Truncated(String),
    Error(String),
}

/// Live handle to one assistant message stream.
///
/// The producer runs on a spawned task that ignores send errors, so dropping
/// the receiver alone would leave it reading the network until the stream ends
/// or the read timeout fires. `Drop` therefore aborts the task explicitly,
/// making an early-dropped stream (a `!finished` return, a consumer that stops
/// reading) leak-free across every transport; `cancel` aborts it sooner.
pub struct TurnStream {
    rx: mpsc::Receiver<AgentEvent>,
    abort: AbortHandle,
}

impl Drop for TurnStream {
    fn drop(&mut self) {
        // Idempotent: an explicit `cancel` before the drop is harmless.
        self.abort.abort();
    }
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

    /// Whether `tool_name` mutates nothing that another tool could touch
    /// concurrently, so a run of such tools may overlap in parallel. Defaults
    /// to `false` (fail-closed): an unknown tool is run alone. Executors
    /// override this for their pure-read tools.
    fn is_concurrent_safe(&self, _tool_name: &str) -> bool {
        false
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

/// Trailing messages whose tool-result bodies are replayed verbatim (from
/// `CompactionConfig::replay_verbatim_tail`). Older dumps (file reads, command
/// logs, MCP payloads) collapse to a stub so a long or resumed session never
/// re-feeds stale bulk output; the durable on-disk transcript keeps everything.
/// Older error results carry signal, so a short head of the body survives.
const REPLAY_ERROR_HEAD_CHARS: usize = 400;
/// A result shorter than this is cheaper to keep verbatim than to replace with
/// the stub sentence, so only genuinely bulky dumps are collapsed.
const REPLAY_MIN_STUB_CHARS: usize = 500;

/// Project the durable transcript into the message list actually sent to the
/// provider. Structure and ordering are preserved (each `tool_use` stays paired
/// with its `tool_result`); only the *body* of a bulky block older than the
/// verbatim tail is replaced with a compact marker — a large tool result, or an
/// inline image attachment (folded to a text placeholder, since re-uploading a
/// stale image every turn is the largest recurring context cost). The session
/// keeps full output on disk; pinned messages are never rewritten.
#[must_use]
fn build_replay_messages(
    messages: &[ConversationMessage],
    verbatim_tail: usize,
) -> Vec<ConversationMessage> {
    let verbatim_from = messages.len().saturating_sub(verbatim_tail);
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if index >= verbatim_from
                || message.pinned
                || !message.blocks.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::ToolResult { .. } | ContentBlock::Image { .. }
                    )
                })
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
                        let total = output.chars().count();
                        let worth_stubbing = if *is_error {
                            total > REPLAY_ERROR_HEAD_CHARS
                        } else {
                            total > REPLAY_MIN_STUB_CHARS
                        };
                        if !worth_stubbing {
                            return block.clone();
                        }
                        let stubbed = if *is_error {
                            let head = truncate_chars(output, REPLAY_ERROR_HEAD_CHARS);
                            format!("[earlier error result: {head}]")
                        } else {
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
                    // Stale images are the largest per-turn re-upload cost: the
                    // projection re-sends every surviving attachment on every
                    // turn, and a provider discards pixels past its vision grid
                    // anyway. Fold anything older than the verbatim tail to a
                    // placeholder exactly like a bulky tool result — the on-disk
                    // transcript keeps the real bytes, a pinned message is never
                    // touched, and a recent attachment inside the tail survives.
                    ContentBlock::Image { data, .. } => ContentBlock::Text {
                        text: format!(
                            "[earlier image attachment omitted from replay: {} KB base64; re-attach the file if it is still needed]",
                            data.len() / 1024
                        ),
                    },
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

/// Drop tool results whose matching `tool_use` never appears earlier in the
/// transcript, so the wire request stays provider-valid.
///
/// A `tool_result` (OpenAI `role:tool`, Anthropic `tool_result` block) must
/// answer a preceding tool call; an orphan makes the provider reject the whole
/// request (OpenAI 400 "Messages with role 'tool' must be a response to a
/// preceding message with 'tool_calls'"). Compaction now aligns its cut to
/// avoid creating these, but a resumed or interrupted session can still carry
/// one, so this is the protocol-agnostic safety net applied to every request.
/// Scanning in order, a result survives only once an earlier assistant message
/// has advertised its `tool_use_id`; a `Tool` message left empty is dropped.
#[must_use]
fn repair_tool_pairing(messages: Vec<ConversationMessage>) -> Vec<ConversationMessage> {
    let mut advertised: HashSet<String> = HashSet::new();
    messages
        .into_iter()
        .filter_map(|message| match message.role {
            MessageRole::Assistant => {
                for block in &message.blocks {
                    if let ContentBlock::ToolUse { id, .. } = block {
                        advertised.insert(id.clone());
                    }
                }
                Some(message)
            }
            MessageRole::Tool => {
                let blocks: Vec<ContentBlock> = message
                    .blocks
                    .into_iter()
                    .filter(|block| match block {
                        ContentBlock::ToolResult { tool_use_id, .. } => {
                            advertised.contains(tool_use_id)
                        }
                        _ => true,
                    })
                    .collect();
                if blocks.is_empty() {
                    None
                } else {
                    Some(ConversationMessage {
                        role: message.role,
                        blocks,
                        usage: message.usage,
                        pinned: message.pinned,
                    })
                }
            }
            MessageRole::System | MessageRole::User => Some(message),
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
    /// Amortized-O(1) token-estimate cache: the trusted message-prefix length
    /// and its estimated sum. Appends extend the prefix (the only in-place
    /// mutation); `compact` and `reset_for_task` replace `messages` wholesale
    /// and reset this to zero.
    token_prefix: Cell<(usize, usize)>,
    /// Correction learned from provider-reported usage; see
    /// [`TokenCalibration`]. Applied on top of `token_prefix`, never folded
    /// into it — the cache must keep holding the raw heuristic sum, or every
    /// observation would be fit against an already-corrected value.
    token_calibration: RefCell<TokenCalibration>,
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
            token_prefix: Cell::new((0, 0)),
            token_calibration: RefCell::new(TokenCalibration::default()),
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
        prompter: Option<&mut dyn PermissionPrompter>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> Result<TurnSummary, RuntimeError> {
        self.run_turn_with_blocks(
            vec![ContentBlock::Text {
                text: user_input.into(),
            }],
            prompter,
            notify,
            cancel,
        )
        .await
    }

    /// Same turn loop as [`Self::run_turn`], for user input that carries more
    /// than text (image attachments ride along as sibling blocks).
    pub async fn run_turn_with_blocks(
        &mut self,
        blocks: Vec<ContentBlock>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> Result<TurnSummary, RuntimeError> {
        self.session
            .messages
            .push(ConversationMessage::user_blocks(blocks));

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
            // Honor a Ctrl+C that landed between iterations (e.g. right after a
            // tool batch) instead of spending another request before noticing it.
            if cancel.is_cancelled() {
                return Err(RuntimeError::new("turn cancelled"));
            }

            // Hermes-style >50% pre-compaction: shrink the context before
            // spending a request on it, but only when a window is configured.
            if self.should_compact(self.compaction) {
                self.compact(self.compaction);
            }

            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: repair_tool_pairing(build_replay_messages(
                    &self.session.messages,
                    self.compaction.replay_verbatim_tail,
                )),
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
                // Free ground truth for the estimator. The assistant reply is
                // not appended yet, so `messages` still holds exactly what this
                // request sent — the predicted figure and the provider's report
                // describe the same prompt. The prediction is computed before
                // the borrow: `observe` must never be fed a calibrated value.
                let predicted = self.raw_estimated_tokens();
                self.token_calibration
                    .borrow_mut()
                    .observe(predicted, value.context_input_tokens());
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

            let allowed = self
                .authorize_tools(&pending_tool_uses, &mut prompter, &mut tool_results, notify)
                .await;
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
                    Some(AgentEvent::Truncated(reason)) => {
                        notify(&AgentEvent::Truncated(reason));
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

    async fn authorize_tools(
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
                        .await
                }
                None => {
                    self.permission_policy
                        .authorize(tool_name, input, None)
                        .await
                }
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

    /// Execute allowed tools preserving the model's submission order while
    /// still overlapping work that cannot interfere: maximal runs of
    /// concurrency-safe tools (reads, searches, fetches) run together in
    /// parallel; every state-mutating or interactive tool (bash, write/edit/
    /// patch, `ask_user`, non-read-only MCP tools) runs alone. This stops a
    /// write racing a concurrent read of the same file and keeps two prompts
    /// from fighting for the terminal, while leaving the fast path of several
    /// independent reads untouched. Transcript entries are appended in
    /// submission order regardless of completion order. Returns true when
    /// cancelled mid-flight.
    async fn execute_tools(
        &mut self,
        allowed: Vec<(String, String, String)>,
        tool_results: &mut Vec<ConversationMessage>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> bool {
        let mut outcomes: Vec<Option<Result<String, ToolError>>> =
            (0..allowed.len()).map(|_| None).collect();
        let mut interrupted = false;
        let mut index = 0usize;
        while index < allowed.len() {
            if cancel.is_cancelled() {
                interrupted = true;
                break;
            }
            // A run of consecutive concurrency-safe tools forms one parallel
            // batch; anything else is a batch of exactly one, run alone.
            let start = index;
            if self.tool_executor.is_concurrent_safe(&allowed[index].1) {
                while index < allowed.len()
                    && self.tool_executor.is_concurrent_safe(&allowed[index].1)
                {
                    index += 1;
                }
            } else {
                index += 1;
            }
            let group: Vec<usize> = (start..index).collect();
            if self
                .run_tool_group(&allowed, &group, &mut outcomes, cancel)
                .await
            {
                interrupted = true;
                break;
            }
        }
        self.record_tool_results(&allowed, outcomes, tool_results, notify);
        interrupted
    }

    /// Run one batch of tool calls (`group` indexes into `allowed`) on the
    /// blocking pool, writing each result into its slot. A group is either a
    /// parallel run of concurrency-safe tools or a single mutating tool, so one
    /// `JoinSet` path serves both. Returns true if cancelled before the group
    /// drained, abandoning the in-flight tasks (their slots stay `None`).
    async fn run_tool_group(
        &self,
        allowed: &[(String, String, String)],
        group: &[usize],
        outcomes: &mut [Option<Result<String, ToolError>>],
        cancel: &CancellationToken,
    ) -> bool {
        let mut join_set = JoinSet::new();
        for &index in group {
            let executor = Arc::clone(&self.tool_executor);
            let tool_name = allowed[index].1.clone();
            let input = allowed[index].2.clone();
            join_set.spawn_blocking(move || {
                let result = executor.execute(&tool_name, &input);
                (index, result)
            });
        }

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
                        // A panicked/cancelled task: blame an unfinished slot
                        // in this group so the model still gets a result line.
                        if let Some(&slot) = group.iter().find(|&&i| outcomes[i].is_none()) {
                            outcomes[slot] = Some(Err(ToolError::new(format!(
                                "tool task failed: {error}"
                            ))));
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
        interrupted
    }

    /// Fold execution outcomes into transcript `tool_result` messages in
    /// submission order, emitting the `ToolResult` events and truncating
    /// oversized output. A slot left `None` means the tool never completed
    /// (cancelled) and is recorded as an interruption error.
    fn record_tool_results(
        &mut self,
        allowed: &[(String, String, String)],
        outcomes: Vec<Option<Result<String, ToolError>>>,
        tool_results: &mut Vec<ConversationMessage>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
    ) {
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
    }

    /// Compact the session in place, keeping a resumable system summary.
    /// The gate uses the amortized token estimate; the in-place compaction
    /// itself moves messages instead of cloning them.
    pub fn compact(&mut self, config: CompactionConfig) -> CompactionResult {
        if self.should_compact(config) {
            let result = compact_session_in_place(&mut self.session, config);
            self.token_prefix.set((0, 0));
            result
        } else {
            CompactionResult {
                summary: String::new(),
                removed_message_count: 0,
            }
        }
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

    /// Amortized-O(1) *raw* token estimate: appends only re-score the new tail;
    /// `compact` and `reset_for_task` reset the cached prefix, so the next call
    /// re-scores the (much shorter) session once. Deliberately uncalibrated:
    /// this is the value the cache stores and the input the calibration learns
    /// from, so folding the factor in here would compound it every turn.
    fn raw_estimated_tokens(&self) -> usize {
        let messages = &self.session.messages;
        let (prefix_len, prefix_sum) = self.token_prefix.get();
        let (from, base) = if prefix_len <= messages.len() {
            (prefix_len, prefix_sum)
        } else {
            (0, 0) // defensive: a wholesale replacement nobody reset for
        };
        let total = estimate_tokens_from(messages, from, base);
        self.token_prefix.set((messages.len(), total));
        total
    }

    /// [`Self::raw_estimated_tokens`] corrected by what provider-reported usage
    /// has taught this session about the heuristic's error. The compaction gate
    /// and the status display use this; the error is systematic enough
    /// (content-mix dependent) that a session's own history is the best
    /// available predictor for its next prompt.
    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        let raw = self.raw_estimated_tokens();
        self.token_calibration.borrow().apply(raw)
    }

    /// Same gate as [`should_compact`], fed by the amortized estimate so the
    /// per-request pressure check costs O(new messages) instead of O(session).
    #[must_use]
    pub fn should_compact(&self, config: CompactionConfig) -> bool {
        should_compact_with_estimate(self.session.messages.len(), self.estimated_tokens(), config)
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
        self.token_prefix.set((0, 0));
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
        build_replay_messages, repair_tool_pairing, AgentEvent, ApiClient, ApiRequest,
        ConversationRuntime, RuntimeError, StaticToolExecutor, ToolError, ToolExecutor, ToolSpec,
        TurnStream,
    };
    use crate::compact::{compact_session_in_place, estimate_session_tokens, CompactionConfig};
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPromptBuilder};
    use crate::redact::redact_session;
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
    use crate::usage::TokenUsage;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn repair_drops_a_leading_orphan_and_keeps_the_advertised_pair() {
        let call = ConversationMessage::assistant(vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            input: "{}".to_string(),
        }]);
        let result = ConversationMessage::tool_result("call_1", "bash", "ok", false);
        // An orphan: a tool result whose tool_use never appeared earlier, the
        // shape a mis-aligned compaction boundary used to hand the provider.
        let orphan = ConversationMessage::tool_result("ghost", "bash", "stale", false);

        let repaired = repair_tool_pairing(vec![orphan, call, result]);

        assert_eq!(repaired.len(), 2, "the leading orphan is dropped");
        assert_eq!(repaired[0].role, MessageRole::Assistant);
        assert_eq!(repaired[1].role, MessageRole::Tool);
        assert!(
            matches!(&repaired[1].blocks[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_1"),
            "the advertised pair survives in order"
        );
    }

    #[test]
    fn repair_strips_only_the_orphan_block_from_a_mixed_tool_message() {
        let call = ConversationMessage::assistant(vec![ContentBlock::ToolUse {
            id: "keep".to_string(),
            name: "bash".to_string(),
            input: "{}".to_string(),
        }]);
        let mixed = ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "keep".to_string(),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "ghost".to_string(),
                    tool_name: "bash".to_string(),
                    output: "stale".to_string(),
                    is_error: false,
                },
            ],
            usage: None,
            pinned: false,
        };

        let repaired = repair_tool_pairing(vec![call, mixed]);

        assert_eq!(repaired.len(), 2, "the message itself survives");
        assert_eq!(
            repaired[1].blocks.len(),
            1,
            "only the orphan block is stripped"
        );
        assert!(
            matches!(&repaired[1].blocks[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "keep"),
            "the valid result is retained"
        );
    }

    /// A dropped `TurnStream` must abort its producer task. The driver ignores
    /// send errors, so without the `Drop` abort an early-dropped stream would
    /// leave the task reading the network until the read timeout fires. The
    /// producer here never sends and never ends on its own, so `is_finished`
    /// can only become `true` via the abort.
    #[tokio::test]
    async fn dropping_turn_stream_aborts_producer() {
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(1);
        let handle = tokio::spawn(async move {
            // Hold the sender open and idle; only an external abort ends this.
            let _keep_open = tx;
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        });
        {
            let _stream = TurnStream::new(rx, handle.abort_handle());
            // `_stream` drops at the end of this scope, which must abort.
        }
        // Yield so the runtime processes the abort before we assert.
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        assert!(
            handle.is_finished(),
            "producer task must be aborted when its TurnStream is dropped"
        );
    }

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
        fn decide<'a>(
            &'a mut self,
            request: &'a PermissionRequest,
        ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
            Box::pin(async move {
                assert_eq!(request.tool_name, "add");
                PermissionPromptDecision::Allow
            })
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
            fn decide<'a>(
                &'a mut self,
                _request: &'a PermissionRequest,
            ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
                Box::pin(async move {
                    PermissionPromptDecision::Deny {
                        reason: "not now".to_string(),
                    }
                })
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
        assert_eq!(runtime.session().messages[0].role, MessageRole::System);
    }

    #[test]
    fn incremental_estimate_matches_full_rescan_across_mutations() {
        // The client is never driven: this test only exercises the estimate
        // cache across `reset_for_task` / `compact` mutations.
        struct IdleApiClient;
        impl ApiClient for IdleApiClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Err(RuntimeError::new("not used in this test"))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            IdleApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );
        let config = CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
            ..CompactionConfig::default()
        };
        let seed = |text: &str| {
            vec![
                ConversationMessage::user_text(text.repeat(300)),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "tail".to_string(),
                }]),
                ConversationMessage::user_text("latest".to_string()),
            ]
        };

        // Warm the prefix cache, then re-read: the cached path must agree
        // with a full re-scan.
        runtime.reset_for_task(seed("seed "));
        let warmed = runtime.estimated_tokens();
        assert_eq!(runtime.estimated_tokens(), warmed);
        assert_eq!(
            runtime.estimated_tokens(),
            crate::compact::estimate_session_tokens(runtime.session())
        );

        // Compaction replaces `messages` wholesale; the estimate must recover
        // from the reset cache and stay equal to a full re-scan.
        runtime.compact(config);
        assert_eq!(
            runtime.estimated_tokens(),
            crate::compact::estimate_session_tokens(runtime.session())
        );
        assert!(runtime.estimated_tokens() < warmed, "compaction shrank it");

        // Same for the fresh-context task reset.
        runtime.reset_for_task(seed("other "));
        assert_eq!(
            runtime.estimated_tokens(),
            crate::compact::estimate_session_tokens(runtime.session())
        );
        assert!(runtime.should_compact(config), "gate sees the estimate");
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

    /// The estimator must learn from provider-reported usage, and that learning
    /// must reach `estimated_tokens` (what the compaction gate reads) while
    /// leaving the cached raw sum alone.
    #[tokio::test]
    async fn calibrates_the_estimate_from_reported_usage() {
        struct HeavyUsageClient;
        impl ApiClient for HeavyUsageClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("ok".to_string()),
                    AgentEvent::Usage(TokenUsage {
                        input_tokens: 20_000,
                        output_tokens: 5,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        // The prompt must clear the calibration's minimum sample size, which is
        // why the session is seeded instead of starting empty.
        let mut session = Session::new();
        session
            .messages
            .push(ConversationMessage::user_text("x".repeat(4_000)));
        let mut runtime = ConversationRuntime::new(
            session,
            HeavyUsageClient,
            PlanExecutor { pending: 0 },
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );

        assert_eq!(
            runtime.estimated_tokens(),
            runtime.raw_estimated_tokens(),
            "an uncalibrated runtime must behave exactly as before"
        );

        runtime
            .run_turn("do it", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn should succeed");

        assert_eq!(runtime.usage().current_turn_usage().input_tokens, 20_000);
        let raw = runtime.raw_estimated_tokens();
        // Seeded from the first residual, so the next prediction reproduces the
        // provider's figure instead of re-deriving the ~20x it was missing.
        assert_eq!(runtime.estimated_tokens(), 20_000);
        assert!(runtime.estimated_tokens() > raw * 4);
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
            // Old but tiny result: cheaper to keep than to stub, stays verbatim.
            ConversationMessage::tool_result("t-short", "bash", "ok", false),
        ];
        // Pad past the verbatim tail so the first three entries fall out of it.
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

        let replay = super::build_replay_messages(&messages, 12);

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

        let short = match &replay[2].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => panic!("expected tool result"),
        };
        assert_eq!(
            short, "ok",
            "tiny old result must not be inflated into a stub"
        );

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

    #[test]
    fn replay_folds_old_image_attachments_but_keeps_recent_and_pinned() {
        // A bulky base64 payload standing in for a real attachment.
        let payload = "A".repeat(200 * 1024);
        let image = |tag: &str| ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: format!("{tag}{payload}"),
        };
        let mut messages = vec![
            // Old attachment: folds to a small text placeholder.
            ConversationMessage::user_blocks(vec![image("old")]),
            // Old but pinned attachment: survives verbatim.
            ConversationMessage::user_blocks(vec![image("pin")]).with_pinned(true),
        ];
        // Pad past the verbatim tail so the two entries above fall out of it.
        for i in 0..12 {
            messages.push(ConversationMessage::user_text(format!("filler {i}")));
        }
        // A recent attachment (inside the tail) must stay verbatim.
        messages.push(ConversationMessage::user_blocks(vec![image("new")]));

        let replay = super::build_replay_messages(&messages, 12);

        // The old image became a tiny marker, not a re-uploaded payload, and the
        // block count is preserved (one image in, one text out).
        assert_eq!(replay[0].blocks.len(), 1);
        match &replay[0].blocks[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("image attachment omitted from replay"));
                assert!(text.chars().count() < 200, "marker stays tiny: {text}");
            }
            other => panic!("expected a folded text marker, got {other:?}"),
        }

        // A pinned image survives byte-identical.
        match &replay[1].blocks[0] {
            ContentBlock::Image { data, .. } => {
                assert_eq!(data, &format!("pin{payload}"), "pinned image survives");
            }
            other => panic!("expected the pinned image, got {other:?}"),
        }

        // A recent image (inside the tail) survives byte-identical.
        match &replay[replay.len() - 1].blocks[0] {
            ContentBlock::Image { data, .. } => {
                assert_eq!(data, &format!("new{payload}"), "recent image survives");
            }
            other => panic!("expected the recent image, got {other:?}"),
        }

        // The durable transcript is never mutated by the projection.
        assert!(matches!(&messages[0].blocks[0], ContentBlock::Image { .. }));
    }

    /// Records whether a mutating tool ever overlapped an in-flight read, and
    /// the peak number of concurrent reads, to pin the scheduler contract.
    #[derive(Default)]
    struct SchedulingProbe {
        active_reads: std::sync::atomic::AtomicUsize,
        max_reads: std::sync::atomic::AtomicUsize,
        write_saw_concurrent: std::sync::atomic::AtomicBool,
    }

    struct ProbeExecutor {
        probe: Arc<SchedulingProbe>,
    }

    impl ToolExecutor for ProbeExecutor {
        fn execute(&self, tool_name: &str, _input: &str) -> Result<String, ToolError> {
            use std::sync::atomic::Ordering;
            if tool_name.starts_with("read") {
                let now = self.probe.active_reads.fetch_add(1, Ordering::SeqCst) + 1;
                self.probe.max_reads.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
                self.probe.active_reads.fetch_sub(1, Ordering::SeqCst);
            } else if self.probe.active_reads.load(Ordering::SeqCst) > 0 {
                self.probe
                    .write_saw_concurrent
                    .store(true, Ordering::SeqCst);
            }
            Ok(String::from("ok"))
        }

        fn is_concurrent_safe(&self, tool_name: &str) -> bool {
            tool_name.starts_with("read")
        }
    }

    struct ToolBatchClient {
        call_count: usize,
    }

    impl ApiClient for ToolBatchClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
            self.call_count += 1;
            if self.call_count == 1 {
                // Two independent reads (a parallel batch) then one mutating
                // tool that must not run alongside them.
                Ok(TurnStream::from_events(vec![
                    AgentEvent::ToolUse {
                        id: "a".to_string(),
                        name: "read_a".to_string(),
                        input: "{}".to_string(),
                    },
                    AgentEvent::ToolUse {
                        id: "b".to_string(),
                        name: "read_b".to_string(),
                        input: "{}".to_string(),
                    },
                    AgentEvent::ToolUse {
                        id: "c".to_string(),
                        name: "write_c".to_string(),
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

    #[tokio::test]
    async fn writes_are_serialized_against_concurrent_reads() {
        use std::sync::atomic::Ordering;
        let probe = Arc::new(SchedulingProbe::default());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ToolBatchClient { call_count: 0 },
            ProbeExecutor {
                probe: Arc::clone(&probe),
            },
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["system".to_string()],
        );
        runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn");

        assert!(
            !probe.write_saw_concurrent.load(Ordering::SeqCst),
            "a mutating tool ran while a read was still in flight"
        );
        assert!(
            probe.max_reads.load(Ordering::SeqCst) >= 2,
            "consecutive independent reads should still run in parallel"
        );
    }

    /// Size of the synthetic tool result body, in bytes.
    const TOOL_RESULT_BYTES: usize = 32 * 1024;

    /// One synthetic turn worth of blocks: user text, an assistant text plus
    /// tool call, and a tool result of `TOOL_RESULT_BYTES`. The first turn also
    /// carries a base64 image, mirroring a screenshot that is attached once and
    /// then stays in the transcript for the rest of the session.
    fn synthetic_turn(round: usize, with_image: bool) -> Vec<ConversationMessage> {
        let tool_id = format!("call-{round}");
        let mut blocks = vec![ContentBlock::Text {
            text: format!("turn {round}: explain what this command does and whether it is safe"),
        }];
        if with_image {
            blocks.push(ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "A".repeat(4 * 1024 * 1024),
            });
        }
        vec![
            ConversationMessage {
                role: MessageRole::User,
                blocks,
                usage: None,
                pinned: false,
            },
            ConversationMessage::assistant(vec![
                ContentBlock::Text {
                    text: "reading the file first".to_string(),
                },
                ContentBlock::ToolUse {
                    id: tool_id.clone(),
                    name: "read_file".to_string(),
                    input: "{\"path\":\"src/main.rs\"}".to_string(),
                },
            ]),
            ConversationMessage::tool_result(
                tool_id,
                "read_file",
                "x".repeat(TOOL_RESULT_BYTES),
                false,
            ),
        ]
    }

    fn synthetic_transcript(rounds: usize) -> Session {
        let mut session = Session::new();
        for round in 0..rounds {
            session.messages.extend(synthetic_turn(round, round == 0));
        }
        session
    }

    /// Fastest of `repeat` runs, in microseconds. The probe reports minima
    /// because these stages are pure CPU: the low end is the signal, the tail is
    /// scheduler noise.
    fn timed(repeat: usize, mut step: impl FnMut()) -> u128 {
        let mut best = u128::MAX;
        for _ in 0..repeat {
            let start = std::time::Instant::now();
            step();
            best = best.min(start.elapsed().as_micros());
        }
        best
    }

    /// Cost curve of the work every turn pays regardless of how much the model
    /// says. Run explicitly:
    ///
    /// `cargo test -p heartflow-runtime --lib -- --ignored --nocapture turn_overhead`
    ///
    /// Numbers are synthetic but scale-faithful: the synthetic tool result is
    /// `TOOL_RESULT_BYTES` and the image is 4 MiB of base64, the size a single
    /// attached screenshot reaches.
    #[test]
    #[ignore = "perf probe: run with --ignored --nocapture"]
    fn turn_overhead_grows_with_transcript() {
        let config = CompactionConfig {
            preserve_recent_messages: 4,
            max_estimated_tokens: 10_000,
            context_window_tokens: 128_000,
            replay_verbatim_tail: 12,
        };
        let literals = vec!["sk-live-credential".to_string()];
        println!("tool result = {TOOL_RESULT_BYTES} B, image = 4 MiB base64");
        for rounds in [200usize, 1000, 3000] {
            let session = synthetic_transcript(rounds);
            let messages = session.messages.len();
            println!("rounds={rounds} messages={messages}");
            let estimate = timed(3, || {
                std::hint::black_box(estimate_session_tokens(&session));
            });
            println!("  estimate_session_tokens  {estimate:>8} us");
            let replay = timed(3, || {
                std::hint::black_box(build_replay_messages(
                    &session.messages,
                    config.replay_verbatim_tail,
                ));
            });
            println!("  build_replay_messages    {replay:>8} us");
            let compact = {
                let mut session = synthetic_transcript(rounds);
                let start = std::time::Instant::now();
                std::hint::black_box(compact_session_in_place(&mut session, config));
                start.elapsed()
            };
            println!("  compact_session         {compact:>9?}");
            let redact = timed(3, || {
                std::hint::black_box(redact_session(&session, &literals));
            });
            println!("  redact_session           {redact:>8} us");
            let serialize = timed(3, || {
                std::hint::black_box(
                    serde_json::to_string_pretty(&session).expect("session should serialize"),
                );
            });
            println!("  to_string_pretty         {serialize:>8} us");
            // The transcript is rewritten whole every turn, so its serialized
            // size is also the per-turn disk write.
            let payload = serde_json::to_string_pretty(&session).expect("session should serialize");
            println!(
                "  payload                  {:>8.1} MiB",
                payload.len() as f64 / (1024.0 * 1024.0)
            );
            let path = std::env::temp_dir().join("hf-probe-session.json");
            let write = timed(3, || {
                std::fs::write(&path, &payload).expect("probe write");
            });
            println!("  fs::write                {write:>8} us");
            let _ = std::fs::remove_file(&path);
        }
    }
}
