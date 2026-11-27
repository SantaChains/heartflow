use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::compact::{
    compact_session_in_place_with, estimate_tokens_from, is_context_overflow_error,
    should_compact_with_estimate, truncate_chars, CompactionConfig, CompactionResult, SummaryMode,
    ThresholdKind, TokenCalibration,
};
use crate::permissions::{PermissionOutcome, PermissionPolicy, PermissionPrompter};
use crate::prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
use crate::schema::validate_tool_input;
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::stop::StopPolicy;
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

impl ApiRequest {
    /// Provider-facing rendering: sections joined with a blank line, minus the
    /// internal [`SYSTEM_PROMPT_DYNAMIC_BOUNDARY`] marker. The marker is
    /// cassette-replay machinery (the live/recorded comparison splits on it),
    /// never model-facing text — leaking it into the system string would burn
    /// tokens every turn and show the model plumbing it cannot act on.
    #[must_use]
    pub fn rendered_system_prompt(&self) -> String {
        self.system_prompt
            .iter()
            .filter(|section| section.as_str() != SYSTEM_PROMPT_DYNAMIC_BOUNDARY)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    }
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
        /// Wall time the tool itself ran, in milliseconds. `None` for results
        /// that were never executed (policy denial, invalid input, cancelled
        /// before start) and for recordings made before this field existed —
        /// kept optional so old cassettes still replay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    Usage(TokenUsage),
    MessageStop,
    /// The provider ended the turn early (`response.incomplete`, e.g. it ran
    /// into `max_output_tokens`). Whatever streamed so far is kept and the turn
    /// still finishes normally; the notice only warns that the answer is partial.
    Truncated(String),
    /// A steered user message was absorbed into the live turn at an iteration
    /// boundary (roadmap §11.2, pi steering): the text is already appended to
    /// the session and rides the *next* request as a user message. Emitted
    /// once per consumed steer. Steers that outlive the turn are never
    /// delivered as this event — the caller recovers them (re-queue as a
    /// follow-up) so no typed work is lost.
    Steered(String),
    Error {
        message: String,
        /// Optional human-readable hint for what to try next. Populated by
        /// the provider layer when it recognises a common failure mode (auth
        /// errors, rate limits, connection issues) so the CLI can surface
        /// actionable advice alongside the raw error message.
        hint: Option<String>,
        /// Structured failure class straight from the provider's `ApiError`
        /// (§12 P-B), whenever the transport can classify without guessing
        /// from text. `None` — and every recording made before this field
        /// existed, via the serde default — falls back to
        /// `classify_stream_error`'s text markers.
        #[serde(default)]
        kind: Option<ErrorKind>,
    },
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
    ///
    /// # Errors
    /// Fails when the request cannot be handed to the provider: transport
    /// setup, authentication, or a request rejected up front (e.g. context
    /// overflow). Failures mid-stream arrive as events on the returned
    /// [`TurnStream`], not here.
    fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError>;
}

pub trait ToolExecutor: Send + Sync + 'static {
    /// Execute one tool call with its raw JSON `input`, returning the output
    /// text handed back to the model.
    ///
    /// # Errors
    /// Fails when the tool is unknown, its arguments are invalid, permission
    /// is denied, or the tool's own operation fails; the message is the
    /// tool-facing error text.
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

    /// Flip the session's web-tool availability (roadmap C-1 联网三态; `0` on,
    /// `1` off, `2` local). Default: no web tools to flip, so nothing happens.
    /// The concrete executor stores the flag shared with its host so the swap
    /// is live without a runtime rebuild.
    fn set_web_access(&self, _access: u8) {}

    /// Namespaced read-only MCP tool names, so a rebuilt permission policy can
    /// keep them usable under read-only-family modes. Executors without MCP
    /// report none.
    fn mcp_read_only_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Seed the plan ledger deterministically from a plan document, bypassing
    /// the model. `input` uses the same JSON shape as `todo_write`. Executors
    /// without a plan ledger reject the request.
    ///
    /// # Errors
    /// The default implementation always fails with "no plan ledger to seed";
    /// implementations fail when `input` does not match the `todo_write` JSON
    /// shape.
    fn seed_plan(&self, _input: &str) -> Result<String, ToolError> {
        Err(ToolError::new("no plan ledger to seed"))
    }
}

/// A tool execution failure with an optional `hint` for the model. W5
/// (nushell `LabeledError` 参照): the hint rides the transcript's tool_result
/// text as a separate `hint:` line, so a repeated failure carries a *fix
/// suggestion*, not just the error — the model's most common failure mode is
/// retrying the identical call after a policy denial or a malformed argument.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{message}{}",
    self.hint
        .as_ref()
        .map_or(String::new(), |h| format!("\nhint: {h}"))
)]
pub struct ToolError {
    message: String,
    hint: Option<String>,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
        }
    }

    /// An error that tells the model what to try next, not only what broke.
    #[must_use]
    pub fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

/// Machine-readable class of a runtime failure (roadmap §16 R2: 失败有类，
/// 类定动作). Callers branch on this instead of string-matching error text:
/// the loop autopilot excludes [`ErrorKind::Transport`] from its blocked
/// streak, the CLI renders cancellation differently, and retry policy hangs
/// off the transport class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    /// Connection/setup failure, mid-stream hang, timeout, or a premature
    /// stream end — transient, worth retrying.
    Transport,
    /// Authentication/authorization failure (401/403, bad key).
    Auth,
    /// The provider rejected the request as over the context window.
    Overflow,
    /// The model misbehaved: empty reply, loop-detected tool spinning, fuse
    /// tripped — not worth retrying the same way.
    Model,
    /// The operator cancelled the turn (Ctrl+C).
    Cancelled,
    /// Anything not covered above.
    Other,
}

impl ErrorKind {
    /// Short label for logs and the loop autopilot's messages.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Auth => "auth",
            Self::Overflow => "overflow",
            Self::Model => "model",
            Self::Cancelled => "cancelled",
            Self::Other => "other",
        }
    }
}

/// Classify a mid-stream `AgentEvent::Error` message. All provider transports
/// surface failures as text at this boundary (the structured `ApiError` lives
/// in the `api` crate and does not cross it), so this is the one deliberate
/// string-matching point — kept next to the overflow matcher in `compact.rs`
/// and covered by tests. Unknown patterns classify as [`ErrorKind::Other`].
///
/// The text is self-produced (`ApiError`'s `Display`), so its stable shapes are
/// read first: an exhausted-retry report, then an echoed status line. The
/// marker lists only back those up, and a status code counts solely as a
/// standalone number — as a bare substring, digits inside request ids and
/// timestamps impersonated a status, and the autopilot acted on that as if it
/// were real.
#[must_use]
pub fn classify_stream_error(message: &str) -> ErrorKind {
    if is_context_overflow_error(message) {
        return ErrorKind::Overflow;
    }
    let lower = message.to_ascii_lowercase();
    // `RetryPolicy` retries only faults it already classes as transient, so an
    // exhausted-retry report is transport by construction.
    if lower.contains("api request failed after ") {
        return ErrorKind::Transport;
    }
    // A status the `api` crate echoed is authoritative (`api returned 429 Too
    // Many Requests: ...`): read it rather than guessing from the body.
    const AUTH_MARKERS: [&str; 4] = [
        "unauthorized",
        "authentication",
        "invalid api key",
        "api key",
    ];
    if let Some(status) = echoed_api_status(&lower) {
        let body_claims_auth = AUTH_MARKERS.iter().any(|marker| lower.contains(marker));
        return match status {
            401 | 403 | 407 => ErrorKind::Auth,
            // Some gateways report a bad key as a 400.
            400 if body_claims_auth => ErrorKind::Auth,
            400 => ErrorKind::Model,
            408 | 409 | 425 | 429 => ErrorKind::Transport,
            500..=599 => ErrorKind::Transport,
            // 404 (model or endpoint), 413, 422, ...: permanent, and nobody's
            // transient fault to back off from.
            _ => ErrorKind::Other,
        };
    }
    const TRANSPORT_MARKERS: [&str; 11] = [
        "connection",
        "timed out",
        "timeout",
        "reset",
        "eof",
        "broken pipe",
        "network",
        "overloaded",
        "stream ended",
        // reqwest's send-phase wrapper: connect failures, send timeouts and
        // resets all render as this line — the underlying cause stays in the
        // source chain and never reaches this text.
        "error sending request",
        // A malformed frame mid-stream is the wire misbehaving, not the model.
        "invalid sse frame",
    ];
    if AUTH_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return ErrorKind::Auth;
    }
    if TRANSPORT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return ErrorKind::Transport;
    }
    let statuses: Vec<u32> = standalone_numbers(&lower).collect();
    // Rate limiting is transient but quota-shaped, not transport-shaped: the
    // autopilot retries it with its backoff, yet it is not a connection
    // fault. Classed as Transport so transient bursts do not kill long
    // missions. Server-side failures likewise: the `api` crate already
    // retries them as transient, so they must not read as a blocked claim.
    if statuses.contains(&429)
        || statuses.iter().any(|status| (500..=599).contains(status))
        || lower.contains("rate limit")
        || lower.contains("too many requests")
    {
        return ErrorKind::Transport;
    }
    if statuses
        .iter()
        .any(|status| matches!(status, 401 | 403 | 407))
    {
        return ErrorKind::Auth;
    }
    if statuses.contains(&400) || lower.contains("invalid request") {
        return ErrorKind::Model;
    }
    ErrorKind::Other
}

/// The status code of an `ApiError::Api` report, which `Display` renders as
/// `api returned <status> ...`. `None` for any other text.
fn echoed_api_status(lower: &str) -> Option<u16> {
    const PREFIX: &str = "api returned ";
    let start = lower.find(PREFIX)? + PREFIX.len();
    lower[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Every decimal number in `lower` that stands on its own — not glued to
/// other letters or digits, and not a `line`/`column` ordinal (serde_json
/// decode errors quote them, and "at line 500 column 3" must not read as a
/// server error) — so request ids, hex digests and timestamps cannot pass
/// for a status code.
fn standalone_numbers(lower: &str) -> impl Iterator<Item = u32> + '_ {
    let mut prev_word: &str = "";
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .filter_map(move |token| {
            let is_number = token.bytes().all(|byte| byte.is_ascii_digit());
            let parsed = if is_number && !matches!(prev_word, "line" | "column") {
                token.parse::<u32>().ok()
            } else {
                None
            };
            if !is_number {
                prev_word = token;
            }
            parsed
        })
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RuntimeError {
    message: String,
    hint: Option<String>,
    kind: ErrorKind,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
            kind: ErrorKind::Other,
        }
    }

    #[must_use]
    pub fn with_hint(message: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            message: message.into(),
            hint,
            kind: ErrorKind::Other,
        }
    }

    /// A classified error: explicit kind, optional user-facing hint.
    #[must_use]
    pub fn with_kind(kind: ErrorKind, message: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            message: message.into(),
            hint,
            kind,
        }
    }

    /// The operator cancelled: the canonical cancellation error.
    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::with_kind(ErrorKind::Cancelled, message, None)
    }

    /// A human-readable hint for what to try next, when the error carries one.
    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// The machine-readable failure class.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub iterations: usize,
    pub continuations: usize,
    /// Tokens *this turn* spent, provider-reported: cumulative usage at the
    /// turn's end minus its start. Covers every request the turn made — the
    /// tool loop, retried transport attempts, and the compaction-summary side
    /// call — because the provider bills all of them for this turn.
    /// Deliberately NOT the session cumulative (that lives on
    /// `runtime.usage()`): the mission ledger debits this figure per turn, and
    /// a cumulative here would re-bill every earlier turn on each
    /// `auto_record_turn` (found in review, 2026-11-27: budgets used to drain
    /// quadratically).
    pub turn_usage: TokenUsage,
    /// Wall time this turn took end to end, in milliseconds (model streaming,
    /// permission waits, tool execution). Carried on the summary so the host can
    /// show it without timing the call itself.
    pub duration_ms: u64,
    /// Summed wall time of the tool executions this turn ran, in milliseconds.
    /// A subset of [`Self::duration_ms`]; authorization and model time are not
    /// counted here.
    pub tool_time_ms: u64,
}

/// Nudge injected as a user message when the model stops with unfinished
/// plan tasks; keeps the transcript API-consistent and resume-safe.
const CONTINUATION_PROMPT: &str = "Continue: your task list still has unfinished items. Work on the next task now and update the todo list as you go.";

/// The D4.5 summarizer contract (ds4 `misc/COMPACT.md`): durable task state
/// only, no invention, reloadable bulk data recorded as path/range, and a
/// hard stop after summarizing — the summarizer never continues the task.
const SUMMARIZER_PROMPT: &str = "You summarize an earlier portion of a coding conversation that is being compacted. Produce a durable task-state summary with exactly these sections, each as a short bullet list, omitting any that are empty: Goal and constraints; Files read or changed; Commands run and their key results; Decisions made and approaches rejected; Known issues; Next steps. Rules: state only facts present in the conversation — never invent or guess; for large outputs or files, record the path or range instead of the content; keep it under 300 words; when you finish the summary, stop. Do not continue the user's task, do not ask questions, do not add commentary.";

/// Tool-result ceiling in characters: larger outputs are head+tail truncated
/// so a runaway command or MCP tool can never blow the model context.
const MAX_TOOL_OUTPUT_CHARS: usize = 32_000;

/// S6 tool-error fuse: this many consecutive tool batches where every executed
/// tool errored trips the turn (success anywhere resets the count). Chosen
/// above the common "model recovers on the second try" band but far below the
/// 64-iteration soft cap, so a hostile environment fails fast instead of
/// burning requests.
const MAX_CONSECUTIVE_TOOL_ERROR_BATCHES: usize = 8;
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
/// A `tool_result` (`OpenAI` `role:tool`, Anthropic `tool_result` block) must
/// answer a preceding tool call; an orphan makes the provider reject the whole
/// request (`OpenAI` 400 "Messages with role 'tool' must be a response to a
/// preceding message with '`tool_calls`'"). Compaction now aligns its cut to
/// avoid creating these, but a resumed or interrupted session can still carry
/// one, so this is the protocol-agnostic safety net applied to every request.
/// Scanning in order, a result survives only once an earlier assistant message
/// has advertised its `tool_use_id`; a `Tool` message left empty is dropped.
///
/// The mirror image gets the same net: a `tool_use` the transcript never
/// answers is closed with a not-executed error result right before the next
/// non-tool message (or at the end). Anthropic and OpenAI both reject an
/// unanswered call, so one dangling advertisement fails every later request
/// in the session. The turn loop records that result itself wherever it aborts
/// (stream failure, cancellation, loop stop); this net covers sessions saved
/// before that, and any abort path that does not.
#[must_use]
fn repair_tool_pairing(messages: Vec<ConversationMessage>) -> Vec<ConversationMessage> {
    let mut advertised: HashSet<String> = HashSet::new();
    // Orphans are not silently swallowed: each drop is surfaced (warning +
    // durable counter, see `compact::record_repair_drops`) because losing a
    // tool result the model never saw answered is information loss on the
    // wire path, not a cosmetic cleanup.
    let mut dropped: Vec<String> = Vec::new();
    // Advertised calls still awaiting their result, in advertisement order.
    // Still non-empty when a non-tool message arrives (or the transcript
    // ends) means those calls were never answered.
    let mut unanswered: Vec<(String, String)> = Vec::new();
    let mut repaired: Vec<ConversationMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        match message.role {
            MessageRole::Assistant => {
                close_unanswered(&mut unanswered, &mut repaired);
                for block in &message.blocks {
                    if let ContentBlock::ToolUse { id, name, .. } = block {
                        advertised.insert(id.clone());
                        unanswered.push((id.clone(), name.clone()));
                    }
                }
                repaired.push(message);
            }
            MessageRole::Tool => {
                let mut kept: Vec<ContentBlock> = Vec::with_capacity(message.blocks.len());
                for block in message.blocks {
                    match &block {
                        ContentBlock::ToolResult { tool_use_id, .. }
                            if !advertised.contains(tool_use_id) =>
                        {
                            dropped.push(tool_use_id.clone());
                        }
                        ContentBlock::ToolResult { tool_use_id, .. } => {
                            unanswered.retain(|(id, _)| id != tool_use_id);
                            kept.push(block);
                        }
                        _ => kept.push(block),
                    }
                }
                if !kept.is_empty() {
                    repaired.push(ConversationMessage {
                        role: message.role,
                        blocks: kept,
                        usage: message.usage,
                        pinned: message.pinned,
                    });
                }
            }
            MessageRole::System | MessageRole::User => {
                close_unanswered(&mut unanswered, &mut repaired);
                repaired.push(message);
            }
        }
    }
    close_unanswered(&mut unanswered, &mut repaired);
    if !dropped.is_empty() {
        crate::compact::record_repair_drops(&dropped);
    }
    repaired
}

/// Answer every still-unanswered advertisement with one error result each,
/// appended to `repaired` in advertisement order.
fn close_unanswered(
    unanswered: &mut Vec<(String, String)>,
    repaired: &mut Vec<ConversationMessage>,
) {
    for (tool_use_id, tool_name) in unanswered.drain(..) {
        tracing::warn!(
            target: "heartflow::compact",
            tool_use_id = %tool_use_id,
            tool_name = %tool_name,
            "repair_tool_pairing closed an unanswered tool_use with a not-executed result"
        );
        repaired.push(ConversationMessage::tool_result(
            tool_use_id,
            tool_name,
            not_executed_result(UNEXECUTED_EARLIER_TURN),
            true,
        ));
    }
}

/// Tool calls the model advertised but the turn ended before running get this
/// result. Every advertisement must be answered (both Anthropic and OpenAI
/// reject an unanswered call), and the model must be able to tell the call
/// never happened rather than assume it ran.
fn not_executed_result(reason: &str) -> String {
    format!("[not executed: {reason}; it had no effect - re-issue it if it is still needed]")
}

const UNEXECUTED_EARLIER_TURN: &str = "an earlier turn ended before this call ran";
const UNEXECUTED_STREAM_FAILED: &str = "the stream failed before this call ran";
const UNEXECUTED_STREAM_ENDED: &str = "the stream ended before this call ran";
const UNEXECUTED_CANCELLED: &str = "the turn was cancelled before this call ran";
const UNEXECUTED_LOOP_STOP: &str = "the turn was stopped by the loop detector before this call ran";

/// Tool calls advertised by an assistant message, in advertisement order, as
/// `(tool_use_id, name, input)`.
fn advertised_tool_uses(blocks: &[ContentBlock]) -> Vec<(String, String, String)> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Shared mailbox for steering an in-flight turn (roadmap §11.2, pi
/// steering). The UI half keeps a clone while the turn future owns the
/// runtime; the turn loop drains it at iteration boundaries — the same safe
/// points where cancellation is honored — and appends each text to the
/// session so the *next* request already carries it. A steer never
/// interrupts an in-flight LLM call (that stays the double-Ctrl+C cancel's
/// job), and one that outlives the turn is left unconsumed for the caller
/// to recover: the mailbox reports nothing on its own.
#[derive(Clone, Debug, Default)]
pub struct SteerMailbox {
    slots: Arc<Mutex<VecDeque<String>>>,
}

impl SteerMailbox {
    /// Queue one steered text (arrival order is delivery order).
    pub fn push(&self, text: String) {
        // A poisoned lock means the draining side panicked mid-turn; the turn
        // is unwinding anyway, so keeping the text cannot make anything worse.
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(text);
    }

    /// Take every queued text, oldest first. Called only by the turn loop at
    /// an iteration boundary.
    pub fn drain(&self) -> Vec<String> {
        let mut guard = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.drain(..).collect()
    }
}

/// One compaction's dropped region, held in the runtime until the turn
/// boundary so the host can archive it before the snapshot overwrites the
/// transcript. A turn may compact more than once (D4.2 two-level hard
/// trigger), so the buffer keeps one entry per compaction rather than one
/// flattened list: generation boundaries must survive into the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionAudit {
    /// `auto` (soft gate), `hard_retry` (D4.2), `manual` (`/compact`), or
    /// `task_reset` (per-task context reset).
    pub reason: String,
    pub messages: Vec<ConversationMessage>,
}

/// Sink for the messages a turn appends to the transcript, so the host can
/// persist them incrementally instead of only at the turn boundary.
///
/// The runtime owns no disk path: [`ConversationRuntime::with_journal`] injects
/// an implementation, and `None` (tests, headless callers) keeps the old
/// behavior. [record](Self::record) is called on the hot path and must never
/// block the turn; [flush](Self::flush) runs at the turn boundary, after the
/// last message and before the snapshot save.
pub trait SessionJournal: Send + Sync {
    /// Queue one message as appended at `ts_ms`. Implementations must not block
    /// the calling turn; dropping a record is worse than queueing it.
    fn record(&self, ts_ms: u64, message: &ConversationMessage);
    /// Drain everything recorded so far and make it durable.
    ///
    /// # Errors
    /// Fails when the underlying writer cannot flush; the caller warns and
    /// continues — the snapshot save is the authority.
    fn flush(&self) -> std::io::Result<()>;
}

#[derive(Debug)]
struct ToolOutcome {
    result: Result<String, ToolError>,
    duration_ms: u64,
}

/// Record `message` on the journal (when present) before it enters memory: if
/// the process dies between the two, recovery sees one extra message that
/// `repair_tool_pairing` safely drops; the reverse order would lose the record.
fn record_message(journal: Option<&Arc<dyn SessionJournal>>, message: &ConversationMessage) {
    if let Some(journal) = journal {
        journal.record(crate::epoch_ms(), message);
    }
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
    /// Dropped regions awaiting the host's archive write, newest last.
    pending_archive: Vec<CompactionAudit>,
    /// Optional incremental transcript sink (crash recovery); `None` disables.
    journal: Option<Arc<dyn SessionJournal>>,
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
    /// Where steered user text waits while a turn runs (§11.2). Empty unless
    /// the host sets a live mailbox before starting the turn — the blocking
    /// REPL never does (it cannot type mid-turn), the TUI loop does.
    steer_mailbox: SteerMailbox,
    /// Extra system-prompt section injected into every request while set
    /// (D1.3 mission reminder). Held apart from `system_prompt` so it never
    /// persists into anything durable — it is a projection of the on-disk
    /// mission ledger, rebuilt by the host at turn boundaries. Appended
    /// last: the section is stable within a turn, so provider prompt
    /// caching only re-pays when the ledger actually changes.
    turn_context: Option<String>,
    /// Base delay for the ST-2 step-level transport retry (1.5s, 3s by
    /// attempts). Tunable only via builder so tests can shrink it to ~zero;
    /// there is no env knob — the default is deliberately fixed.
    transport_retry_backoff: Duration,
    /// ST-2 step-level transport retry budget (default
    /// [`MAX_TRANSPORT_RETRIES`]). Zero disables the step-level retry
    /// entirely — hosts that already retry at their own layer (autopilot
    /// transport backoff, jobs workers) can opt out to avoid double retries.
    transport_retries_max: usize,
    /// Per-turn token budget (D1.2 mid-turn half): the cumulative tokens this
    /// turn may spend before it soft-lands at the next iteration boundary
    /// (same shape as the iteration cap — completed work preserved, a
    /// `Truncated` note explains the stop). `None` (the default) leaves the
    /// turn uncapped; hosts set it per turn from the single active mission's
    /// remaining budget (`mission::turn_token_budget`).
    turn_token_budget: Option<u64>,
}

/// ST-2: transport-class stream faults are retried at the step level before
/// the turn fails. Two attempts (three requests total) bounds the worst case
/// a dead tunnel can stall a turn at roughly base + 2×base of pure backoff.
const MAX_TRANSPORT_RETRIES: usize = 2;

/// Base for the linear-in-attempt transport backoff: 1.5s after the first
/// fault, 3s after the second.
const TRANSPORT_RETRY_BACKOFF_BASE: Duration = Duration::from_millis(1500);

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
            max_iterations: 64,
            max_continuations: 3,
            usage_tracker,
            pending_archive: Vec::new(),
            journal: None,
            compaction: CompactionConfig::default(),
            token_prefix: Cell::new((0, 0)),
            token_calibration: RefCell::new(TokenCalibration::default()),
            steer_mailbox: SteerMailbox::default(),
            turn_context: None,
            transport_retry_backoff: TRANSPORT_RETRY_BACKOFF_BASE,
            transport_retries_max: MAX_TRANSPORT_RETRIES,
            turn_token_budget: None,
        }
    }

    /// Set the per-turn context section (D1.3): appended to the system
    /// prompt of every request in the coming turn(s), never persisted into
    /// the session. `None` clears it.
    pub fn set_turn_context(&mut self, section: Option<String>) {
        self.turn_context = section;
    }

    /// Arm the steer mailbox for the next turn: the host keeps a clone to
    /// push into while the turn future owns `self`; the loop drains it at
    /// iteration boundaries. Setting a fresh mailbox per turn also drops any
    /// stale handle a finished turn left behind.
    pub fn set_steer_mailbox(&mut self, mailbox: SteerMailbox) {
        self.steer_mailbox = mailbox;
    }

    /// Inject the incremental transcript sink. Every message the turn appends
    /// is recorded before it enters memory, so a crash mid-turn recovers the
    /// completed work from the journal beside the last snapshot.
    #[must_use]
    pub fn with_journal(mut self, journal: Arc<dyn SessionJournal>) -> Self {
        self.journal = Some(journal);
        self
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

    /// Override the ST-2 step-level transport retry policy: `max` retry
    /// attempts per turn (0 disables) with `base` backoff (multiplied by the
    /// attempt number). Intended for tests (shrink to ~zero) and hosts that
    /// retry at their own layer.
    #[must_use]
    pub fn with_transport_retry_policy(mut self, max: usize, base: Duration) -> Self {
        self.transport_retries_max = max;
        self.transport_retry_backoff = base;
        self
    }

    /// Set the per-turn token budget (D1.2 mid-turn half): the turn
    /// soft-lands at the next iteration boundary once it has spent this many
    /// cumulative tokens (input + output + cache axes, provider-reported).
    /// `None` uncaps the turn. Hosts set it per turn from the mission
    /// ledger — same lifecycle as [`Self::set_turn_context`] — so a mission
    /// edited mid-turn re-arms on the next turn.
    pub fn set_turn_token_budget(&mut self, budget: Option<u64>) {
        self.turn_token_budget = budget;
    }

    /// Run one user turn: stream assistant messages, execute requested tools
    /// concurrently (order preserved in the transcript), and repeat until the
    /// model stops calling tools.
    ///
    /// `notify` observes every event as it happens; `cancel` aborts the turn
    /// at the next await point while keeping the session API-consistent.
    ///
    /// # Errors
    /// Fails under the same conditions as [`Self::run_turn_with_blocks`].
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
    ///
    /// # Errors
    /// Fails when the provider request cannot be started or the stream
    /// reports an error (a context-overflow rejection compacts and retries,
    /// degrading the preserved tail once harder; a third rejection fails the
    /// turn), when the stream ends without a message stop or produces no
    /// content, when the loop exceeds `max_iterations`, or when the turn is
    /// cancelled. Tool failures do not error the turn; they are recorded as
    /// error `tool_result` messages.
    pub async fn run_turn_with_blocks(
        &mut self,
        blocks: Vec<ContentBlock>,
        prompter: Option<&mut dyn PermissionPrompter>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> Result<TurnSummary, RuntimeError> {
        let started = std::time::Instant::now();
        let result = self.run_turn_timed(blocks, prompter, notify, cancel).await;
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Count everything the turn spent, including turns that ended in an
        // error or were cancelled: the wall clock still elapsed. Added before
        // returning so the host's usage line reads the up-to-date session total.
        self.session.runtime_ms = self.session.runtime_ms.saturating_add(elapsed);
        result.map(|mut summary| {
            summary.duration_ms = elapsed;
            summary
        })
    }

    /// The turn loop itself; [`Self::run_turn_with_blocks`] wraps it to measure
    /// the wall time on every exit path (including errors) in one place.
    async fn run_turn_timed(
        &mut self,
        blocks: Vec<ContentBlock>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> Result<TurnSummary, RuntimeError> {
        self.push_message(ConversationMessage::user_blocks(blocks));

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut tool_time_ms = 0u64;
        let mut iterations = 0usize;
        let mut continuations = 0usize;
        // S6 (roadmap §16 R2): behavior-level fuses for the turn loop.
        let mut stop_policy = StopPolicy::new();
        let mut consecutive_tool_error_batches = 0usize;
        // D4.2 + ST-2: the hard-trigger escalation happens at most twice per
        // turn — level 1 compacts with the configured policy, level 2 halves
        // the preserved tail; a third overflow rejection surfaces as a real
        // error, so a poisoned session can never loop here.
        let mut hard_retries = 0usize;
        // ST-2: transport-class stream faults are retried at the step level
        // before the turn fails (see MAX_TRANSPORT_RETRIES).
        let mut transport_retries = 0usize;
        // D1.2 mid-turn half: the cumulative usage this turn started from.
        // Both the per-turn token budget below and `TurnSummary::turn_usage`
        // are deltas against this snapshot.
        let turn_start = self.usage_tracker.cumulative_usage();

        loop {
            // Honor a Ctrl+C FIRST (C2, semantic review): cancellation must
            // outrank the iteration-limit soft landing — a cancelled turn is
            // an interrupt, never a "success carrying completed work".
            if cancel.is_cancelled() {
                return Err(RuntimeError::cancelled("turn cancelled"));
            }
            iterations += 1;
            if iterations > self.max_iterations {
                // Soft landing: the previous iteration finished (its tool
                // results are in the transcript and the session is
                // consistent), so the turn ends as a success carrying all
                // completed work — not as an error that discards the run.
                // Long agentic turns (dozens of tool batches over minutes)
                // hit the old hard error mid-task; the follow-up turn
                // continues naturally from the preserved transcript.
                notify(&AgentEvent::Truncated(format!(
                    "iteration limit ({}) reached - the turn stops here with all completed work preserved; send a follow-up to continue",
                    self.max_iterations
                )));
                break;
            }
            // D1.2 mid-turn token budget: same soft-landing shape as the
            // iteration cap. Measured from provider-reported usage only —
            // estimates drift, and the budget is the operator's cap on real
            // spend. Checked before spending the next request, so the last
            // completed iteration's work is fully preserved. The unit is
            // input+output: the same unit the mission ledger debits, so
            // "remaining budget" and this guard's measurement agree (adding
            // the cache axes would trip the guard early on cache-heavy
            // providers).
            if let Some(budget) = self.turn_token_budget {
                let cumulative = self.usage_tracker.cumulative_usage();
                let spent = u64::from(
                    cumulative
                        .input_tokens
                        .saturating_sub(turn_start.input_tokens),
                )
                .saturating_add(u64::from(
                    cumulative
                        .output_tokens
                        .saturating_sub(turn_start.output_tokens),
                ));
                if spent >= budget {
                    notify(&AgentEvent::Truncated(format!(
                        "turn token budget ({budget}) reached - the turn stops here with all completed work preserved; send a follow-up (or raise the mission budget) to continue"
                    )));
                    break;
                }
            }

            // Steer consumption point (§11.2): absorb steered user text at the
            // same safe boundary where cancellation is honored, so the request
            // below already carries it as a user message. Never mid-stream —
            // a steer that arrives during the final (tool-less) reply simply
            // stays unconsumed for the caller to recover.
            for text in self.steer_mailbox.drain() {
                self.push_message(ConversationMessage::user_text(text.clone()));
                notify(&AgentEvent::Steered(text));
            }

            // Hermes-style >50% pre-compaction: shrink the context before
            // spending a request on it, but only when a window is configured.
            // D4.5: in Model summary mode the dropped region is summarized by
            // the provider first (degrading to the deterministic summary if
            // that side call fails — compaction itself must never fail).
            if self.should_compact(self.compaction) {
                let model_summary = if self.compaction.summary_mode == SummaryMode::Model {
                    self.summarize_dropped_via_llm(cancel).await?
                } else {
                    None
                };
                self.compact_with(
                    self.compaction,
                    if self.compaction.context_window_tokens > 0 {
                        ThresholdKind::ContextWindowHalf
                    } else {
                        ThresholdKind::AbsoluteMax
                    },
                    "auto",
                    model_summary.as_deref(),
                );
            }

            let mut system_prompt = self.system_prompt.clone();
            if let Some(section) = &self.turn_context {
                system_prompt.push(section.clone());
            }
            let request = ApiRequest {
                system_prompt,
                messages: repair_tool_pairing(build_replay_messages(
                    &self.session.messages,
                    self.compaction.replay_verbatim_tail,
                )),
                tools: self.tool_executor.specs(),
            };
            let mut stream = self.api_client.stream(request)?;

            let (blocks, usage, finished, cancelled, stream_error, error_hint, error_kind) =
                Self::consume_stream(&mut stream, notify, cancel).await;

            if let Some(message) = stream_error {
                // C1: the failed attempt already ran server-side, so whatever
                // usage it reported describes a billed request — record it
                // whether we retry below or fail the turn.
                if let Some(value) = usage {
                    self.usage_tracker.record(value);
                }
                // §12 P-B: the provider's structured class is authoritative
                // when the transport tagged the failure; text markers stay the
                // fallback for events without one (scripted clients, older
                // recordings).
                let kind = error_kind.unwrap_or_else(|| classify_stream_error(&message));
                // D4.2 hard trigger with two-level degradation (ST-2): a
                // context-overflow rejection is the only ground truth that the
                // session no longer fits (estimates drift; a 400 does not).
                // Level 1 compacts with the configured policy; level 2 halves
                // the preserved tail and budget. A third rejection fails the
                // turn for real, so a poisoned session can never loop here.
                if kind == ErrorKind::Overflow
                    && self.compaction.hard_trigger
                    && self.compaction.context_window_tokens > 0
                    && hard_retries < 2
                {
                    let (config, label) = if hard_retries == 0 {
                        (self.compaction, "compacting")
                    } else {
                        (
                            CompactionConfig {
                                preserve_recent_messages: self
                                    .compaction
                                    .preserve_recent_messages
                                    .saturating_sub(1),
                                tail_budget_tokens: self.compaction.tail_budget_tokens / 2,
                                ..self.compaction
                            },
                            "compacting harder (halved tail)",
                        )
                    };
                    if self.session.messages.len() > config.preserve_recent_messages {
                        hard_retries += 1;
                        tracing::warn!(
                            target: "heartflow::compact",
                            "provider rejected the request as over the context window; {label} and retrying (hard trigger, level {hard_retries})"
                        );
                        let result = self.compact_with(
                            config,
                            ThresholdKind::WindowFull,
                            "hard_retry",
                            None,
                        );
                        notify(&AgentEvent::Truncated(format!(
                            "context overflow (level {hard_retries}); {} older messages compacted, retrying",
                            result.removed_message_count
                        )));
                        continue;
                    }
                    // Nothing left to drop under even the degraded policy:
                    // fall through and fail the turn with the provider's
                    // message rather than loop forever.
                }
                // ST-2 step-level transport retry: a transport-class fault
                // (dead connection, idle timeout, mid-stream reset) is worth
                // one clean regeneration — nothing has landed in the
                // transcript yet on this path (S7 landing below happens only
                // when the retry budget is exhausted), so a retry cannot
                // duplicate content.
                if kind == ErrorKind::Transport && transport_retries < self.transport_retries_max {
                    transport_retries += 1;
                    let delay = self.transport_retry_backoff * transport_retries as u32;
                    notify(&AgentEvent::Truncated(format!(
                        "transport fault ({message}); retrying {transport_retries}/{MAX_TRANSPORT_RETRIES} after {delay:?}"
                    )));
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        () = cancel.cancelled() => {
                            return Err(RuntimeError::cancelled(
                                "turn cancelled during transport retry backoff",
                            ));
                        }
                    }
                    continue;
                }
                // S7: the turn is about to fail, but the user already watched
                // these deltas stream in — dropping them would make the saved
                // transcript lie about what happened and leave tool_use blocks
                // unrecoverable. Land the partial content (skipped on the
                // hard-trigger retry above: the retry regenerates this content
                // and must not carry it into a larger, already-overflowing
                // request).
                if !blocks.is_empty() {
                    let unexecuted = advertised_tool_uses(&blocks);
                    let partial = ConversationMessage::assistant_with_usage(blocks, usage);
                    self.push_message(partial);
                    Self::record_unexecuted_tools(
                        &unexecuted,
                        UNEXECUTED_STREAM_FAILED,
                        &mut self.session,
                        self.journal.as_ref(),
                        &mut tool_results,
                        notify,
                    );
                }
                return Err(RuntimeError::with_kind(kind, message, error_hint));
            }
            if !finished {
                // C1: usage reported before the cut still describes a billed
                // request.
                if let Some(value) = usage {
                    self.usage_tracker.record(value);
                }
                // Preserve streamed text so the transcript stays API-consistent,
                // then surface the interruption.
                if !blocks.is_empty() {
                    let unexecuted = advertised_tool_uses(&blocks);
                    let message = ConversationMessage::assistant_with_usage(blocks, usage);
                    self.push_message(message.clone());
                    assistant_messages.push(message);
                    let unexecuted_reason = if cancelled {
                        UNEXECUTED_CANCELLED
                    } else {
                        UNEXECUTED_STREAM_ENDED
                    };
                    Self::record_unexecuted_tools(
                        &unexecuted,
                        unexecuted_reason,
                        &mut self.session,
                        self.journal.as_ref(),
                        &mut tool_results,
                        notify,
                    );
                }
                let (reason, kind) = if cancelled {
                    ("turn cancelled", ErrorKind::Cancelled)
                } else {
                    // The stream died before the message stop event: treat it
                    // as a transport fault (retryable upstream) rather than a
                    // model failure.
                    (
                        "assistant stream ended without a message stop event",
                        ErrorKind::Transport,
                    )
                };
                return Err(RuntimeError::with_kind(kind, reason, None));
            }
            if blocks.is_empty() {
                return Err(RuntimeError::with_kind(
                    ErrorKind::Model,
                    "assistant stream produced no content",
                    None,
                ));
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

            let pending_tool_uses = advertised_tool_uses(&blocks);

            let assistant_message = ConversationMessage::assistant_with_usage(blocks, usage);
            self.push_message(assistant_message.clone());
            assistant_messages.push(assistant_message);

            // S6 loop detection (roadmap §16 R2): the same tool batch hashing
            // identically iteration after iteration means the model is
            // spinning. Fail the turn with a Model-class error so the loop
            // autopilot treats it as a real block, not a transient fault; the
            // transcript is already consistent (assistant + prior tool results
            // are in the session).
            if !pending_tool_uses.is_empty() {
                let batch: Vec<(String, String)> = pending_tool_uses
                    .iter()
                    .map(|(_, name, input)| (name.clone(), input.to_string()))
                    .collect();
                if let Some(reason) = stop_policy.register(&batch) {
                    notify(&AgentEvent::Truncated(format!(
                        "stop policy: {reason} - ending the turn"
                    )));
                    Self::record_unexecuted_tools(
                        &pending_tool_uses,
                        UNEXECUTED_LOOP_STOP,
                        &mut self.session,
                        self.journal.as_ref(),
                        &mut tool_results,
                        notify,
                    );
                    return Err(RuntimeError::with_kind(
                        ErrorKind::Model,
                        format!("loop detected: {reason}"),
                        Some(
                            "the model kept repeating identical tool calls; restate the goal with the concrete next step, or investigate the failing step manually".to_string(),
                        ),
                    ));
                }
            }

            if pending_tool_uses.is_empty() {
                // S5 (roadmap §16 R3): steers that arrived during the final
                // (tool-less) reply used to evaporate with the mailbox. Drain
                // them into the transcript as user messages so the next
                // request — autopilot continuation or the operator's next
                // turn — carries them; nothing the user typed is lost.
                for text in self.steer_mailbox.drain() {
                    self.push_message(ConversationMessage::user_text(text.clone()));
                    notify(&AgentEvent::Steered(text));
                }
                // Self-iteration: with unfinished plan tasks, nudge the model
                // to keep working instead of ending the turn early.
                if self.tool_executor.pending_tasks() > 0 && continuations < self.max_continuations
                {
                    continuations += 1;
                    self.push_message(ConversationMessage::user_text(
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
                // C3: denials and schema rejections are results too. A model
                // that keeps re-asking for a refused call used to spin until
                // the iteration cap; the fuse must see these batches like any
                // all-error batch.
                consecutive_tool_error_batches += 1;
                if consecutive_tool_error_batches >= MAX_CONSECUTIVE_TOOL_ERROR_BATCHES {
                    return Err(RuntimeError::with_kind(
                        ErrorKind::Model,
                        format!(
                            "tool-error fuse: {consecutive_tool_error_batches} consecutive tool batches all failed"
                        ),
                        Some(
                            "every recent tool call failed or was denied; adapt the approach (or ask the operator to approve it) instead of repeating the call"
                                .to_string(),
                        ),
                    ));
                }
                continue;
            }

            // S6 tool-error fuse: a batch where every executed tool errored
            // advances the streak; any success resets it. Too many consecutive
            // all-error batches means the environment is fighting the model —
            // stop burning requests and surface it (zaly's error-circuit
            // breaker; success clears the count).
            let batch_start = tool_results.len();
            let (interrupted, batch_tool_ms) = self
                .execute_tools(allowed, &mut tool_results, notify, cancel)
                .await;
            tool_time_ms = tool_time_ms.saturating_add(batch_tool_ms);
            if interrupted {
                return Err(RuntimeError::cancelled("turn cancelled"));
            }
            let batch_results: Vec<&ContentBlock> = tool_results[batch_start..]
                .iter()
                .flat_map(|message| &message.blocks)
                .collect();
            let has_result = batch_results
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
            let all_errored = has_result
                && batch_results.iter().all(|block| match block {
                    ContentBlock::ToolResult { is_error, .. } => *is_error,
                    _ => true,
                });
            if all_errored {
                consecutive_tool_error_batches += 1;
            } else if has_result {
                consecutive_tool_error_batches = 0;
            }
            if consecutive_tool_error_batches >= MAX_CONSECUTIVE_TOOL_ERROR_BATCHES {
                return Err(RuntimeError::with_kind(
                    ErrorKind::Model,
                    format!(
                        "tool-error fuse: {consecutive_tool_error_batches} consecutive tool batches all failed"
                    ),
                    Some(
                        "every recent tool call failed; fix the underlying problem (missing file, bad command, permission) before retrying".to_string(),
                    ),
                ));
            }
        }

        let cumulative = self.usage_tracker.cumulative_usage();
        let turn_usage = TokenUsage {
            input_tokens: cumulative
                .input_tokens
                .saturating_sub(turn_start.input_tokens),
            output_tokens: cumulative
                .output_tokens
                .saturating_sub(turn_start.output_tokens),
            cache_creation_input_tokens: cumulative
                .cache_creation_input_tokens
                .saturating_sub(turn_start.cache_creation_input_tokens),
            cache_read_input_tokens: cumulative
                .cache_read_input_tokens
                .saturating_sub(turn_start.cache_read_input_tokens),
        };
        Ok(TurnSummary {
            assistant_messages,
            tool_results,
            iterations,
            continuations,
            turn_usage,
            // The wrapper measures the real wall time on every exit path and
            // overwrites this before handing the summary back.
            duration_ms: 0,
            tool_time_ms,
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
        Option<String>,
        Option<ErrorKind>,
    ) {
        let mut text = String::new();
        let mut blocks = Vec::new();
        let mut usage = None;
        let mut finished = false;
        let mut cancelled = false;
        let mut error = None;
        let mut error_hint = None;
        let mut error_kind = None;

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
                    Some(AgentEvent::Error { message, hint, kind }) => {
                        error = Some(message);
                        error_hint = hint;
                        error_kind = kind;
                        break;
                    }
                    // ToolResult events are produced by the runtime itself.
                    Some(AgentEvent::ToolResult { .. }) => {}
                    // Steered is emitted by the turn loop, never by a wire
                    // stream; one showing up here would be a scripted client
                    // echoing it back — ignore rather than double-inject.
                    Some(AgentEvent::Steered(_)) => {}
                },
            }
            if cancelled {
                break;
            }
        }

        flush_text_block(&mut text, &mut blocks);
        (
            blocks, usage, finished, cancelled, error, error_hint, error_kind,
        )
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
                    // W5: a denial is a policy refusal, not a tool fault —
                    // the model's reflex (retry unchanged) is exactly wrong.
                    // The hint rides the tool_result text the model reads.
                    let output = format!(
                        "{reason}\nhint: this is a policy denial, not a tool failure; do not retry the call unchanged - reshape it to stay inside the workspace and non-destructive, or ask the operator to approve it"
                    );
                    notify(&AgentEvent::ToolResult {
                        id: tool_use_id.clone(),
                        name: tool_name.clone(),
                        output: output.clone(),
                        is_error: true,
                        duration_ms: None,
                    });
                    let message =
                        ConversationMessage::tool_result(tool_use_id, tool_name, output, true);
                    self.push_message(message.clone());
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
                        duration_ms: None,
                    });
                    let message =
                        ConversationMessage::tool_result(&tool_use_id, &tool_name, output, true);
                    self.push_message(message.clone());
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
    /// submission order regardless of completion order. Returns whether the
    /// turn was cancelled mid-flight and the summed tool wall time in
    /// milliseconds.
    async fn execute_tools(
        &mut self,
        allowed: Vec<(String, String, String)>,
        tool_results: &mut Vec<ConversationMessage>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
        cancel: &CancellationToken,
    ) -> (bool, u64) {
        let mut outcomes: Vec<Option<ToolOutcome>> = (0..allowed.len()).map(|_| None).collect();
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
        let tool_time_ms = self.record_tool_results(&allowed, outcomes, tool_results, notify);
        (interrupted, tool_time_ms)
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
        outcomes: &mut [Option<ToolOutcome>],
        cancel: &CancellationToken,
    ) -> bool {
        let mut join_set = JoinSet::new();
        for &index in group {
            let executor = Arc::clone(&self.tool_executor);
            let tool_name = allowed[index].1.clone();
            let input = allowed[index].2.clone();
            join_set.spawn_blocking(move || {
                // Measured around the executor only: permission prompts and the
                // model round trip stay out of the tool's own time.
                let started = std::time::Instant::now();
                let result = executor.execute(&tool_name, &input);
                let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                (index, result, duration_ms)
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
                    Some(Ok((index, result, duration_ms))) => {
                        outcomes[index] = Some(ToolOutcome { result, duration_ms });
                    }
                    Some(Err(error)) => {
                        // A panicked/cancelled task: blame an unfinished slot
                        // in this group so the model still gets a result line.
                        if let Some(&slot) = group.iter().find(|&&i| outcomes[i].is_none()) {
                            outcomes[slot] = Some(ToolOutcome {
                                result: Err(ToolError::new(format!("tool task failed: {error}"))),
                                duration_ms: 0,
                            });
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

    /// Close tool calls the model advertised but the turn is ending before
    /// running: one error result each, in advertisement order. An unanswered
    /// advertisement makes the provider reject every later request in the
    /// session.
    fn record_unexecuted_tools(
        pending: &[(String, String, String)],
        reason: &str,
        session: &mut Session,
        journal: Option<&Arc<dyn SessionJournal>>,
        tool_results: &mut Vec<ConversationMessage>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
    ) {
        for (tool_use_id, tool_name, _) in pending {
            let output = not_executed_result(reason);
            notify(&AgentEvent::ToolResult {
                id: tool_use_id.clone(),
                name: tool_name.clone(),
                output: output.clone(),
                is_error: true,
                duration_ms: None,
            });
            let message = ConversationMessage::tool_result(tool_use_id, tool_name, output, true);
            record_message(journal, &message);
            session.messages.push(message.clone());
            tool_results.push(message);
        }
    }

    /// Fold execution outcomes into transcript `tool_result` messages in
    /// submission order, emitting the `ToolResult` events (with each tool's own
    /// wall time) and truncating oversized output. A slot left `None` means the
    /// tool never completed (cancelled) and is recorded as an interruption
    /// error. Returns the summed tool time in milliseconds.
    fn record_tool_results(
        &mut self,
        allowed: &[(String, String, String)],
        outcomes: Vec<Option<ToolOutcome>>,
        tool_results: &mut Vec<ConversationMessage>,
        notify: &mut (dyn FnMut(&AgentEvent) + Send),
    ) -> u64 {
        let mut tool_time_ms = 0u64;
        for ((tool_use_id, tool_name, _), slot) in allowed.iter().zip(outcomes) {
            let (output, is_error, duration_ms) = match slot {
                Some(ToolOutcome {
                    result: Ok(output),
                    duration_ms,
                }) => (truncate_tool_output(&output), false, Some(duration_ms)),
                Some(ToolOutcome {
                    result: Err(error),
                    duration_ms,
                }) => (
                    truncate_tool_output(&error.to_string()),
                    true,
                    Some(duration_ms),
                ),
                None => (String::from("[interrupted by user]"), true, None),
            };
            if let Some(duration) = duration_ms {
                tool_time_ms = tool_time_ms.saturating_add(duration);
            }
            notify(&AgentEvent::ToolResult {
                id: tool_use_id.clone(),
                name: tool_name.clone(),
                output: output.clone(),
                is_error,
                duration_ms,
            });
            let message =
                ConversationMessage::tool_result(tool_use_id, tool_name, output, is_error);
            self.push_message(message.clone());
            tool_results.push(message);
        }
        tool_time_ms
    }

    /// Compact the session in place, keeping a resumable system summary.
    /// The gate uses the amortized token estimate; the in-place compaction
    /// itself moves messages instead of cloning them.
    pub fn compact(&mut self, config: CompactionConfig) -> CompactionResult {
        if self.should_compact(config) {
            self.compact_with(
                config,
                if config.context_window_tokens > 0 {
                    ThresholdKind::ContextWindowHalf
                } else {
                    ThresholdKind::AbsoluteMax
                },
                "auto",
                None,
            )
        } else {
            CompactionResult {
                summary: String::new(),
                removed_message_count: 0,
                dropped: Vec::new(),
                trigger: if config.context_window_tokens > 0 {
                    ThresholdKind::ContextWindowHalf
                } else {
                    ThresholdKind::AbsoluteMax
                },
            }
        }
    }

    /// Gate-free compaction for callers that already decided (manual
    /// `/compact`): same in-place rebuild, deterministic summary.
    pub fn compact_now(&mut self, config: CompactionConfig) -> CompactionResult {
        self.compact_with(
            config,
            if config.context_window_tokens > 0 {
                ThresholdKind::ContextWindowHalf
            } else {
                ThresholdKind::AbsoluteMax
            },
            "manual",
            None,
        )
    }

    /// The loop's compaction core: an explicit trigger kind for the
    /// observation event, the archive reason tag, and an optional
    /// model-generated summary (D4.5).
    fn compact_with(
        &mut self,
        config: CompactionConfig,
        trigger: ThresholdKind,
        reason: &str,
        model_summary: Option<&str>,
    ) -> CompactionResult {
        let mut result =
            compact_session_in_place_with(&mut self.session, config, trigger, model_summary);
        self.token_prefix.set((0, 0));
        if !result.dropped.is_empty() {
            self.pending_archive.push(CompactionAudit {
                reason: reason.to_string(),
                messages: std::mem::take(&mut result.dropped),
            });
        }
        result
    }

    /// Take the dropped regions accumulated since the last call. The host
    /// archives each entry before saving the snapshot: after the save the
    /// in-memory transcript is the only other copy, and compaction already
    /// dropped it there.
    #[must_use]
    pub fn take_compaction_audit(&mut self) -> Vec<CompactionAudit> {
        std::mem::take(&mut self.pending_archive)
    }

    /// D4.5: summarize the region a compaction is about to drop with one
    /// provider call, under the ds4 durable-task-state contract (goal and
    /// constraints, files read/changed, commands run and their key results,
    /// decisions and rejected approaches, known issues, next steps; no
    /// invented facts; summarize and stop — never continue the user's task).
    /// Returns `None` on any failure (stream error, empty reply, no stop) so
    /// the caller degrades to the deterministic summary; compaction must
    /// never fail because its summarizer hiccuped. Events are swallowed on
    /// purpose: this is a side request, not turn output, and must not pollute
    /// the transcript.
    ///
    /// # Errors
    /// [`ErrorKind::Cancelled`] when the operator cancels while the summary
    /// streams. The caller then skips the compaction altogether: compacting
    /// with the lossy deterministic summary is work the operator just asked
    /// to stop, and the next turn retries the model summary instead.
    async fn summarize_dropped_via_llm(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, RuntimeError> {
        let config = self.compaction;
        let keep_from = self
            .session
            .messages
            .len()
            .saturating_sub(config.preserve_recent_messages);
        if keep_from == 0 {
            return Ok(None);
        }
        const SUMMARY_INPUT_CAP: usize = 48_000;
        const SUMMARY_OUTPUT_CAP: usize = 16_000;
        let mut transcript = String::new();
        for message in &self.session.messages[..keep_from] {
            for block in &message.blocks {
                if let crate::session::ContentBlock::Text { text } = block {
                    transcript.push_str(text);
                    transcript.push('\n');
                }
            }
            if transcript.len() >= SUMMARY_INPUT_CAP {
                break;
            }
        }
        if transcript.trim().is_empty() {
            return Ok(None);
        }
        let request = ApiRequest {
            system_prompt: vec![SUMMARIZER_PROMPT.to_string()],
            messages: vec![ConversationMessage::user_blocks(vec![ContentBlock::Text {
                text: format!("Conversation to summarize:\n\n{transcript}"),
            }])],
            tools: Vec::new(),
        };
        let Ok(mut stream) = self.api_client.stream(request) else {
            return Ok(None);
        };
        let mut summary = String::new();
        let mut finished = false;
        loop {
            // Dropping `stream` on the cancel arm aborts the producer task.
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(RuntimeError::cancelled(
                        "turn cancelled during the compaction summary",
                    ));
                }
                event = stream.recv() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                AgentEvent::TextDelta(delta) => {
                    if summary.len() < SUMMARY_OUTPUT_CAP {
                        summary.push_str(&delta);
                    }
                }
                // The summarize call is billed like any other request: dropping
                // its usage here would under-report the session total against
                // the provider's own account (the cumulative tracker is the
                // only budget the usage line shows).
                AgentEvent::Usage(value) => self.usage_tracker.record(value),
                AgentEvent::MessageStop => {
                    finished = true;
                    break;
                }
                AgentEvent::Error { .. } => return Ok(None),
                _ => {}
            }
        }
        Ok(if finished && !summary.trim().is_empty() {
            Some(summary)
        } else {
            None
        })
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

    /// Append one message to the transcript, journalling it first.
    fn push_message(&mut self, message: ConversationMessage) {
        record_message(self.journal.as_ref(), &message);
        self.session.messages.push(message);
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
        let previous = std::mem::replace(&mut self.session.messages, seeds);
        self.token_prefix.set((0, 0));
        if !previous.is_empty() {
            self.pending_archive.push(CompactionAudit {
                reason: "task_reset".to_string(),
                messages: previous,
            });
        }
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
    ///
    /// # Errors
    /// Fails when the underlying executor has no plan ledger or rejects
    /// `input` (same semantics as [`ToolExecutor::seed_plan`]).
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
            .ok_or_else(|| {
                ToolError::with_hint(
                    format!("unknown tool: {tool_name}"),
                    "call with a name from the advertised tool specs; check spelling and namespace prefixes before retrying",
                )
            })?(input)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_replay_messages, repair_tool_pairing, AgentEvent, ApiClient, ApiRequest,
        ConversationRuntime, RuntimeError, SessionJournal, StaticToolExecutor, ToolError,
        ToolExecutor, ToolSpec, TurnStream,
    };
    use crate::compact::{
        compact_session_in_place, estimate_session_tokens, CompactionConfig, SoftTrigger,
        SummaryMode,
    };
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPromptBuilder, SYSTEM_PROMPT_DYNAMIC_BOUNDARY};
    use crate::redact::redact_session;
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
    use crate::usage::TokenUsage;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn rendered_system_prompt_strips_internal_boundary_marker() {
        let request = ApiRequest {
            system_prompt: vec![
                "Identity and ground rules.".to_string(),
                SYSTEM_PROMPT_DYNAMIC_BOUNDARY.to_string(),
                "Environment.".to_string(),
            ],
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let rendered = request.rendered_system_prompt();
        assert_eq!(rendered, "Identity and ground rules.\n\nEnvironment.");
        assert!(!rendered.contains("DYNAMIC_BOUNDARY"));
    }

    #[test]
    fn compaction_pipeline_preserves_tool_pairing_for_random_well_formed_sessions() {
        // Phase 4 invariant property (roadmap §4): a random well-formed
        // session (mixed tool_use ads + results, pinned turns) pushed through
        // `compact_session_in_place` + `build_replay_messages` +
        // `repair_tool_pairing` must satisfy the pairing invariants on the
        // wire projection — every result answers an earlier ad, every ad is
        // answered later, and no tail opens on an orphan result. The xorshift
        // seed is fixed so any failure reproduces exactly.
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for case in 0..200 {
            let mut messages: Vec<ConversationMessage> = Vec::new();
            let mut call_counter: usize = 0;
            let turns = 10 + (next() % 40) as usize;
            for _ in 0..turns {
                match next() % 4 {
                    0 => {
                        let pinned = next() % 8 == 0;
                        messages.push(
                            ConversationMessage::user_text(format!(
                                "user-{case} {}",
                                "x".repeat((next() % 300) as usize)
                            ))
                            .with_pinned(pinned),
                        );
                    }
                    1 => {
                        let pinned = next() % 8 == 0;
                        messages.push(
                            ConversationMessage::assistant(vec![ContentBlock::Text {
                                text: format!(
                                    "assistant-{case} {}",
                                    "y".repeat((next() % 300) as usize)
                                ),
                            }])
                            .with_pinned(pinned),
                        );
                    }
                    2 => {
                        // An ad set immediately answered by its results — the
                        // well-formed shape the pipeline must never break.
                        let ad_count = 1 + (next() % 2) as usize;
                        let first_call = call_counter + 1;
                        call_counter += ad_count;
                        let blocks = (first_call..=call_counter)
                            .map(|i| ContentBlock::ToolUse {
                                id: format!("call_{i}"),
                                name: "bash".to_string(),
                                input: "{}".to_string(),
                            })
                            .collect();
                        messages.push(ConversationMessage::assistant(blocks));
                        for i in first_call..=call_counter {
                            messages.push(ConversationMessage::tool_result(
                                format!("call_{i}"),
                                "bash",
                                format!("result-{i} {}", "z".repeat((next() % 200) as usize)),
                                false,
                            ));
                        }
                    }
                    _ => messages.push(ConversationMessage::assistant(vec![ContentBlock::Text {
                        text: "thinking out loud".to_string(),
                    }])),
                }
            }

            let config = CompactionConfig {
                preserve_recent_messages: 1 + (next() % 4) as usize,
                max_estimated_tokens: 1,
                tail_budget_tokens: match next() % 3 {
                    0 => 0,
                    1 => 800,
                    _ => 6_000,
                },
                ..CompactionConfig::default()
            };
            let mut session = Session {
                version: 1,
                runtime_ms: 0,
                messages,
            };
            let result = compact_session_in_place(&mut session, config);
            if result.removed_message_count == 0 {
                continue;
            }

            // Invariant 1: the rebuilt transcript never opens on an orphan.
            assert_ne!(
                session.messages[1].role,
                MessageRole::Tool,
                "case {case}: tail opens on an orphan tool result"
            );

            let replay = build_replay_messages(&session.messages, config.replay_verbatim_tail);
            let repaired = repair_tool_pairing(replay);

            let mut advertised = std::collections::HashSet::new();
            let mut answered = std::collections::HashSet::new();
            let mut ads: Vec<String> = Vec::new();
            for message in &repaired {
                match message.role {
                    MessageRole::Assistant => {
                        for block in &message.blocks {
                            if let ContentBlock::ToolUse { id, .. } = block {
                                advertised.insert(id.clone());
                                ads.push(id.clone());
                            }
                        }
                    }
                    MessageRole::Tool => {
                        for block in &message.blocks {
                            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                                assert!(
                                    advertised.contains(tool_use_id),
                                    "case {case}: orphan result {tool_use_id} reached the wire"
                                );
                                answered.insert(tool_use_id.clone());
                            }
                        }
                    }
                    MessageRole::System | MessageRole::User => {}
                }
            }
            for id in &ads {
                assert!(
                    answered.contains(id),
                    "case {case}: dangling tool_use {id} reached the wire"
                );
            }
        }
    }

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

    /// Every `tool_use` is answered by a later `tool_result` before any other
    /// message intervenes, and every result answers an earlier advertisement —
    /// the shape both Anthropic and OpenAI accept.
    fn assert_well_paired(messages: &[ConversationMessage]) {
        let mut pending: Vec<String> = Vec::new();
        let mut advertised: Vec<String> = Vec::new();
        for message in messages {
            match message.role {
                MessageRole::Assistant => {
                    assert!(
                        pending.is_empty(),
                        "unanswered tool_use before an assistant message: {pending:?}"
                    );
                    for block in &message.blocks {
                        if let ContentBlock::ToolUse { id, .. } = block {
                            pending.push(id.clone());
                            advertised.push(id.clone());
                        }
                    }
                }
                MessageRole::Tool => {
                    for block in &message.blocks {
                        let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                            continue;
                        };
                        assert!(
                            advertised.contains(tool_use_id),
                            "orphan tool_result {tool_use_id}"
                        );
                        let at = pending
                            .iter()
                            .position(|id| id == tool_use_id)
                            .unwrap_or_else(|| {
                                panic!("tool_result {tool_use_id} answers nothing pending")
                            });
                        pending.swap_remove(at);
                    }
                }
                MessageRole::System | MessageRole::User => {
                    assert!(
                        pending.is_empty(),
                        "unanswered tool_use before a user/system message: {pending:?}"
                    );
                }
            }
        }
        assert!(
            pending.is_empty(),
            "trailing unanswered tool_use {pending:?}"
        );
    }

    fn unanswered_ads() -> ConversationMessage {
        ConversationMessage::assistant(vec![
            ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
            },
            ContentBlock::ToolUse {
                id: "call_2".to_string(),
                name: "read_file".to_string(),
                input: "{}".to_string(),
            },
        ])
    }

    #[test]
    fn repair_closes_an_unanswered_tool_use_before_the_next_message() {
        // The shape a turn leaves when it aborts right after advertising
        // calls (stream failure, cancellation, loop stop). Both Anthropic and
        // OpenAI reject the request as-is: the calls are never answered.
        let next = ConversationMessage::user_text("continue".to_string());
        let repaired = repair_tool_pairing(vec![unanswered_ads(), next]);

        assert_well_paired(&repaired);
        assert_eq!(
            repaired.len(),
            4,
            "one not-executed result per unanswered call"
        );
        for message in &repaired[1..3] {
            assert_eq!(message.role, MessageRole::Tool);
            assert!(matches!(
                &message.blocks[0],
                ContentBlock::ToolResult { is_error: true, output, .. }
                    if output.contains("not executed") && output.contains("no effect")
            ));
        }
    }

    #[test]
    fn repair_closes_only_the_calls_a_partial_batch_left_unanswered() {
        let answered = ConversationMessage::tool_result("call_1", "bash", "ok", false);
        let next = ConversationMessage::user_text("continue".to_string());
        let repaired = repair_tool_pairing(vec![unanswered_ads(), answered, next]);

        assert_well_paired(&repaired);
        assert_eq!(repaired.len(), 4);
        // The real result survives untouched; only call_2 is synthesized.
        assert!(matches!(
            &repaired[1].blocks[0],
            ContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "call_1"
        ));
        assert!(matches!(
            &repaired[2].blocks[0],
            ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "call_2"
        ));
    }

    #[test]
    fn repair_closes_a_trailing_unanswered_tool_use() {
        let user = ConversationMessage::user_text("go".to_string());
        let repaired = repair_tool_pairing(vec![user, unanswered_ads()]);

        assert_well_paired(&repaired);
        assert_eq!(repaired.len(), 4);
    }

    #[test]
    fn repair_leaves_a_well_formed_transcript_untouched() {
        let call = ConversationMessage::assistant(vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            input: "{}".to_string(),
        }]);
        let result = ConversationMessage::tool_result("call_1", "bash", "ok", false);
        let user = ConversationMessage::user_text("continue".to_string());

        let repaired = repair_tool_pairing(vec![call, result, user]);

        assert_eq!(repaired.len(), 3);
        assert_well_paired(&repaired);
        assert!(
            repaired.iter().all(|message| message
                .blocks
                .iter()
                .all(|block| !matches!(block, ContentBlock::ToolResult { output, .. } if output.contains("not executed")))),
            "a paired transcript must not gain a synthesized result"
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
        assert_eq!(summary.turn_usage.output_tokens, 10);
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
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult {
                id,
                name,
                output,
                is_error: false,
                // Executed tools report a measured duration.
                duration_ms: Some(_),
            } if id == "tool-1" && name == "add" && output == "4"
        )));
    }

    #[tokio::test]
    async fn journals_every_appended_message_and_times_the_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone, Default)]
        struct RecordingJournal {
            messages: Arc<StdMutex<Vec<ConversationMessage>>>,
            flushes: Arc<AtomicUsize>,
        }

        impl SessionJournal for RecordingJournal {
            fn record(&self, _ts_ms: u64, message: &ConversationMessage) {
                self.messages
                    .lock()
                    .expect("journal lock")
                    .push(message.clone());
            }

            fn flush(&self) -> std::io::Result<()> {
                self.flushes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let journal = RecordingJournal::default();
        let tool_executor =
            StaticToolExecutor::new().register("add", |input| Ok(input.to_string()));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ScriptedApiClient { call_count: 0 },
            tool_executor,
            PermissionPolicy::new(PermissionMode::Prompt),
            Vec::new(),
        )
        .with_journal(Arc::new(journal.clone()));

        let mut events = Vec::new();
        let summary = runtime
            .run_turn(
                "what is 2 + 2?",
                Some(&mut PromptAllowOnce),
                &mut |event| events.push(event.clone()),
                &CancellationToken::new(),
            )
            .await
            .expect("turn should succeed");

        // Journal order mirrors memory: user, assistant+tool_use, tool_result,
        // final assistant — recorded before each push.
        let recorded = journal.messages.lock().expect("journal lock").clone();
        assert_eq!(recorded, runtime.session().messages);
        assert_eq!(recorded.len(), 4);
        // The turn wrapper folded its wall time into the persisted accumulator.
        assert_eq!(summary.duration_ms, runtime.session().runtime_ms);
        // Executed tools carry a measured duration; derived results do not.
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult {
                duration_ms: Some(_),
                ..
            }
        )));
        journal.flush().expect("flush");
        assert_eq!(journal.flushes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn steered_text_joins_the_next_request_as_a_user_message() {
        use std::sync::{Arc, Mutex as StdMutex};

        use crate::SteerMailbox;

        // The steer is pushed from inside the tool handler — the exact window
        // between iteration 1's drain and iteration 2's — so the test proves
        // the boundary consumption, not just a pre-turn push.
        #[derive(Clone)]
        struct CapturingClient {
            requests: Arc<StdMutex<Vec<ApiRequest>>>,
        }
        impl ApiClient for CapturingClient {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let call = {
                    let mut guard = self
                        .requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(request);
                    guard.len()
                };
                match call {
                    1 => Ok(TurnStream::from_events(vec![
                        AgentEvent::ToolUse {
                            id: "probe-1".to_string(),
                            name: "probe".to_string(),
                            input: "x".to_string(),
                        },
                        AgentEvent::MessageStop,
                    ])),
                    _ => Ok(TurnStream::from_events(vec![
                        AgentEvent::TextDelta("noted.".to_string()),
                        AgentEvent::MessageStop,
                    ])),
                }
            }
        }

        let requests: Arc<StdMutex<Vec<ApiRequest>>> = Arc::new(StdMutex::new(Vec::new()));
        let mailbox = SteerMailbox::default();
        let push_mailbox = mailbox.clone();
        let tool_executor = StaticToolExecutor::new().register("probe", move |_| {
            push_mailbox.push("STEER-MARKER while the tool ran".to_string());
            Ok("probed".to_string())
        });
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingClient {
                requests: Arc::clone(&requests),
            },
            tool_executor,
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        );
        runtime.set_steer_mailbox(mailbox);

        let mut events = Vec::new();
        let summary = runtime
            .run_turn(
                "run the probe",
                None,
                &mut |event| events.push(event.clone()),
                &CancellationToken::new(),
            )
            .await
            .expect("steered turn should succeed");
        assert_eq!(summary.iterations, 2, "tool round then steered round");

        // Exactly one Steered event, and the second request carries the text
        // as a plain user message.
        let steered_events = events
            .iter()
            .filter(
                |event| matches!(event, AgentEvent::Steered(text) if text.contains("STEER-MARKER")),
            )
            .count();
        assert_eq!(steered_events, 1, "one Steered event per consumed steer");
        let guard = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(guard.len(), 2);
        let second = &guard[1];
        let carries = second.messages.iter().any(|message| {
            message.role == MessageRole::User
                && message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("STEER-MARKER")))
        });
        assert!(
            carries,
            "the steered text rides request two as a user message"
        );
        // The first request must NOT carry it (it was pushed mid-turn).
        let first_carries = guard[0].messages.iter().any(|message| {
            message
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("STEER-MARKER")))
        });
        assert!(
            !first_carries,
            "the steer joins the *next* request, not the in-flight one"
        );
    }

    #[tokio::test]
    async fn turn_context_rides_the_system_prompt_and_never_the_session() {
        use std::sync::{Arc, Mutex as StdMutex};

        struct PromptCapture(Arc<StdMutex<Vec<Vec<String>>>>);
        impl ApiClient for PromptCapture {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request.system_prompt.clone());
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("ok".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let captured: Arc<StdMutex<Vec<Vec<String>>>> = Arc::new(StdMutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            PromptCapture(Arc::clone(&captured)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            vec!["base prompt".to_string()],
        );

        runtime.set_turn_context(Some("MISSION REMINDER".to_string()));
        runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn with context");
        runtime.set_turn_context(None);
        runtime
            .run_turn("again", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("turn without context");

        let guard = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(guard.len(), 2);
        assert_eq!(
            guard[0],
            vec!["base prompt".to_string(), "MISSION REMINDER".to_string()],
            "the section is appended after the base prompt"
        );
        assert_eq!(
            guard[1],
            vec!["base prompt".to_string()],
            "clearing the context removes it from later requests"
        );
        assert!(
            !runtime
                .session()
                .messages
                .iter()
                .any(|message| message.role == MessageRole::System),
            "the reminder is a per-request projection, never persisted"
        );
    }

    /// Seed a session with enough history that compaction has something to
    /// fold (`message_count > preserve_recent_messages`) and enough *weight*
    /// that the token estimates cross the gates under test (~chars/4).
    fn fat_session(rounds: usize) -> Session {
        let mut session = Session::new();
        for round in 0..rounds {
            session.messages.push(ConversationMessage::user_text(
                format!("question {round} with some padding text to give the region weight ")
                    .repeat(6),
            ));
            session
                .messages
                .push(ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: format!("answer {round} with padding text of its own ").repeat(6),
                }]));
        }
        session
    }

    #[tokio::test]
    async fn hard_trigger_compacts_and_retries_once_on_context_overflow() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct OverflowThenReply(Arc<StdMutex<Vec<ApiRequest>>>);
        impl ApiClient for OverflowThenReply {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let call = {
                    let mut guard = self
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(request);
                    guard.len()
                };
                match call {
                    1 => Ok(TurnStream::from_events(vec![AgentEvent::Error {
                        message: "this model's maximum context length is 1000 tokens. However, you requested 1200 tokens".to_string(),
                        hint: None,
                        kind: None,
                    }])),
                    _ => Ok(TurnStream::from_events(vec![
                        AgentEvent::TextDelta("recovered".to_string()),
                        AgentEvent::MessageStop,
                    ])),
                }
            }
        }

        let requests: Arc<StdMutex<Vec<ApiRequest>>> = Arc::new(StdMutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new(
            fat_session(3),
            OverflowThenReply(Arc::clone(&requests)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_compaction(CompactionConfig {
            context_window_tokens: 2_000,
            // Soft gate high enough to stay out of the way: the hard path is
            // under test, and a soft compaction would consume call 1.
            soft_trigger: SoftTrigger::Ds4,
            hard_trigger: true,
            ..CompactionConfig::default()
        });

        let summary = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("the hard retry recovers the turn");
        assert_eq!(summary.iterations, 2, "overflow, compact, retry");

        let guard = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(guard.len(), 2, "exactly one retry");
        assert!(
            guard[1].messages.len() < guard[0].messages.len(),
            "the retry went out after compaction"
        );
    }

    #[tokio::test]
    async fn overflow_fails_after_two_degradation_levels_and_unarmed_never_retries() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct AlwaysOverflow(Arc<StdMutex<usize>>);
        impl ApiClient for AlwaysOverflow {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let mut calls = self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *calls += 1;
                Ok(TurnStream::from_events(vec![AgentEvent::Error {
                    message: "prompt is too long: 210000 tokens > 200000 maximum".to_string(),
                    hint: None,
                    kind: None,
                }]))
            }
        }

        // Armed but the overflow repeats: level-1 compaction, then the ST-2
        // level-2 degradation (halved tail), then a real failure.
        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            fat_session(3),
            AlwaysOverflow(Arc::clone(&calls)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_compaction(CompactionConfig {
            context_window_tokens: 2_000,
            soft_trigger: SoftTrigger::Ds4,
            hard_trigger: true,
            ..CompactionConfig::default()
        });
        let error = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect_err("overflow surviving both degradation levels must fail the turn");
        assert!(error.to_string().contains("prompt is too long"));
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            3,
            "two escalation levels, then the third rejection fails"
        );

        // Unarmed (the shipped default): the overflow surfaces immediately.
        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            fat_session(3),
            AlwaysOverflow(Arc::clone(&calls)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_compaction(CompactionConfig {
            context_window_tokens: 2_000,
            soft_trigger: SoftTrigger::Ds4,
            ..CompactionConfig::default()
        });
        let error = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect_err("without the hard trigger the overflow is terminal");
        assert!(error.to_string().contains("prompt is too long"));
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1,
            "no retry when unarmed"
        );
    }

    #[tokio::test]
    async fn second_overflow_degrades_the_tail_harder_and_recovers() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct OverflowTwiceThenReply(Arc<StdMutex<Vec<ApiRequest>>>);
        impl ApiClient for OverflowTwiceThenReply {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let call = {
                    let mut guard = self
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(request);
                    guard.len()
                };
                match call {
                    1 | 2 => Ok(TurnStream::from_events(vec![AgentEvent::Error {
                        message: "this model's maximum context length is 1000 tokens. However, you requested 1200 tokens".to_string(),
                        hint: None,
                        kind: None,
                    }])),
                    _ => Ok(TurnStream::from_events(vec![
                        AgentEvent::TextDelta("recovered at level 2".to_string()),
                        AgentEvent::MessageStop,
                    ])),
                }
            }
        }

        let requests: Arc<StdMutex<Vec<ApiRequest>>> = Arc::new(StdMutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new(
            fat_session(3),
            OverflowTwiceThenReply(Arc::clone(&requests)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_compaction(CompactionConfig {
            context_window_tokens: 2_000,
            soft_trigger: SoftTrigger::Ds4,
            hard_trigger: true,
            ..CompactionConfig::default()
        });
        let summary = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("the level-2 degradation recovers the turn");
        assert_eq!(summary.iterations, 3, "overflow, compact, degrade, retry");

        let guard = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(guard.len(), 3, "two escalations, then success");
        assert!(
            guard[1].messages.len() < guard[0].messages.len(),
            "level-1 compaction shrank the request"
        );
        assert!(
            guard[2].messages.len() < guard[1].messages.len(),
            "level-2 degradation shrank it further"
        );
    }

    #[tokio::test]
    async fn transport_error_retries_then_recovers() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct TransportThenReply(Arc<StdMutex<usize>>);
        impl ApiClient for TransportThenReply {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let mut calls = self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *calls += 1;
                if *calls == 1 {
                    // A dead tunnel mid-stream, as the idle watchdog reports it
                    // (the message must classify as Transport, not Other).
                    return Ok(TurnStream::from_events(vec![AgentEvent::Error {
                        message:
                            "stream went silent for 2s mid-turn; the connection is presumed dead"
                                .to_string(),
                        hint: None,
                        kind: None,
                    }]));
                }
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("recovered".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TransportThenReply(Arc::clone(&calls)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_transport_retry_policy(2, std::time::Duration::from_millis(1));
        let summary = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("one transport fault is retried, not terminal");
        assert_eq!(summary.iterations, 2, "fault, backoff, retry");
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            2
        );
        let reply_text = summary
            .assistant_messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            reply_text.contains("recovered"),
            "the retry's reply is the turn's output: {reply_text}"
        );
    }

    #[tokio::test]
    async fn transport_retry_exhausts_after_two_attempts() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct AlwaysTransport(Arc<StdMutex<usize>>);
        impl ApiClient for AlwaysTransport {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let mut calls = self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *calls += 1;
                Ok(TurnStream::from_events(vec![AgentEvent::Error {
                    message: "connection reset by peer".to_string(),
                    hint: None,
                    kind: None,
                }]))
            }
        }

        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            AlwaysTransport(Arc::clone(&calls)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_transport_retry_policy(2, std::time::Duration::from_millis(1));
        let error = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect_err("repeated transport faults exhaust the retry budget");
        assert!(error.kind == super::ErrorKind::Transport, "{error}");
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            3,
            "initial request plus MAX_TRANSPORT_RETRIES retries"
        );
    }

    /// A stream that advertises one tool call, then keeps the line open for
    /// `hold` without a message stop. A zero `hold` closes the channel on its
    /// own (stream ended); a long one leaves only a cancel to end the turn.
    fn advertising_stream(hold: Duration) -> TurnStream {
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(4);
        let handle = tokio::spawn(async move {
            let _ = tx
                .send(AgentEvent::ToolUse {
                    id: "t-1".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                })
                .await;
            tokio::time::sleep(hold).await;
        });
        TurnStream::new(rx, handle.abort_handle())
    }

    #[tokio::test]
    async fn a_tool_call_left_by_a_failed_stream_is_closed_as_not_executed() {
        struct AdvertiseThenFail;
        impl ApiClient for AdvertiseThenFail {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Ok(TurnStream::from_events(vec![
                    AgentEvent::ToolUse {
                        id: "t-1".to_string(),
                        name: "bash".to_string(),
                        input: "{}".to_string(),
                    },
                    // Usage reported before the fault (Anthropic's
                    // message_delta lands ahead of message_stop): C1 says it
                    // still describes a billed request.
                    AgentEvent::Usage(TokenUsage {
                        input_tokens: 50,
                        output_tokens: 5,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AgentEvent::Error {
                        message: "connection reset by peer".to_string(),
                        hint: None,
                        kind: None,
                    },
                ]))
            }
        }

        let executor = StaticToolExecutor::new().register("bash", |_| Ok("ran".to_string()));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            AdvertiseThenFail,
            executor,
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        // No step-level retry: the first fault lands the partial content.
        .with_transport_retry_policy(0, Duration::ZERO);
        let mut events: Vec<AgentEvent> = Vec::new();
        let mut notify = |event: &AgentEvent| events.push(event.clone());
        let error = runtime
            .run_turn("go", None, &mut notify, &CancellationToken::new())
            .await
            .expect_err("a transport fault with retries disabled fails the turn");
        assert_eq!(error.kind(), super::ErrorKind::Transport);
        // C1: the failed attempt's reported usage reached the cumulative
        // account — the provider billed it regardless of the fault.
        let usage = runtime.usage().cumulative_usage();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 5);

        let messages = &runtime.session().messages;
        assert_well_paired(messages);
        assert_eq!(
            messages.len(),
            3,
            "user, the advertisement, its closing result"
        );
        let ContentBlock::ToolResult {
            tool_use_id,
            output,
            is_error,
            ..
        } = &messages[2].blocks[0]
        else {
            panic!("expected the closing tool_result");
        };
        assert_eq!(tool_use_id, "t-1");
        assert!(*is_error);
        assert!(output.contains("stream failed"), "{output}");
        // The operator sees the call was skipped, and it never actually ran.
        assert!(events.iter().any(
            |event| matches!(event, AgentEvent::ToolResult { id, is_error: true, .. } if id == "t-1")
        ));
        assert!(events.iter().all(
            |event| !matches!(event, AgentEvent::ToolResult { output, .. } if output == "ran")
        ));
    }

    /// §12 P-B: a structured `kind` on the error event is authoritative —
    /// the text markers never run when the transport tagged the failure.
    /// Both directions: structured Auth silences the transport retry on
    /// transport-sounding text, and structured Transport triggers the retry
    /// on text with no recognizable markers.
    #[tokio::test]
    async fn a_structured_kind_outranks_the_text_markers() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct StructuredKind(Arc<StdMutex<usize>>);
        impl ApiClient for StructuredKind {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let n = {
                    let mut calls = self
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *calls += 1;
                    *calls
                };
                Ok(TurnStream::from_events(if n == 1 {
                    vec![AgentEvent::Error {
                        message: "connection reset by peer".to_string(),
                        hint: None,
                        // Auth is not retryable at the step level — the
                        // structured field must silence the retry despite
                        // the transport-sounding message.
                        kind: Some(super::ErrorKind::Auth),
                    }]
                } else {
                    vec![
                        AgentEvent::TextDelta("unreachable".to_string()),
                        AgentEvent::MessageStop,
                    ]
                }))
            }
        }
        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            StructuredKind(Arc::clone(&calls)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_transport_retry_policy(2, Duration::from_millis(1));
        let error = runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect_err("structured Auth fails the turn without retrying");
        assert_eq!(error.kind(), super::ErrorKind::Auth);
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1,
            "the transport retry must not fire for a structured Auth"
        );

        // The inverse: structured Transport on marker-free text still gets
        // the clean regeneration.
        #[derive(Clone)]
        struct NovelThenReply(Arc<StdMutex<usize>>);
        impl ApiClient for NovelThenReply {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let n = {
                    let mut calls = self
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *calls += 1;
                    *calls
                };
                Ok(TurnStream::from_events(if n == 1 {
                    vec![AgentEvent::Error {
                        message: "utterly novel wording nothing matches".to_string(),
                        hint: None,
                        kind: Some(super::ErrorKind::Transport),
                    }]
                } else {
                    vec![
                        AgentEvent::TextDelta("recovered".to_string()),
                        AgentEvent::MessageStop,
                    ]
                }))
            }
        }
        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NovelThenReply(Arc::clone(&calls)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_transport_retry_policy(2, Duration::from_millis(1));
        runtime
            .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("structured Transport gets the step-level retry");
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            2
        );
    }

    /// C3: a batch where every call was denied (or schema-rejected) is an
    /// all-error batch for the fuse — a model that keeps re-asking for a
    /// refused call trips the fuse instead of spinning to the iteration cap.
    /// Inputs vary per call so the S6 loop detector stays out of the way.
    #[tokio::test]
    async fn denied_only_batches_trip_the_tool_error_fuse() {
        use std::sync::Mutex as StdMutex;

        struct AlwaysDeniedPrompter;
        impl PermissionPrompter for AlwaysDeniedPrompter {
            fn decide<'a>(
                &'a mut self,
                _request: &'a PermissionRequest,
            ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
                Box::pin(async move {
                    PermissionPromptDecision::Deny {
                        reason: "not approved".to_string(),
                    }
                })
            }
        }

        #[derive(Clone)]
        struct VaryingToolClient(Arc<StdMutex<usize>>);
        impl ApiClient for VaryingToolClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let n = {
                    let mut calls = self
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *calls += 1;
                    *calls
                };
                Ok(TurnStream::from_events(vec![
                    AgentEvent::ToolUse {
                        id: format!("t-{n}"),
                        name: "tick".to_string(),
                        input: format!(r#"{{"n":{n}}}"#),
                    },
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            VaryingToolClient(Arc::clone(&calls)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Prompt),
            Vec::new(),
        );
        let mut prompter = AlwaysDeniedPrompter;
        let error = runtime
            .run_turn(
                "go",
                Some(&mut prompter),
                &mut noop_notify(),
                &CancellationToken::new(),
            )
            .await
            .expect_err("denial-only batches must trip the tool-error fuse");
        assert!(error.to_string().contains("tool-error fuse"), "{error}");
        assert_eq!(error.kind(), super::ErrorKind::Model);
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            super::MAX_CONSECUTIVE_TOOL_ERROR_BATCHES,
            "one request per denied batch, tripping exactly at the fuse"
        );
        // Well-paired despite every call being denied: each denial is an
        // in-band error result.
        for message in &runtime.session().messages {
            for block in &message.blocks {
                if let ContentBlock::ToolUse { id, .. } = block {
                    assert!(runtime.session().messages.iter().any(|later| later
                        .role == MessageRole::Tool
                        && later
                            .blocks
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id))));
                }
            }
        }
    }

    #[tokio::test]
    async fn a_tool_call_left_by_an_ended_or_cancelled_stream_is_closed_as_not_executed() {
        struct AdvertiseThenHang(Duration);
        impl ApiClient for AdvertiseThenHang {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                Ok(advertising_stream(self.0))
            }
        }

        for (hold, cancel_after, expected_kind, expected_reason) in [
            (
                Duration::ZERO,
                None,
                super::ErrorKind::Transport,
                "stream ended",
            ),
            (
                Duration::from_secs(3),
                Some(Duration::from_millis(30)),
                super::ErrorKind::Cancelled,
                "cancelled",
            ),
        ] {
            let executor = StaticToolExecutor::new().register("bash", |_| Ok("ran".to_string()));
            let mut runtime = ConversationRuntime::new(
                Session::new(),
                AdvertiseThenHang(hold),
                executor,
                PermissionPolicy::new(PermissionMode::Allow),
                Vec::new(),
            );
            let cancel = CancellationToken::new();
            if let Some(delay) = cancel_after {
                let trigger = cancel.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    trigger.cancel();
                });
            }
            let error = runtime
                .run_turn("go", None, &mut noop_notify(), &cancel)
                .await
                .expect_err("the turn must not succeed");
            assert_eq!(error.kind(), expected_kind, "{error}");

            let messages = &runtime.session().messages;
            assert_well_paired(messages);
            assert_eq!(messages.len(), 3, "{expected_reason}");
            let ContentBlock::ToolResult {
                tool_use_id,
                output,
                is_error,
                ..
            } = &messages[2].blocks[0]
            else {
                panic!("{expected_reason}: expected the closing tool_result");
            };
            assert_eq!(tool_use_id, "t-1");
            assert!(*is_error);
            assert!(output.contains(expected_reason), "{output}");
        }
    }

    /// The S6 loop fuse stops the turn after the advertisement landed and
    /// before the calls ran. The session must stay provider-valid — an
    /// unanswered advertisement makes every later request fail — and the
    /// stopped calls must never have executed.
    #[tokio::test]
    async fn a_loop_fuse_stop_leaves_the_session_requestable() {
        use std::sync::Mutex as StdMutex;

        #[derive(Clone)]
        struct SameCallRecorder {
            calls: Arc<StdMutex<usize>>,
            requests: Arc<StdMutex<Vec<ApiRequest>>>,
        }
        impl ApiClient for SameCallRecorder {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                self.requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request);
                let n = {
                    let mut calls = self
                        .calls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *calls += 1;
                    *calls
                };
                Ok(if n <= 3 {
                    TurnStream::from_events(vec![
                        AgentEvent::ToolUse {
                            id: format!("t-{n}"),
                            name: "bash".to_string(),
                            input: "{}".to_string(),
                        },
                        AgentEvent::MessageStop,
                    ])
                } else {
                    TurnStream::from_events(vec![
                        AgentEvent::TextDelta("done".to_string()),
                        AgentEvent::MessageStop,
                    ])
                })
            }
        }

        let calls = Arc::new(StdMutex::new(0usize));
        let requests: Arc<StdMutex<Vec<ApiRequest>>> = Arc::new(StdMutex::new(Vec::new()));
        let executor = StaticToolExecutor::new().register("bash", |_| Ok("ran".to_string()));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SameCallRecorder {
                calls: Arc::clone(&calls),
                requests: Arc::clone(&requests),
            },
            executor,
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        );
        let cancel = CancellationToken::new();
        let error = runtime
            .run_turn("spin", None, &mut noop_notify(), &cancel)
            .await
            .expect_err("identical batches must trip the loop fuse");
        assert!(error.to_string().contains("loop detected"), "{error}");

        let messages = &runtime.session().messages;
        assert_well_paired(messages);
        assert_eq!(
            messages.len(),
            7,
            "two executed rounds, then the stopped advertisement closed"
        );
        assert!(matches!(
            &messages[6].blocks[0],
            ContentBlock::ToolResult { tool_use_id, is_error: true, output, .. }
                if tool_use_id == "t-3" && output.contains("loop detector")
        ));

        // The very next request must carry a transcript the provider accepts.
        runtime
            .run_turn("again", None, &mut noop_notify(), &cancel)
            .await
            .expect("the turn after a loop stop is a normal turn");
        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 4);
        assert_well_paired(&requests[3].messages);
    }

    #[tokio::test]
    async fn cancelling_during_the_model_summary_skips_the_compaction() {
        struct SummaryHangs;
        impl ApiClient for SummaryHangs {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                assert!(
                    request
                        .system_prompt
                        .first()
                        .is_some_and(|section| section.contains("durable task-state summary")),
                    "the turn must stop at the summary"
                );
                let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(4);
                let handle = tokio::spawn(async move {
                    let _ = tx.send(AgentEvent::TextDelta("Goal: ".to_string())).await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                });
                Ok(TurnStream::new(rx, handle.abort_handle()))
            }
        }

        let session = fat_session(4);
        let before = session.messages.len();
        let mut runtime = ConversationRuntime::new(
            session,
            SummaryHangs,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_compaction(CompactionConfig {
            context_window_tokens: 1_000,
            summary_mode: SummaryMode::Model,
            ..CompactionConfig::default()
        });
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let error = runtime
            .run_turn("go", None, &mut noop_notify(), &cancel)
            .await
            .expect_err("cancelling mid-summary fails the turn");
        assert_eq!(error.kind(), super::ErrorKind::Cancelled);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the cancel must interrupt the summary, not wait it out"
        );
        // Only the user input was appended: nothing was compacted under the
        // operator's Ctrl+C, so the next turn retries the model summary.
        assert_eq!(runtime.session().messages.len(), before + 1);
        assert!(runtime.should_compact(runtime.compaction()));
    }

    #[tokio::test]
    async fn model_summary_rides_the_continuation_and_degrades_on_failure() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct SummaryThenReply {
            requests: Arc<StdMutex<Vec<ApiRequest>>>,
            fail_the_summarizer: bool,
        }
        impl ApiClient for SummaryThenReply {
            fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let is_summarizer = request
                    .system_prompt
                    .first()
                    .is_some_and(|section| section.contains("durable task-state summary"));
                {
                    let mut guard = self
                        .requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(request);
                }
                if is_summarizer {
                    return Ok(if self.fail_the_summarizer {
                        TurnStream::from_events(vec![AgentEvent::Error {
                            message: "summarizer offline".to_string(),
                            hint: None,
                            kind: None,
                        }])
                    } else {
                        TurnStream::from_events(vec![
                            AgentEvent::TextDelta("Goal: land compaction.".to_string()),
                            AgentEvent::MessageStop,
                        ])
                    });
                }
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("ok".to_string()),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        for (fail, expect_model_summary) in [(false, true), (true, false)] {
            let requests: Arc<StdMutex<Vec<ApiRequest>>> = Arc::new(StdMutex::new(Vec::new()));
            let mut runtime = ConversationRuntime::new(
                fat_session(4),
                SummaryThenReply {
                    requests: Arc::clone(&requests),
                    fail_the_summarizer: fail,
                },
                StaticToolExecutor::new(),
                PermissionPolicy::new(PermissionMode::Allow),
                Vec::new(),
            )
            .with_compaction(CompactionConfig {
                // The half-window soft gate (500) fires on the seeded
                // session (~1150 estimated) while the post-compaction tail
                // (~430) stays under it, exercising summarize-then-compact
                // exactly once.
                context_window_tokens: 1_000,
                summary_mode: SummaryMode::Model,
                ..CompactionConfig::default()
            });
            runtime
                .run_turn("go", None, &mut noop_notify(), &CancellationToken::new())
                .await
                .expect("compaction must never fail the turn");

            let guard = requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The soft gate fires at the loop top: the side summarizer call
            // goes out before the main request.
            assert!(
                guard.len() >= 2,
                "summarizer call then main request (fail={fail})"
            );
            let text = runtime
                .session()
                .messages
                .first()
                .map(|message| {
                    message
                        .blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            assert_eq!(
                text.contains("model-generated from the dropped region"),
                expect_model_summary,
                "fail={fail}: {text}"
            );
            if expect_model_summary {
                assert!(text.contains("Goal: land compaction."));
            }
        }
    }

    /// `TurnSummary::turn_usage` is the turn's own spend, never the session
    /// cumulative: the mission ledger debits it per turn, and a cumulative
    /// used to re-bill every earlier turn on each `auto_record_turn`
    /// (review, 2026-11-27 — budgets drained quadratically).
    #[tokio::test]
    async fn turn_usage_is_the_turns_own_delta_not_the_session_cumulative() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct TwoTurnUsageClient(Arc<StdMutex<usize>>);
        impl ApiClient for TwoTurnUsageClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let n = {
                    let mut calls = self
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *calls += 1;
                    *calls
                };
                let (input, output) = if n == 1 { (100, 10) } else { (30, 5) };
                Ok(TurnStream::from_events(vec![
                    AgentEvent::TextDelta("reply".to_string()),
                    AgentEvent::Usage(TokenUsage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TwoTurnUsageClient(Arc::clone(&calls)),
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        );
        let first = runtime
            .run_turn("one", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("first turn");
        let second = runtime
            .run_turn("two", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("second turn");

        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            2
        );
        // Each summary carries only its own turn's request.
        assert_eq!(first.turn_usage.input_tokens, 100);
        assert_eq!(first.turn_usage.output_tokens, 10);
        assert_eq!(second.turn_usage.input_tokens, 30);
        assert_eq!(second.turn_usage.output_tokens, 5);
        // The runtime-level cumulative keeps summing across turns — the two
        // views are distinct by design, and a regression to cumulative-in-
        // summary would show here as 130.
        let cumulative = runtime.usage().cumulative_usage();
        assert_eq!(cumulative.input_tokens, 130);
        assert_eq!(cumulative.output_tokens, 15);
    }

    /// D1.2 mid-turn half: a per-turn token budget soft-lands the turn at an
    /// iteration boundary (same shape as the iteration cap — Ok summary,
    /// completed work preserved, a Truncated note) instead of burning past
    /// the operator's cap by a full turn. Inputs vary so the S6 loop detector
    /// stays out of the way; each iteration reports 60 tokens.
    #[tokio::test]
    async fn a_turn_token_budget_soft_lands_with_preserved_work() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Clone)]
        struct VaryingSpendClient(Arc<StdMutex<usize>>);
        impl ApiClient for VaryingSpendClient {
            fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
                let n = {
                    let mut calls = self
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *calls += 1;
                    *calls
                };
                Ok(TurnStream::from_events(vec![
                    AgentEvent::ToolUse {
                        id: format!("t-{n}"),
                        name: "tick".to_string(),
                        input: format!(r#"{{"n":{n}}}"#),
                    },
                    AgentEvent::Usage(TokenUsage {
                        input_tokens: 60,
                        output_tokens: 0,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AgentEvent::MessageStop,
                ]))
            }
        }

        let calls = Arc::new(StdMutex::new(0usize));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            VaryingSpendClient(Arc::clone(&calls)),
            StaticToolExecutor::new().register("tick", |_| Ok("ok".to_string())),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        );
        runtime.set_turn_token_budget(Some(100));
        let mut events: Vec<AgentEvent> = Vec::new();
        let mut notify = |event: &AgentEvent| events.push(event.clone());
        let summary = runtime
            .run_turn("go", None, &mut notify, &CancellationToken::new())
            .await
            .expect("a budget trip is a soft landing, not an error");
        // 60 tokens after iteration 1 (60 < 100 → continue), 120 after
        // iteration 2 → the iteration-3 check trips before any request.
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            2
        );
        assert_eq!(summary.tool_results.len(), 2, "completed work preserved");
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Truncated(reason) if reason.contains("turn token budget (100)")
        )));
        // The spent tokens still reached the cumulative account — the
        // provider billed them.
        assert_eq!(runtime.usage().cumulative_usage().input_tokens, 120);

        // A cleared budget uncaps the next turn (same runtime, budget=None).
        runtime.set_turn_token_budget(None);
        runtime
            .run_turn("again", None, &mut noop_notify(), &CancellationToken::new())
            .await
            .expect("an uncapped turn runs normally");
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
            ContentBlock::ToolResult { is_error: true, output, .. } if output.starts_with("not now")
        ));
        // W5: the denial rides a policy hint so the model does not retry
        // the identical call.
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { output, .. } if output.contains("hint: this is a policy denial")
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

    #[test]
    fn compaction_audit_holds_the_dropped_region_until_taken() {
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
        runtime.reset_for_task(vec![
            ConversationMessage::user_text("one"),
            ConversationMessage::user_text("two"),
            ConversationMessage::user_text("three"),
            ConversationMessage::user_text("four"),
        ]);
        assert!(
            runtime.take_compaction_audit().is_empty(),
            "the first reset has no previous context to archive"
        );

        runtime.compact(CompactionConfig {
            preserve_recent_messages: 1,
            max_estimated_tokens: 1,
            ..CompactionConfig::default()
        });
        let audit = runtime.take_compaction_audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].reason, "auto");
        assert!(!audit[0].messages.is_empty(), "dropped region is kept");
        assert!(runtime.take_compaction_audit().is_empty(), "take clears");

        runtime.reset_for_task(vec![ConversationMessage::user_text("seed")]);
        let audit = runtime.take_compaction_audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].reason, "task_reset");
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

    #[test]
    fn tool_error_hint_rides_the_display_text() {
        // W5: the hint reaches the transcript as a separate line, so a model
        // reading the tool_result sees a fix suggestion, not only the error.
        let plain = ToolError::new("boom");
        assert_eq!(plain.to_string(), "boom");
        let hinted = ToolError::with_hint("denied", "reshape the call");
        assert_eq!(hinted.to_string(), "denied\nhint: reshape the call");
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
            ..CompactionConfig::default()
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

#[cfg(test)]
mod iteration_limit_tests {
    use super::{
        AgentEvent, ApiClient, ApiRequest, ConversationRuntime, RuntimeError, StaticToolExecutor,
        TurnStream,
    };
    use crate::permissions::{PermissionMode, PermissionPolicy};
    use crate::session::Session;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A client that always answers with one tool call: the loop never ends on
    /// its own, so the iteration limit is the only exit. The input varies per
    /// call — an identical batch every iteration would trip the S6 loop
    /// detector (that contract is pinned in `loop_detector_tests` below).
    struct EverCallingClient {
        calls: Arc<AtomicUsize>,
    }

    impl ApiClient for EverCallingClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(TurnStream::from_events(vec![
                AgentEvent::TextDelta("working".to_string()),
                AgentEvent::ToolUse {
                    id: format!("t-{call}"),
                    name: "tick".to_string(),
                    input: format!(r#"{{"n":{call}}}"#),
                },
                AgentEvent::MessageStop,
            ]))
        }
    }

    #[tokio::test]
    async fn iteration_limit_soft_lands_with_preserved_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = EverCallingClient {
            calls: Arc::clone(&calls),
        };
        let executor = StaticToolExecutor::new().register("tick", |_| Ok("ok".to_string()));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            client,
            executor,
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_max_iterations(3);
        let mut events: Vec<AgentEvent> = Vec::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut notify = |event: &AgentEvent| events.push(event.clone());
        let summary = runtime
            .run_turn("keep going", None, &mut notify, &cancel)
            .await
            .expect("soft landing returns Ok, not an error");
        // The counter includes the limit probe (incremented before the
        // check), so it reads one past the completed iterations; the real
        // work count is the API call figure above.
        assert_eq!(summary.iterations, 4);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // Every completed iteration's work is preserved in the summary.
        assert_eq!(summary.tool_results.len(), 3);
        // The truncation note explains the stop instead of an error.
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::Truncated(reason) if reason.contains("iteration limit (3)"))));
    }
}

/// S6 loop detection end-to-end (roadmap §16 R2): an identical tool batch
/// three iterations in a row fails the turn with a Model-class error.
#[cfg(test)]
mod loop_detector_tests {
    use super::{
        AgentEvent, ApiClient, ApiRequest, ContentBlock, ConversationRuntime, MessageRole,
        RuntimeError, Session, StaticToolExecutor, TurnStream,
    };
    use crate::PermissionMode;
    use crate::PermissionPolicy;

    struct SameCallClient {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ApiClient for SameCallClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            Ok(TurnStream::from_events(vec![
                AgentEvent::ToolUse {
                    id: format!("t-{call}"),
                    name: "tick".to_string(),
                    input: "{}".to_string(),
                },
                AgentEvent::MessageStop,
            ]))
        }
    }

    #[tokio::test]
    async fn identical_tool_batches_trip_the_loop_fuse() {
        let executor = StaticToolExecutor::new().register("tick", |_| Ok("ok".to_string()));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SameCallClient {
                calls: std::sync::Arc::clone(&calls),
            },
            executor,
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        )
        .with_max_iterations(64);
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut notify = |_: &AgentEvent| {};
        let error = runtime
            .run_turn("spin", None, &mut notify, &cancel)
            .await
            .expect_err("identical batches must trip the loop fuse");
        assert_eq!(error.kind(), super::ErrorKind::Model);
        assert!(error.to_string().contains("loop detected"), "{error}");
        // D7 (ai-rust-hygiene): pin the trigger ORDER. If MAX_REPEAT_STREAK
        // were ever raised above the iteration cap, this test would otherwise
        // drift into testing the cap's soft landing instead of the fuse. The
        // fuse must fire strictly before the cap would.
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) < 64,
            "the loop fuse must trip before the iteration limit soft-landing"
        );
        // The stopped advertisement never ran, but it must still be answered:
        // an unanswered tool_use makes the provider reject every later request.
        let messages = &runtime.session().messages;
        for message in messages {
            for block in &message.blocks {
                if let ContentBlock::ToolUse { id, .. } = block {
                    assert!(
                        messages.iter().any(|later| later.role == MessageRole::Tool
                            && later.blocks.iter().any(|block| matches!(
                                block,
                                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id
                            ))),
                        "advertisement {id} must be answered"
                    );
                }
            }
        }
    }
}

/// ErrorKind + classifier tests (roadmap §16 R2): the one deliberate
/// string-matching boundary, so its arms are pinned here.
#[cfg(test)]
mod error_kind_tests {
    use super::{classify_stream_error, ErrorKind, RuntimeError};

    #[test]
    fn overflow_wins_over_everything() {
        assert_eq!(
            classify_stream_error("api returned 400 (invalid_request_error): prompt is too long: 210000 tokens > 200000 maximum"),
            ErrorKind::Overflow
        );
    }

    #[test]
    fn auth_markers_classify_auth() {
        assert_eq!(
            classify_stream_error("api returned 401 (authentication_error): invalid x-api-key"),
            ErrorKind::Auth
        );
        assert_eq!(
            classify_stream_error("api returned 403: permission denied"),
            ErrorKind::Auth
        );
    }

    #[test]
    fn transport_markers_classify_transport() {
        for message in [
            "error sending request: connection closed before message completed",
            "stream idle timeout exceeded",
            "api returned 502: bad gateway",
            "provider overloaded_error",
            // reqwest's send-phase wrapper: the only text a connect failure,
            // send timeout or reset shows (the cause stays in the source
            // chain) — it used to fall to Other and cost an immediate claim.
            "http error: error sending request for url (http://127.0.0.1:8401/v1)",
            "invalid sse frame: data line missing after event",
        ] {
            assert_eq!(
                classify_stream_error(message),
                ErrorKind::Transport,
                "{message}"
            );
        }
    }

    #[test]
    fn rate_limits_are_transport_class() {
        // S2: a 429 burst must not poison the autopilot's blocked streak —
        // with or without a numeric status in the text.
        assert_eq!(
            classify_stream_error("api returned 429 (rate_limit_error): too many requests"),
            ErrorKind::Transport
        );
        assert_eq!(
            classify_stream_error("server replied: too many requests, please slow down"),
            ErrorKind::Transport
        );
    }

    #[test]
    fn bad_requests_are_model_class() {
        assert_eq!(
            classify_stream_error("api returned 400 (invalid_request_error): unknown tool name"),
            ErrorKind::Model
        );
    }

    #[test]
    fn unknown_patterns_fall_to_other() {
        assert_eq!(
            classify_stream_error("something completely novel"),
            ErrorKind::Other
        );
    }

    #[test]
    fn digits_inside_ids_timestamps_and_ordinals_do_not_impersonate_a_status() {
        // A bare "401"/"403" substring used to read as Auth — which the
        // autopilot counts as a blocked claim, with no transport backoff.
        assert_eq!(
            classify_stream_error(
                "api returned 429 Too Many Requests: rate limited, request_id 8a1b2c3d4012ef45"
            ),
            ErrorKind::Transport
        );
        assert_eq!(
            classify_stream_error("upstream timed out after 14031ms"),
            ErrorKind::Transport
        );
        // serde_json decode errors quote line/column ordinals: neither is a
        // status ("line 500" is not a server error, "line 403" not a 403).
        assert_eq!(
            classify_stream_error(
                "json error: trailing characters at line 500 column 3 of the stream"
            ),
            ErrorKind::Other
        );
        assert_eq!(
            classify_stream_error("json error: expected value at line 403 column 1"),
            ErrorKind::Other
        );
        // A status that stands on its own still reads.
        assert_eq!(
            classify_stream_error("upstream rejected the tunnel: 403 forbidden by proxy"),
            ErrorKind::Auth
        );
        assert_eq!(classify_stream_error("HTTP 429"), ErrorKind::Transport);
        assert_eq!(classify_stream_error("HTTP 400"), ErrorKind::Model);
    }

    #[test]
    fn exhausted_retries_and_server_errors_are_transport_class() {
        // RetryPolicy only retries faults it already classes as transient, so
        // an exhausted report is never a blocked claim; a bare 500 used to
        // fall through to Other.
        for message in [
            "api request failed after 3 attempts (server error): api returned 500 Internal Server Error: {}",
            "api request failed after 3 attempts (connection failed): http error: error sending request for url (https://api.example.com/v1/chat/completions)",
            "api returned 500 Internal Server Error: {\"error\":{\"message\":\"internal server error\"}}",
            "api returned 520 origin down: error code: 520",
        ] {
            assert_eq!(classify_stream_error(message), ErrorKind::Transport, "{message}");
        }
    }

    #[test]
    fn an_echoed_status_is_authoritative() {
        // A 429 whose body mentions the key is still a rate limit, not an auth
        // failure; a gateway that reports a bad key as a 400 still reports one.
        assert_eq!(
            classify_stream_error(
                "api returned 429 Too Many Requests: rate limit reached for this api key"
            ),
            ErrorKind::Transport
        );
        assert_eq!(
            classify_stream_error("api returned 400 Bad Request: invalid api key"),
            ErrorKind::Auth
        );
        // Permanent client errors are nobody's transient fault to back off from.
        for message in [
            "api returned 404 Not Found: model not found",
            "api returned 413 Payload Too Large",
            "api returned 402 Payment Required: insufficient balance",
        ] {
            assert_eq!(
                classify_stream_error(message),
                ErrorKind::Other,
                "{message}"
            );
        }
    }

    #[test]
    fn runtime_error_kinds_round_trip() {
        let error = RuntimeError::new("plain");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert_eq!(
            RuntimeError::cancelled("turn cancelled").kind(),
            ErrorKind::Cancelled
        );
        let classified = RuntimeError::with_kind(
            ErrorKind::Transport,
            "connection reset",
            Some("check the network".to_string()),
        );
        assert_eq!(classified.kind(), ErrorKind::Transport);
        assert_eq!(classified.hint(), Some("check the network"));
        assert_eq!(ErrorKind::Transport.label(), "transport");
    }
}
