use std::collections::VecDeque;
use std::time::Duration;

use serde::Deserialize;
use tracing::debug;

use crate::error::ApiError;
use crate::retry::{build_http, is_retryable_status, RetryPolicy};
use crate::sse::{SseFrame, SseParser};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// `OpenAI` `Responses` API transport (`POST {base}/responses`), the dialect
/// `DeepSeek` and other gateways expose alongside `chat/completions`.
///
/// The request body is assembled by the caller as JSON `Value`s (input items,
/// tools), keeping this transport thin over the wire contract.
#[derive(Debug, Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    retry: RetryPolicy,
}

impl ResponsesClient {
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

    fn endpoint(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    /// One non-streaming request (fallback path).
    pub async fn send(&self, body: &serde_json::Value) -> Result<ResponsesResponse, ApiError> {
        let body = with_flag(body, false);
        let response = self.send_with_retry(&body).await?;
        response
            .json::<ResponsesResponse>()
            .await
            .map_err(ApiError::from)
    }

    /// Start a streaming request; typed events arrive as they are emitted.
    pub async fn stream(&self, body: &serde_json::Value) -> Result<ResponsesStream, ApiError> {
        let body = with_flag(body, true);
        let response = self.send_with_retry(&body).await?;
        Ok(ResponsesStream {
            response,
            parser: SseParser::new(),
            pending: VecDeque::new(),
            done: false,
        })
    }

    async fn send_with_retry(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, ApiError> {
        self.retry
            .run(|| self.send_raw(body.clone()), expect_success)
            .await
    }

    async fn send_raw(&self, body: serde_json::Value) -> Result<reqwest::Response, ApiError> {
        let url = self.endpoint();
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(ApiError::from)?;
        debug!(url = %url, status = %response.status(), "responses request completed");
        Ok(response)
    }
}

/// Inject/overwrite the `stream` flag on the caller-built body.
fn with_flag(body: &serde_json::Value, stream: bool) -> serde_json::Value {
    let mut value = body.clone();
    if let Some(map) = value.as_object_mut() {
        map.insert("stream".to_string(), serde_json::Value::Bool(stream));
    }
    value
}

async fn expect_success(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    Err(ApiError::Api {
        status,
        error_type: None,
        message,
        body,
        retryable: is_retryable_status(status),
    })
}

/// Streaming handle over `responses` SSE frames; yields parsed typed events.
pub struct ResponsesStream {
    response: reqwest::Response,
    parser: SseParser,
    pending: VecDeque<ResponsesEvent>,
    done: bool,
}

impl ResponsesStream {
    pub async fn next_event(&mut self) -> Result<Option<ResponsesEvent>, ApiError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
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
                if let Ok(event) = serde_json::from_str::<ResponsesEvent>(&frame.data) {
                    self.pending.push_back(event);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A request body assembled by the adapter; kept untyped here on purpose.
pub type ResponsesRequest = serde_json::Value;

/// One streamed Responses event. Only the fields the agent loop consumes are
/// modeled; unknown fields are ignored so forward-compatible events parse fine.
#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub delta: Option<String>,
    #[serde(default)]
    pub item: Option<ResponsesItem>,
    #[serde(default)]
    pub response: Option<ResponsesPayload>,
}

/// An output item carried in `output_item.added` / `.done`.
#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesItem {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// The terminal `response` payload (usage on completion).
#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesPayload {
    #[serde(default)]
    pub usage: Option<ResponsesUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub input_tokens_details: Option<InputTokenDetails>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InputTokenDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

/// Non-streaming response body.
#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesResponse {
    #[serde(default)]
    pub output: Vec<ResponsesOutputItem>,
    #[serde(default)]
    pub usage: Option<ResponsesUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesOutputItem {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub content: Option<Vec<ResponsesContent>>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesContent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{ResponsesEvent, ResponsesResponse};
    use serde_json::json;

    #[test]
    fn parses_text_and_function_call_deltas() {
        let text: ResponsesEvent =
            serde_json::from_value(json!({ "type": "response.output_text.delta", "delta": "hi" }))
                .expect("event");
        assert_eq!(text.kind, "response.output_text.delta");
        assert_eq!(text.delta.as_deref(), Some("hi"));

        let added: ResponsesEvent = serde_json::from_value(json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "call_id": "c1", "name": "bash" }
        }))
        .expect("event");
        let item = added.item.expect("item");
        assert_eq!(item.kind, "function_call");
        assert_eq!(item.call_id.as_deref(), Some("c1"));
        assert_eq!(item.name.as_deref(), Some("bash"));
    }

    #[test]
    fn parses_completed_usage_with_cache() {
        let done: ResponsesEvent = serde_json::from_value(json!({
            "type": "response.completed",
            "response": { "usage": {
                "input_tokens": 100, "output_tokens": 20,
                "input_tokens_details": { "cached_tokens": 64 }
            }}
        }))
        .expect("event");
        let usage = done.response.expect("response").usage.expect("usage");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.input_tokens_details.expect("det").cached_tokens, 64);
    }

    #[test]
    fn ignores_unknown_event_fields() {
        let event: ResponsesEvent = serde_json::from_value(json!({
            "type": "response.in_progress",
            "sequence_number": 3,
            "unexpected": { "deep": true }
        }))
        .expect("unknown events should still parse");
        assert_eq!(event.kind, "response.in_progress");
        assert!(event.delta.is_none());
    }

    #[test]
    fn parses_non_streaming_output() {
        let response: ResponsesResponse = serde_json::from_value(json!({
            "output": [
                { "type": "message", "content": [{ "type": "output_text", "text": "hello" }] },
                { "type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{}" }
            ],
            "usage": { "input_tokens": 5, "output_tokens": 2 }
        }))
        .expect("response");
        assert_eq!(response.output.len(), 2);
        assert_eq!(
            response.output[0].content.as_ref().expect("content")[0]
                .text
                .as_deref(),
            Some("hello")
        );
        assert_eq!(response.output[1].name.as_deref(), Some("bash"));
    }
}
