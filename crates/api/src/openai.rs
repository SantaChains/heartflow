use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::ApiError;
use crate::retry::{build_http, is_retryable_status, parse_retry_after, RetryPolicy};
use crate::sse::{SseFrame, SseParser};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1";

/// OpenAI-compatible chat transport (`DeepSeek` native API among others).
///
/// The key is sent as `Authorization: Bearer`; the base URL must include the
/// version segment (for example `https://api.deepseek.com/v1`).
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    retry: RetryPolicy,
}

impl OpenAiClient {
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: build_http(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            retry: RetryPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    #[must_use]
    pub fn with_retry_policy(
        mut self,
        max_retries: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        self.retry = RetryPolicy::new(max_retries, initial_backoff, max_backoff);
        self
    }

    /// One non-streaming completion (fallback path).
    pub async fn send_chat(&self, request: &ChatRequest) -> Result<ChatResponse, ApiError> {
        let request = ChatRequest {
            stream: false,
            stream_options: None,
            ..request.clone()
        };
        let response = self.send_with_retry(&request).await?;
        response
            .json::<ChatResponse>()
            .await
            .map_err(ApiError::from)
    }

    /// Start a streaming completion; chunks arrive as they are emitted.
    pub async fn stream_chat(&self, request: &ChatRequest) -> Result<ChatStream, ApiError> {
        let request = ChatRequest {
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            ..request.clone()
        };
        let response = self.send_with_retry(&request).await?;
        Ok(ChatStream {
            response,
            parser: SseParser::new(),
            pending: VecDeque::new(),
            done: false,
        })
    }

    async fn send_with_retry(&self, request: &ChatRequest) -> Result<reqwest::Response, ApiError> {
        self.retry
            .run(|| self.send_raw_request(request), expect_success)
            .await
    }

    async fn send_raw_request(&self, request: &ChatRequest) -> Result<reqwest::Response, ApiError> {
        let request_url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .post(&request_url)
            .bearer_auth(&self.api_key)
            .json(request)
            .send()
            .await
            .map_err(ApiError::from)?;
        debug!(url = %request_url, status = %response.status(), "openai-compatible request completed");
        Ok(response)
    }

    /// Endpoint root without the trailing version segment, for the
    /// non-`/v1` routes a provider exposes (e.g. `DeepSeek`'s `/user/balance`).
    #[must_use]
    pub fn root_url(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
    }

    /// Self-discovery: list the models an endpoint advertises (`GET {base}/models`).
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ApiError> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self.get(&url).await?;
        let list: ModelList = response.json().await.map_err(ApiError::from)?;
        Ok(list.data)
    }

    /// `DeepSeek` account balance (`GET {root}/user/balance`). Not part of the
    /// generic `OpenAI` surface; callers gate on the provider before invoking.
    pub async fn get_balance(&self) -> Result<Balance, ApiError> {
        let url = format!("{}/user/balance", self.root_url());
        let response = self.get(&url).await?;
        response.json::<Balance>().await.map_err(ApiError::from)
    }

    /// Authenticated `GET` with the shared retry/expect-success path.
    async fn get(&self, url: &str) -> Result<reqwest::Response, ApiError> {
        self.retry
            .run(
                || async {
                    self.http
                        .get(url)
                        .bearer_auth(&self.api_key)
                        .send()
                        .await
                        .map_err(ApiError::from)
                },
                expect_success,
            )
            .await
    }
}

async fn expect_success(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    // Capture `Retry-After` before the body is consumed.
    let retry_after = parse_retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    let parsed_error = serde_json::from_str::<OpenAiErrorEnvelope>(&body).ok();
    let retryable = is_retryable_status(status);

    Err(ApiError::Api {
        status,
        error_type: parsed_error
            .as_ref()
            .and_then(|error| error.error.r#type.clone()),
        message: parsed_error
            .as_ref()
            .map(|error| error.error.message.clone()),
        body,
        retryable,
        retry_after,
    })
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiErrorBody,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorBody {
    message: String,
    r#type: Option<String>,
}

/// Streaming handle over `chat/completions` SSE frames.
pub struct ChatStream {
    response: reqwest::Response,
    parser: SseParser,
    pending: VecDeque<ChatChunk>,
    done: bool,
}

impl ChatStream {
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>, ApiError> {
        loop {
            if let Some(chunk) = self.pending.pop_front() {
                return Ok(Some(chunk));
            }

            if self.done {
                return Ok(None);
            }

            let frames: Vec<SseFrame> = if let Some(chunk) = self.response.chunk().await? {
                self.parser.push(&chunk)?
            } else {
                self.done = true;
                self.parser.finish()?
            };

            for frame in frames {
                if frame.data.is_empty() || frame.data == "[DONE]" {
                    continue;
                }
                let chunk: ChatChunk = serde_json::from_str(&frame.data).map_err(ApiError::from)?;
                self.pending.push_back(chunk);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types (OpenAI chat-completions dialect, DeepSeek extensions included)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ChatToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// `DeepSeek` thinking control; `Some` enables reasoning mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingControl>,
    /// `DeepSeek`/`OpenAI` reasoning effort ("low" | "medium" | "high").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ThinkingControl {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl ThinkingControl {
    #[must_use]
    pub const fn enabled() -> Self {
        Self { kind: "enabled" }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatToolChoice {
    Auto,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamOptions {
    #[serde(rename = "include_usage")]
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ChatContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Message content in either form the dialect accepts: a bare string, or an
/// ordered list of parts when the turn carries images.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ChatContentPart>),
}

impl ChatContent {
    /// Concatenate the text parts; image parts contribute nothing.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ChatContentPart::Text { text } => Some(text.as_str()),
                    ChatContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatContentPart {
    Text { text: String },
    ImageUrl { image_url: ChatImageUrl },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatImageUrl {
    /// Either an `http(s)` URL or an inline `data:<media>;base64,<payload>` URL.
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub call_type: ChatToolCallType,
    pub function: ChatFunctionCall,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatToolCallType {
    Function,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub tool_type: ChatToolCallType,
    pub function: ChatToolSpec,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChatToolSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<ChatChoice>,
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChoice {
    pub message: ChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChatChunkChoice>,
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunkChoice {
    #[serde(default)]
    pub delta: ChatDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// One streamed delta. `reasoning_content` is the DeepSeek-native
/// reasoning stream; plain `OpenAI` servers simply omit it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChatDeltaToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatDeltaToolCall {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChatDeltaFunction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatDeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Usage with DeepSeek-native cache accounting.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ChatUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u32>,
}

/// One model entry from `GET {base}/models`. Servers differ on how much they
/// disclose, so everything but `id` is optional; `context_length` is present on
/// self-hosted OpenAI-compatible servers (llama.cpp, vLLM) but not `DeepSeek`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub owned_by: Option<String>,
    #[serde(default)]
    pub context_length: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelInfo>,
}

/// `DeepSeek` account balance per currency. Amounts are strings on the wire to
/// preserve precision, so they are kept as `String` rather than coerced.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BalanceInfo {
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub total_balance: String,
    #[serde(default)]
    pub granted_balance: String,
    #[serde(default)]
    pub topped_up_balance: String,
}

/// Envelope for `DeepSeek` `GET {root}/user/balance`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Balance {
    #[serde(default)]
    pub is_available: bool,
    #[serde(default)]
    pub balance_infos: Vec<BalanceInfo>,
}

#[cfg(test)]
mod tests {
    use super::{
        ChatChunk, ChatContent, ChatMessage, ChatRequest, ChatRole, ChatTool, ChatToolCallType,
        ChatToolChoice, ChatToolSpec, ChatUsage, OpenAiClient, ThinkingControl,
    };
    use serde_json::json;

    #[test]
    fn chat_request_serializes_with_openai_field_names() {
        let request = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: Some(ChatContent::Text("hi".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            max_tokens: Some(512),
            stream: true,
            tools: Some(vec![ChatTool {
                tool_type: ChatToolCallType::Function,
                function: ChatToolSpec {
                    name: "bash".to_string(),
                    description: Some("run".to_string()),
                    parameters: json!({"type": "object"}),
                },
            }]),
            tool_choice: Some(ChatToolChoice::Auto),
            stream_options: None,
            thinking: None,
            reasoning_effort: None,
        };

        let value: serde_json::Value =
            serde_json::to_value(&request).expect("request should serialize");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["tool_choice"], "auto");
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "bash");
        assert!(value.get("stream_options").is_none());
        assert!(value.get("thinking").is_none());
        assert!(value.get("reasoning_effort").is_none());
    }

    #[test]
    fn serializes_thinking_control_when_enabled() {
        let request = ChatRequest {
            model: "deepseek-flash".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: Some(ChatContent::Text("hi".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            max_tokens: Some(512),
            stream: false,
            tools: None,
            tool_choice: None,
            stream_options: None,
            thinking: Some(ThinkingControl::enabled()),
            reasoning_effort: Some("high".to_string()),
        };

        let value: serde_json::Value =
            serde_json::to_value(&request).expect("request should serialize");
        assert_eq!(value["thinking"]["type"], "enabled");
        assert_eq!(value["reasoning_effort"], "high");
    }

    #[test]
    fn parses_reasoning_and_tool_call_delta_chunks() {
        let chunk: ChatChunk = serde_json::from_value(json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "thinking...",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {"name": "bash", "arguments": "{\"co"}
                    }]
                },
                "finish_reason": null
            }]
        }))
        .expect("chunk should parse");

        let delta = &chunk.choices[0].delta;
        assert_eq!(delta.reasoning_content.as_deref(), Some("thinking..."));
        let call = &delta.tool_calls.as_ref().expect("tool calls")[0];
        assert_eq!(call.index, 0);
        assert_eq!(call.id.as_deref(), Some("call_1"));
        assert_eq!(
            call.function.as_ref().expect("function").name.as_deref(),
            Some("bash")
        );
    }

    #[test]
    fn parses_usage_with_cache_fields() {
        let chunk: ChatChunk = serde_json::from_value(json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 8192,
                "completion_tokens": 16,
                "prompt_cache_hit_tokens": 8192,
                "prompt_cache_miss_tokens": 0
            }
        }))
        .expect("usage chunk should parse");

        let usage: ChatUsage = chunk.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 8192);
        assert_eq!(usage.prompt_cache_hit_tokens, Some(8192));
        assert_eq!(usage.completion_tokens, 16);
    }

    #[test]
    fn parses_tool_message_and_assistant_tool_calls() {
        let message: ChatMessage = serde_json::from_value(json!({
            "role": "tool",
            "content": "ok",
            "tool_call_id": "call_1"
        }))
        .expect("tool message should parse");
        assert_eq!(message.role, ChatRole::Tool);

        let message: ChatMessage = serde_json::from_value(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]
        }))
        .expect("assistant message should parse");
        assert_eq!(message.content, None);
        assert_eq!(message.tool_calls.expect("calls").len(), 1);
    }

    #[test]
    fn chunk_without_choices_is_tolerated() {
        let chunk: ChatChunk =
            serde_json::from_value(json!({ "choices": [] })).expect("chunk should parse");
        assert!(chunk.choices.is_empty());
        assert!(chunk.usage.is_none());
    }

    #[test]
    fn parses_model_list_with_optional_context() {
        let list: super::ModelList = serde_json::from_value(json!({
            "object": "list",
            "data": [
                { "id": "deepseek-chat", "owned_by": "DeepSeek" },
                { "id": "llama", "context_length": 8192 }
            ]
        }))
        .expect("model list should parse");
        assert_eq!(list.data.len(), 2);
        assert_eq!(list.data[0].id, "deepseek-chat");
        assert_eq!(list.data[0].context_length, None);
        assert_eq!(list.data[1].context_length, Some(8192));
    }

    #[test]
    fn parses_balance_envelope() {
        let balance: super::Balance = serde_json::from_value(json!({
            "is_available": true,
            "balance_infos": [{
                "currency": "CNY",
                "total_balance": "12.34",
                "granted_balance": "0.00",
                "topped_up_balance": "12.34"
            }]
        }))
        .expect("balance should parse");
        assert!(balance.is_available);
        assert_eq!(balance.balance_infos[0].total_balance, "12.34");
    }

    #[test]
    fn root_url_strips_version_segment() {
        let client = OpenAiClient::new("k").with_base_url("https://api.deepseek.com/v1");
        assert_eq!(client.root_url(), "https://api.deepseek.com");
        let client = OpenAiClient::new("k").with_base_url("https://x/v1/");
        assert_eq!(client.root_url(), "https://x");
        let client = OpenAiClient::new("k").with_base_url("http://localhost:8000");
        assert_eq!(client.root_url(), "http://localhost:8000");
    }
}
