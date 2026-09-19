use runtime::{ContentBlock, MessageRole, TokenUsage};

/// Metadata describing a saved conversation, stored alongside its messages.
#[derive(Debug, Clone, Default)]
pub struct SessionMeta {
    /// When the session first entered the store (unix seconds).
    pub created_at: i64,
    /// When the session was last written (unix seconds).
    pub updated_at: i64,
    /// Path of the JSON file this session was imported from, if any.
    pub source_path: Option<String>,
    /// Provider id the session ran against, for analytics.
    pub provider: Option<String>,
    /// Model id the session ran against, for analytics.
    pub model: Option<String>,
}

/// A compact row returned by `Store::list_sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub message_count: i64,
    /// Truncated text of the first user message, for display.
    pub preview: String,
}

/// A single match returned by `Store::search`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub session_id: String,
    pub seq: i64,
    pub role: MessageRole,
    /// A bounded snippet of the matched message's searchable text.
    pub snippet: String,
    /// Which retrieval path produced the hit, for diagnostics/tests.
    pub method: SearchMethod,
}

/// The retrieval strategy that matched a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMethod {
    /// FTS5 `trigram` index (used when the query has >= 3 code points).
    Fts,
    /// `LIKE` substring scan (used for short queries, incl. 1-2 char CJK).
    Like,
}

/// Aggregate token counters across stored messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub messages_with_usage: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
}

/// Stable snake-case string for a role, matching the JSON serde representation.
#[must_use]
pub fn role_str(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

/// Parse a stored role string back into `MessageRole`.
pub fn role_from_str(s: &str) -> Result<MessageRole, crate::error::StoreError> {
    Ok(match s {
        "system" => MessageRole::System,
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "tool" => MessageRole::Tool,
        other => {
            return Err(crate::error::StoreError::Format(format!(
                "unknown role: {other}"
            )))
        }
    })
}

/// Flatten a message's blocks into one searchable text blob.
///
/// Tool inputs and outputs are included so commands and their results are
/// findable, but JSON punctuation from the block envelopes never leaks in.
#[must_use]
pub fn flatten_search_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        let piece = match block {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::ToolUse { name, input, .. } => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(name);
                out.push(' ');
                out.push_str(input);
                continue;
            }
            ContentBlock::ToolResult { output, .. } => output.as_str(),
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(piece);
    }
    out
}

/// Build a `TokenUsage` from the numeric columns of a stored message row.
#[must_use]
pub fn usage_from_columns(
    has_usage: bool,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
) -> Option<TokenUsage> {
    if !has_usage {
        return None;
    }
    Some(TokenUsage {
        input_tokens: u32::try_from(input_tokens).unwrap_or_default(),
        output_tokens: u32::try_from(output_tokens).unwrap_or_default(),
        cache_creation_input_tokens: u32::try_from(cache_creation_input_tokens).unwrap_or_default(),
        cache_read_input_tokens: u32::try_from(cache_read_input_tokens).unwrap_or_default(),
    })
}
