use std::collections::VecDeque;
use std::time::Duration;

use serde::Deserialize;
use tracing::debug;

use crate::error::ApiError;
use crate::retry::{build_http, is_retryable_status, RetryPolicy};
use crate::sse::{anthropic_event, SseParser};
use crate::types::{MessageRequest, MessageResponse, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const REQUEST_ID_HEADER: &str = "request-id";
const ALT_REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Debug, Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    auth_token: Option<String>,
    base_url: String,
    retry: RetryPolicy,
}

impl AnthropicClient {
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: build_http(),
            api_key: api_key.into(),
            auth_token: None,
            base_url: DEFAULT_BASE_URL.to_string(),
            retry: RetryPolicy::default(),
        }
    }

    pub fn from_env() -> Result<Self, ApiError> {
        Ok(Self::new(read_api_key()?)
            .with_auth_token(read_auth_token())
            .with_base_url(read_base_url()))
    }

    #[must_use]
    pub fn with_auth_token(mut self, auth_token: Option<String>) -> Self {
        self.auth_token = auth_token.filter(|token| !token.is_empty());
        self
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

    pub async fn send_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        let request = MessageRequest {
            stream: false,
            ..request.clone()
        };
        let response = self.send_with_retry(&request).await?;
        let request_id = request_id_from_headers(response.headers());
        let mut response = response
            .json::<MessageResponse>()
            .await
            .map_err(ApiError::from)?;
        if response.request_id.is_none() {
            response.request_id = request_id;
        }
        Ok(response)
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageStream, ApiError> {
        let response = self
            .send_with_retry(&request.clone().with_streaming())
            .await?;
        Ok(MessageStream {
            request_id: request_id_from_headers(response.headers()),
            response,
            parser: SseParser::new(),
            pending: VecDeque::new(),
            done: false,
        })
    }

    async fn send_with_retry(
        &self,
        request: &MessageRequest,
    ) -> Result<reqwest::Response, ApiError> {
        self.retry
            .run(|| self.send_raw_request(request), expect_success)
            .await
    }

    async fn send_raw_request(
        &self,
        request: &MessageRequest,
    ) -> Result<reqwest::Response, ApiError> {
        let request_url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut request_builder = self
            .http
            .post(&request_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");

        if let Some(auth_token) = &self.auth_token {
            request_builder = request_builder.bearer_auth(auth_token);
        }

        let response = request_builder
            .json(request)
            .send()
            .await
            .map_err(ApiError::from)?;
        debug!(url = %request_url, status = %response.status(), "anthropic request completed");
        Ok(response)
    }
}

fn read_api_key() -> Result<String, ApiError> {
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(api_key) if !api_key.is_empty() => Ok(api_key),
        Ok(_) => Err(ApiError::MissingApiKey),
        Err(std::env::VarError::NotPresent) => match std::env::var("ANTHROPIC_AUTH_TOKEN") {
            Ok(api_key) if !api_key.is_empty() => Ok(api_key),
            Ok(_) | Err(std::env::VarError::NotPresent) => Err(ApiError::MissingApiKey),
            Err(error) => Err(ApiError::from(error)),
        },
        Err(error) => Err(ApiError::from(error)),
    }
}

fn read_auth_token() -> Option<String> {
    match std::env::var("ANTHROPIC_AUTH_TOKEN") {
        Ok(token) if !token.is_empty() => Some(token),
        _ => None,
    }
}

fn read_base_url() -> String {
    std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

fn request_id_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(REQUEST_ID_HEADER)
        .or_else(|| headers.get(ALT_REQUEST_ID_HEADER))
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

#[derive(Debug)]
pub struct MessageStream {
    request_id: Option<String>,
    response: reqwest::Response,
    parser: SseParser,
    pending: VecDeque<StreamEvent>,
    done: bool,
}

impl MessageStream {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ApiError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }

            if self.done {
                for frame in self.parser.finish()? {
                    if let Some(event) = anthropic_event(&frame)? {
                        self.pending.push_back(event);
                    }
                }
                if let Some(event) = self.pending.pop_front() {
                    return Ok(Some(event));
                }
                return Ok(None);
            }

            match self.response.chunk().await? {
                Some(chunk) => {
                    for frame in self.parser.push(&chunk)? {
                        if let Some(event) = anthropic_event(&frame)? {
                            self.pending.push_back(event);
                        }
                    }
                }
                None => {
                    self.done = true;
                }
            }
        }
    }
}

async fn expect_success(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_else(|_| String::new());
    let parsed_error = serde_json::from_str::<AnthropicErrorEnvelope>(&body).ok();
    let retryable = is_retryable_status(status);

    Err(ApiError::Api {
        status,
        error_type: parsed_error
            .as_ref()
            .map(|error| error.error.error_type.clone()),
        message: parsed_error
            .as_ref()
            .map(|error| error.error.message.clone()),
        body,
        retryable,
    })
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorEnvelope {
    error: AnthropicErrorBody,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorBody {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::{ALT_REQUEST_ID_HEADER, REQUEST_ID_HEADER};

    use crate::types::{ContentBlockDelta, MessageRequest};

    // The env-based lookups share process-global state, so all precedence
    // cases run inside one sequential test to avoid parallel interference.
    #[test]
    fn read_api_key_env_precedence() {
        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "auth-token");
        std::env::set_var("ANTHROPIC_API_KEY", "legacy-key");
        assert_eq!(
            super::read_api_key().expect("api key should load"),
            "legacy-key"
        );

        std::env::remove_var("ANTHROPIC_API_KEY");
        assert_eq!(super::read_auth_token().as_deref(), Some("auth-token"));

        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "");
        let error = super::read_api_key().expect_err("empty key should error");
        assert!(matches!(error, crate::error::ApiError::MissingApiKey));

        std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
        let error = super::read_api_key().expect_err("missing key should error");
        assert!(matches!(error, crate::error::ApiError::MissingApiKey));
    }

    #[test]
    fn message_request_stream_helper_sets_stream_true() {
        let request = MessageRequest {
            model: "claude-3-7-sonnet-latest".to_string(),
            max_tokens: 64,
            messages: vec![],
            system: None,
            tools: None,
            tool_choice: None,
            stream: false,
            thinking: None,
        };

        assert!(request.with_streaming().stream);
    }

    #[test]
    fn tool_delta_variant_round_trips() {
        let delta = ContentBlockDelta::InputJsonDelta {
            partial_json: "{\"city\":\"Paris\"}".to_string(),
        };
        let encoded = serde_json::to_string(&delta).expect("delta should serialize");
        let decoded: ContentBlockDelta =
            serde_json::from_str(&encoded).expect("delta should deserialize");
        assert_eq!(decoded, delta);
    }

    #[test]
    fn request_id_uses_primary_or_fallback_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(REQUEST_ID_HEADER, "req_primary".parse().expect("header"));
        assert_eq!(
            super::request_id_from_headers(&headers).as_deref(),
            Some("req_primary")
        );

        headers.clear();
        headers.insert(
            ALT_REQUEST_ID_HEADER,
            "req_fallback".parse().expect("header"),
        );
        assert_eq!(
            super::request_id_from_headers(&headers).as_deref(),
            Some("req_fallback")
        );
    }
}
