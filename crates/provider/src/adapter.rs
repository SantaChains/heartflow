use std::collections::BTreeMap;

use api::{
    AnthropicClient, ApiError, ChatContent, ChatContentPart, ChatFunctionCall, ChatImageUrl,
    ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatTool, ChatToolCall, ChatToolCallType,
    ChatToolChoice, ChatToolSpec, ContentBlockDelta, ImageSource, InputContentBlock, InputMessage,
    MessageRequest, MessageResponse, OpenAiClient, OutputContentBlock, ResponsesClient,
    ResponsesEvent, ResponsesPayload, ResponsesResponse, ResponsesUsage, StreamEvent, Thinking,
    ThinkingControl, ToolChoice, ToolDefinition, ToolResultContentBlock,
};
use runtime::{
    AgentEvent, ApiClient, ApiRequest, ContentBlock, ConversationMessage, MessageRole,
    RuntimeError, TokenUsage, ToolSpec, TurnStream,
};
use tokio::sync::mpsc;
use tracing::debug;

use crate::config::{ProviderProfile, ProviderProtocol};

const DEFAULT_MAX_TOKENS: u32 = 4096;
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Bridges the blocking `ApiClient` seam to the async Anthropic transport.
///
/// `stream` spawns a producer task that forwards SSE deltas into a bounded
/// channel as they arrive, so the UI observes tokens at wire speed.
pub struct AnthropicStreamClient {
    client: AnthropicClient,
    model: String,
    max_tokens: u32,
    enable_tools: bool,
    reasoning_effort: Option<String>,
}

impl AnthropicStreamClient {
    /// Legacy environment-driven construction (`ANTHROPIC_*` variables).
    pub fn from_env(model: impl Into<String>, enable_tools: bool) -> Result<Self, ApiError> {
        Ok(Self {
            client: AnthropicClient::from_env()?,
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            enable_tools,
            reasoning_effort: None,
        })
    }

    /// Explicit provider construction for configured profiles.
    #[must_use]
    pub fn from_profile(profile: &ProviderProfile, enable_tools: bool) -> Self {
        let client = AnthropicClient::new(profile.api_key.clone())
            .with_auth_token(profile.auth_token.clone())
            .with_base_url(profile.base_url.clone());
        Self {
            client,
            model: profile.model.clone(),
            max_tokens: profile.max_tokens,
            enable_tools,
            reasoning_effort: profile.reasoning_effort.clone(),
        }
    }
}

impl ApiClient for AnthropicStreamClient {
    fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = self.client.clone();
        let message_request = build_message_request(
            &request,
            &self.model,
            self.max_tokens,
            self.enable_tools,
            self.reasoning_effort.as_deref(),
        );

        let handle = tokio::spawn(async move {
            if let Err(error) = drive_stream(&client, message_request, &tx).await {
                let _ = tx.send(AgentEvent::Error(error.to_string())).await;
            }
        });

        Ok(TurnStream::new(rx, handle.abort_handle()))
    }
}

/// OpenAI-compatible (`chat/completions`) transport bridge.
pub struct OpenAiStreamClient {
    client: OpenAiClient,
    model: String,
    max_tokens: u32,
    enable_tools: bool,
    reasoning_effort: Option<String>,
}

impl OpenAiStreamClient {
    #[must_use]
    pub fn from_profile(profile: &ProviderProfile, enable_tools: bool) -> Self {
        let client =
            OpenAiClient::new(profile.api_key.clone()).with_base_url(profile.base_url.clone());
        Self {
            client,
            model: profile.model.clone(),
            max_tokens: profile.max_tokens,
            enable_tools,
            reasoning_effort: profile.reasoning_effort.clone(),
        }
    }
}

impl ApiClient for OpenAiStreamClient {
    fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = self.client.clone();
        let chat_request = build_chat_request(
            &request,
            &self.model,
            self.max_tokens,
            self.enable_tools,
            self.reasoning_effort.clone(),
        );

        let handle = tokio::spawn(async move {
            if let Err(error) = drive_openai_stream(&client, chat_request, &tx).await {
                let _ = tx.send(AgentEvent::Error(error.to_string())).await;
            }
        });

        Ok(TurnStream::new(rx, handle.abort_handle()))
    }
}

/// Protocol-dispatching transport chosen by the provider profile.
pub enum TransportClient {
    Anthropic(AnthropicStreamClient),
    OpenAi(OpenAiStreamClient),
    OpenAiResponses(ResponsesStreamClient),
}

impl TransportClient {
    #[must_use]
    pub fn from_profile(profile: &ProviderProfile, enable_tools: bool) -> Self {
        match profile.protocol {
            ProviderProtocol::Anthropic => {
                Self::Anthropic(AnthropicStreamClient::from_profile(profile, enable_tools))
            }
            ProviderProtocol::OpenAi => {
                Self::OpenAi(OpenAiStreamClient::from_profile(profile, enable_tools))
            }
            ProviderProtocol::OpenAiResponses => {
                Self::OpenAiResponses(ResponsesStreamClient::from_profile(profile, enable_tools))
            }
        }
    }
}

impl ApiClient for TransportClient {
    fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        match self {
            Self::Anthropic(client) => client.stream(request),
            Self::OpenAi(client) => client.stream(request),
            Self::OpenAiResponses(client) => client.stream(request),
        }
    }
}

async fn drive_stream(
    client: &AnthropicClient,
    request: MessageRequest,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(), ApiError> {
    let mut stream = client.stream_message(&request).await?;
    let mut pending_tool: Option<(String, String, String)> = None;
    let mut saw_content = false;
    let mut saw_stop = false;
    // `message_start` carries the authoritative input/cache counts; the
    // `message_delta` usage only updates output (and sometimes input).
    let mut input_tokens: u32 = 0;
    let mut cache_creation_input_tokens: u32 = 0;
    let mut cache_read_input_tokens: u32 = 0;

    while let Some(event) = stream.next_event().await? {
        match event {
            StreamEvent::MessageStart(start) => {
                input_tokens = start.message.usage.input_tokens;
                cache_creation_input_tokens = start.message.usage.cache_creation_input_tokens;
                cache_read_input_tokens = start.message.usage.cache_read_input_tokens;
                for block in start.message.content {
                    emit_block(block, tx, &mut pending_tool, &mut saw_content).await;
                }
            }
            StreamEvent::ContentBlockStart(start) => {
                emit_block(start.content_block, tx, &mut pending_tool, &mut saw_content).await;
            }
            StreamEvent::ContentBlockDelta(delta) => match delta.delta {
                ContentBlockDelta::TextDelta { text } if !text.is_empty() => {
                    saw_content = true;
                    let _ = tx.send(AgentEvent::TextDelta(text)).await;
                }
                ContentBlockDelta::InputJsonDelta { partial_json } => {
                    if let Some((_, _, input)) = &mut pending_tool {
                        append_tool_input(input, &partial_json);
                    }
                }
                ContentBlockDelta::ThinkingDelta { thinking } if !thinking.is_empty() => {
                    let _ = tx.send(AgentEvent::ThinkingDelta(thinking)).await;
                }
                _ => {}
            },
            StreamEvent::ContentBlockStop(_) => {
                if let Some((id, name, input)) = pending_tool.take() {
                    saw_content = true;
                    let _ = tx.send(AgentEvent::ToolUse { id, name, input }).await;
                }
            }
            StreamEvent::MessageDelta(delta) => {
                // Delta values win when present (some endpoints report the
                // full usage here); message_start counts fill the gaps.
                if delta.usage.input_tokens > 0 {
                    input_tokens = delta.usage.input_tokens;
                }
                if delta.usage.cache_creation_input_tokens > 0 {
                    cache_creation_input_tokens = delta.usage.cache_creation_input_tokens;
                }
                if delta.usage.cache_read_input_tokens > 0 {
                    cache_read_input_tokens = delta.usage.cache_read_input_tokens;
                }
                let _ = tx
                    .send(AgentEvent::Usage(TokenUsage {
                        input_tokens,
                        output_tokens: delta.usage.output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    }))
                    .await;
            }
            StreamEvent::MessageStop(_) => {
                saw_stop = true;
                let _ = tx.send(AgentEvent::MessageStop).await;
                break;
            }
        }
    }

    if saw_stop {
        return Ok(());
    }
    if saw_content {
        // Server closed the stream after delivering content: keep what arrived.
        debug!("stream closed after content without message_stop; accepting delivered blocks");
        let _ = tx.send(AgentEvent::MessageStop).await;
        return Ok(());
    }
    // Nothing usable arrived on the stream; retry once without streaming.
    debug!("stream delivered no content; falling back to non-streaming request");
    // Extended thinking requires streaming; drop it for the non-streaming retry.
    let fallback = MessageRequest {
        thinking: None,
        ..request
    };
    let response = client.send_message(&fallback).await?;
    emit_response(response, tx).await;
    Ok(())
}

/// Accumulate tool-input JSON deltas.
///
/// `content_block_start.input` is `{}` by spec; some providers (`DeepSeek`'s
/// Anthropic-compatible endpoint) resend the full object as a delta, so an
/// untouched placeholder is replaced by the first delta instead of prepended.
fn append_tool_input(input: &mut String, delta: &str) {
    if input == "{}" {
        *input = delta.to_string();
    } else {
        input.push_str(delta);
    }
}

async fn emit_block(
    block: OutputContentBlock,
    tx: &mpsc::Sender<AgentEvent>,
    pending_tool: &mut Option<(String, String, String)>,
    saw_content: &mut bool,
) {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                *saw_content = true;
                let _ = tx.send(AgentEvent::TextDelta(text)).await;
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            *pending_tool = Some((id, name, input.to_string()));
        }
        OutputContentBlock::Thinking { .. } => {}
    }
}

async fn emit_response(response: MessageResponse, tx: &mpsc::Sender<AgentEvent>) {
    for block in response.content {
        match block {
            OutputContentBlock::Text { text } => {
                if !text.is_empty() {
                    let _ = tx.send(AgentEvent::TextDelta(text)).await;
                }
            }
            OutputContentBlock::ToolUse { id, name, input } => {
                let _ = tx
                    .send(AgentEvent::ToolUse {
                        id,
                        name,
                        input: input.to_string(),
                    })
                    .await;
            }
            OutputContentBlock::Thinking { .. } => {}
        }
    }
    let _ = tx
        .send(AgentEvent::Usage(TokenUsage {
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
        }))
        .await;
    let _ = tx.send(AgentEvent::MessageStop).await;
}

fn build_message_request(
    request: &ApiRequest,
    model: &str,
    max_tokens: u32,
    enable_tools: bool,
    reasoning_effort: Option<&str>,
) -> MessageRequest {
    MessageRequest {
        model: model.to_string(),
        max_tokens,
        messages: convert_messages(&request.messages),
        system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.join("\n\n")),
        tools: enable_tools.then(|| {
            request
                .tools
                .iter()
                .map(|spec: &ToolSpec| ToolDefinition {
                    name: spec.name.clone(),
                    description: Some(spec.description.clone()),
                    input_schema: spec.input_schema.clone(),
                })
                .collect()
        }),
        tool_choice: enable_tools.then_some(ToolChoice::Auto),
        stream: true,
        thinking: anthropic_thinking(reasoning_effort, max_tokens),
    }
}

/// Map a configured reasoning effort to an Anthropic extended-thinking budget.
/// Returns `None` when the effort is unset/blank or `max_tokens` is too small
/// to leave a legal budget (thinking tokens count against `max_tokens` and the
/// API requires `budget_tokens < max_tokens`), so requests stay wire-legal and
/// default behavior is unchanged for users who do not opt in.
fn anthropic_thinking(reasoning_effort: Option<&str>, max_tokens: u32) -> Option<Thinking> {
    let effort = reasoning_effort?.trim();
    if effort.is_empty() {
        return None;
    }
    let requested: u32 = match effort {
        "low" => 1024,
        "medium" => 4096,
        _ => 8192, // "high" and any explicit value default to a generous budget
    };
    let budget = requested.min(max_tokens.saturating_sub(1024));
    if budget < 1024 {
        return None;
    }
    Some(Thinking::enabled(budget))
}

fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };
            let content = message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => InputContentBlock::Text { text: text.clone() },
                    ContentBlock::ToolUse { id, name, input } => InputContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::from_str(input)
                            .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        is_error,
                        ..
                    } => InputContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: vec![ToolResultContentBlock::Text {
                            text: output.clone(),
                        }],
                        is_error: *is_error,
                    },
                    ContentBlock::Image { media_type, data } => InputContentBlock::Image {
                        source: ImageSource::base64(media_type.clone(), data.clone()),
                    },
                })
                .collect::<Vec<_>>();
            (!content.is_empty()).then(|| InputMessage {
                role: role.to_string(),
                content,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// OpenAI-compatible dialect
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PendingChatToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

async fn drive_openai_stream(
    client: &OpenAiClient,
    request: ChatRequest,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(), ApiError> {
    let mut stream = client.stream_chat(&request).await?;
    let mut tool_calls: BTreeMap<usize, PendingChatToolCall> = BTreeMap::new();
    let mut saw_content = false;

    while let Some(chunk) = stream.next_chunk().await? {
        if let Some(usage) = &chunk.usage {
            let _ = tx
                .send(AgentEvent::Usage(TokenUsage {
                    input_tokens: usage.prompt_tokens,
                    output_tokens: usage.completion_tokens,
                    // Native dialect reports cache hits; nothing writes caches.
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: usage.prompt_cache_hit_tokens.unwrap_or(0),
                }))
                .await;
        }
        for choice in &chunk.choices {
            let delta = &choice.delta;
            if let Some(reasoning) = &delta.reasoning_content {
                if !reasoning.is_empty() {
                    let _ = tx.send(AgentEvent::ThinkingDelta(reasoning.clone())).await;
                }
            }
            if let Some(text) = &delta.content {
                if !text.is_empty() {
                    saw_content = true;
                    let _ = tx.send(AgentEvent::TextDelta(text.clone())).await;
                }
            }
            for call in delta.tool_calls.iter().flatten() {
                let pending = tool_calls.entry(call.index).or_default();
                if let Some(id) = &call.id {
                    pending.id = Some(id.clone());
                }
                if let Some(function) = &call.function {
                    if let Some(name) = &function.name {
                        pending.name.clone_from(name);
                    }
                    if let Some(arguments) = &function.arguments {
                        pending.arguments.push_str(arguments);
                    }
                }
            }
        }
    }

    for (index, call) in tool_calls {
        saw_content = true;
        let _ = tx
            .send(AgentEvent::ToolUse {
                id: call.id.unwrap_or_else(|| format!("openai_tool_{index}")),
                name: call.name,
                input: call.arguments,
            })
            .await;
    }

    if saw_content {
        let _ = tx.send(AgentEvent::MessageStop).await;
        return Ok(());
    }
    // Nothing usable arrived on the stream; retry once without streaming.
    debug!("openai stream delivered no content; falling back to non-streaming request");
    let response = client.send_chat(&request).await?;
    emit_chat_response(response, tx).await;
    Ok(())
}

async fn emit_chat_response(response: ChatResponse, tx: &mpsc::Sender<AgentEvent>) {
    for choice in response.choices {
        if let Some(content) = &choice.message.content {
            let text = content.text();
            if !text.is_empty() {
                let _ = tx.send(AgentEvent::TextDelta(text)).await;
            }
        }
        for call in choice.message.tool_calls.into_iter().flatten() {
            let _ = tx
                .send(AgentEvent::ToolUse {
                    id: call.id.unwrap_or_else(|| "openai_tool_0".to_string()),
                    name: call.function.name,
                    input: call.function.arguments,
                })
                .await;
        }
    }
    if let Some(usage) = response.usage {
        let _ = tx
            .send(AgentEvent::Usage(TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: usage.prompt_cache_hit_tokens.unwrap_or(0),
            }))
            .await;
    }
    let _ = tx.send(AgentEvent::MessageStop).await;
}

fn build_chat_request(
    request: &ApiRequest,
    model: &str,
    max_tokens: u32,
    enable_tools: bool,
    reasoning_effort: Option<String>,
) -> ChatRequest {
    let mut messages = Vec::new();
    if !request.system_prompt.is_empty() {
        messages.push(ChatMessage {
            role: ChatRole::System,
            content: Some(ChatContent::Text(request.system_prompt.join("\n\n"))),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    messages.extend(convert_chat_messages(&request.messages));

    ChatRequest {
        model: model.to_string(),
        messages,
        max_tokens: Some(max_tokens),
        stream: true,
        tools: enable_tools.then(|| {
            request
                .tools
                .iter()
                .map(|spec: &ToolSpec| ChatTool {
                    tool_type: ChatToolCallType::Function,
                    function: ChatToolSpec {
                        name: spec.name.clone(),
                        description: Some(spec.description.clone()),
                        parameters: spec.input_schema.clone(),
                    },
                })
                .collect()
        }),
        tool_choice: enable_tools.then_some(ChatToolChoice::Auto),
        // `stream_chat` injects include_usage; keep the field out here.
        stream_options: None,
        thinking: reasoning_effort
            .as_ref()
            .map(|_| ThinkingControl::enabled()),
        reasoning_effort,
    }
}

fn convert_chat_messages(messages: &[ConversationMessage]) -> Vec<ChatMessage> {
    let mut converted = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::System | MessageRole::User => {
                if let Some(content) = chat_content(&message.blocks) {
                    converted.push(ChatMessage {
                        role: ChatRole::User,
                        content: Some(content),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            MessageRole::Assistant => {
                let text = text_of_blocks(&message.blocks).join("");
                let tool_calls = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, name, input } => Some(ChatToolCall {
                            id: Some(id.clone()),
                            call_type: ChatToolCallType::Function,
                            function: ChatFunctionCall {
                                name: name.clone(),
                                arguments: input.clone(),
                            },
                        }),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !text.is_empty() || !tool_calls.is_empty() {
                    converted.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: (!text.is_empty()).then_some(ChatContent::Text(text)),
                        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                        tool_call_id: None,
                    });
                }
            }
            MessageRole::Tool => {
                for block in &message.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        is_error,
                        ..
                    } = block
                    {
                        let content = if *is_error {
                            format!("tool error: {output}")
                        } else {
                            output.clone()
                        };
                        converted.push(ChatMessage {
                            role: ChatRole::Tool,
                            content: Some(ChatContent::Text(content)),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                }
            }
        }
    }
    converted
}

fn text_of_blocks(blocks: &[ContentBlock]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Inline base64 attachment as a data URL, the shape both `OpenAI` dialects
/// accept in place of a remote image URL.
fn image_data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

/// Content for one user message: a bare string, or ordered parts once the
/// message carries attachments.
fn chat_content(blocks: &[ContentBlock]) -> Option<ChatContent> {
    if !blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }))
    {
        let text = text_of_blocks(blocks).join("");
        return (!text.is_empty()).then_some(ChatContent::Text(text));
    }
    let parts = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                Some(ChatContentPart::Text { text: text.clone() })
            }
            ContentBlock::Image { media_type, data } => Some(ChatContentPart::ImageUrl {
                image_url: ChatImageUrl {
                    url: image_data_url(media_type, data),
                    detail: None,
                },
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then_some(ChatContent::Parts(parts))
}

// ---------------------------------------------------------------------------
// OpenAI Responses (`/v1/responses`) dialect
// ---------------------------------------------------------------------------

/// `OpenAI` `Responses` transport bridge.
pub struct ResponsesStreamClient {
    client: ResponsesClient,
    model: String,
    max_tokens: u32,
    enable_tools: bool,
}

impl ResponsesStreamClient {
    #[must_use]
    pub fn from_profile(profile: &ProviderProfile, enable_tools: bool) -> Self {
        let client =
            ResponsesClient::new(profile.api_key.clone()).with_base_url(profile.base_url.clone());
        Self {
            client,
            model: profile.model.clone(),
            max_tokens: profile.max_tokens,
            enable_tools,
        }
    }
}

impl ApiClient for ResponsesStreamClient {
    fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = self.client.clone();
        let body = build_responses_body(&request, &self.model, self.max_tokens, self.enable_tools);
        let handle = tokio::spawn(async move {
            if let Err(error) = drive_responses_stream(&client, body, &tx).await {
                let _ = tx.send(AgentEvent::Error(error.to_string())).await;
            }
        });
        Ok(TurnStream::new(rx, handle.abort_handle()))
    }
}

/// Assemble a `Responses` request body from the runtime request.
fn build_responses_body(
    request: &ApiRequest,
    model: &str,
    max_tokens: u32,
    enable_tools: bool,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "input": responses_input_items(&request.messages),
        "max_output_tokens": max_tokens,
        "stream": true,
    });
    if !request.system_prompt.is_empty() {
        body["instructions"] = serde_json::Value::String(request.system_prompt.join("\n\n"));
    }
    if enable_tools {
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|spec: &ToolSpec| {
                serde_json::json!({
                    "type": "function",
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": spec.input_schema,
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
        body["tool_choice"] = serde_json::Value::String("auto".to_string());
    }
    body
}

/// Content parts for one `Responses` input message. Attachments are only legal
/// on user turns, so a system-sourced message keeps its text alone.
fn responses_content_parts(blocks: &[ContentBlock], allow_images: bool) -> Vec<serde_json::Value> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                Some(serde_json::json!({ "type": "input_text", "text": text }))
            }
            ContentBlock::Image { media_type, data } if allow_images => Some(serde_json::json!({
                "type": "input_image",
                "image_url": image_data_url(media_type, data),
            })),
            _ => None,
        })
        .collect()
}

/// Convert runtime history into `Responses` input items (messages, function
/// calls, and function-call outputs).
fn responses_input_items(messages: &[ConversationMessage]) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::System | MessageRole::User => {
                let content =
                    responses_content_parts(&message.blocks, message.role == MessageRole::User);
                if !content.is_empty() {
                    items.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            MessageRole::Assistant => {
                let text = text_of_blocks(&message.blocks).join("");
                if !text.is_empty() {
                    items.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for block in &message.blocks {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        items.push(serde_json::json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": input,
                        }));
                    }
                }
            }
            MessageRole::Tool => {
                for block in &message.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        is_error,
                        ..
                    } = block
                    {
                        let text = if *is_error {
                            format!("tool error: {output}")
                        } else {
                            output.clone()
                        };
                        items.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": text,
                        }));
                    }
                }
            }
        }
    }
    items
}

/// How a `Responses` stream declared the turn over. Exactly one of these
/// arrives (there is no `[DONE]` sentinel in this dialect).
#[derive(Debug, Clone)]
enum ResponsesTerminal {
    Completed,
    Incomplete(String),
    Failed(String),
}

async fn drive_responses_stream(
    client: &ResponsesClient,
    body: serde_json::Value,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(), ApiError> {
    let mut stream = client.stream(&body).await?;
    let mut saw_content = false;
    let mut terminal = None;

    while let Some(event) = stream.next_event().await? {
        let ResponsesEvent {
            kind,
            delta,
            item,
            response,
        } = event;
        if kind == "response.output_text.delta" {
            if let Some(text) = delta.filter(|value| !value.is_empty()) {
                saw_content = true;
                let _ = tx.send(AgentEvent::TextDelta(text)).await;
            }
        } else if kind.contains("reasoning") && kind.rsplit('.').next() == Some("delta") {
            if let Some(thinking) = delta.filter(|value| !value.is_empty()) {
                let _ = tx.send(AgentEvent::ThinkingDelta(thinking)).await;
            }
        } else if kind == "response.output_item.done" {
            if let Some(call) = item.filter(|value| value.kind == "function_call") {
                saw_content = true;
                let _ = tx
                    .send(AgentEvent::ToolUse {
                        id: call.call_id.unwrap_or_else(|| "responses_tool".to_string()),
                        name: call.name.unwrap_or_default(),
                        input: call.arguments.unwrap_or_else(|| "{}".to_string()),
                    })
                    .await;
            }
        } else if matches!(
            kind.as_str(),
            "response.completed" | "response.incomplete" | "response.failed"
        ) {
            // Truncated and failed turns still report usage; record it before
            // deciding how the turn ends.
            if let Some(usage) = response.as_ref().and_then(|payload| payload.usage.as_ref()) {
                let _ = tx.send(AgentEvent::Usage(responses_usage(usage))).await;
            }
            terminal = Some(match kind.as_str() {
                "response.completed" => ResponsesTerminal::Completed,
                "response.failed" => ResponsesTerminal::Failed(response.as_ref().map_or_else(
                    || "no error detail".to_string(),
                    ResponsesPayload::failure_reason,
                )),
                _ => ResponsesTerminal::Incomplete(response.as_ref().map_or_else(
                    || "unknown reason".to_string(),
                    ResponsesPayload::failure_reason,
                )),
            });
        }
    }

    match terminal {
        Some(ResponsesTerminal::Failed(reason)) => {
            let _ = tx
                .send(AgentEvent::Error(format!(
                    "responses request failed: {reason}"
                )))
                .await;
            return Ok(());
        }
        // Nothing usable arrived, so the truncation is the only thing worth
        // reporting; surfacing the partial stream would just fail downstream.
        Some(ResponsesTerminal::Incomplete(reason)) if !saw_content => {
            let _ = tx
                .send(AgentEvent::Error(format!(
                    "responses response ended incomplete: {reason}"
                )))
                .await;
            return Ok(());
        }
        Some(ResponsesTerminal::Incomplete(reason)) => {
            let _ = tx.send(AgentEvent::Truncated(reason)).await;
        }
        Some(ResponsesTerminal::Completed) | None => {}
    }

    if saw_content {
        let _ = tx.send(AgentEvent::MessageStop).await;
        return Ok(());
    }
    debug!("responses stream delivered no content; falling back to non-streaming request");
    let response = client.send(&body).await?;
    emit_responses_response(response, tx).await;
    Ok(())
}

/// Map `Responses` token counts onto the runtime's usage type.
fn responses_usage(usage: &ResponsesUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        // Native dialect reports cache hits only; nothing writes caches.
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cached_tokens),
    }
}

async fn emit_responses_response(response: ResponsesResponse, tx: &mpsc::Sender<AgentEvent>) {
    for item in response.output {
        if item.kind == "function_call" {
            let _ = tx
                .send(AgentEvent::ToolUse {
                    id: item.call_id.unwrap_or_else(|| "responses_tool".to_string()),
                    name: item.name.unwrap_or_default(),
                    input: item.arguments.unwrap_or_else(|| "{}".to_string()),
                })
                .await;
            continue;
        }
        for content in item.content.unwrap_or_default() {
            if let Some(text) = content.text.filter(|value| !value.is_empty()) {
                let _ = tx.send(AgentEvent::TextDelta(text)).await;
            }
        }
    }
    if let Some(usage) = &response.usage {
        let _ = tx.send(AgentEvent::Usage(responses_usage(usage))).await;
    }
    let _ = tx.send(AgentEvent::MessageStop).await;
}

#[cfg(test)]
mod tests {
    use super::{
        anthropic_thinking, append_tool_input, build_chat_request, build_message_request,
        build_responses_body, chat_content, convert_chat_messages, convert_messages,
        responses_input_items,
    };
    use api::{ChatContent, ChatContentPart, ChatRole, ChatToolCallType};
    use runtime::{ApiRequest, ContentBlock, ConversationMessage, MessageRole, ToolSpec};

    #[test]
    fn tool_input_delta_replaces_empty_placeholder() {
        let mut input = "{}".to_string();
        append_tool_input(&mut input, "{\"command\": \"echo ok\"");
        assert_eq!(input, "{\"command\": \"echo ok\"");
        append_tool_input(&mut input, ", \"cwd\": \".\"}");
        assert_eq!(input, "{\"command\": \"echo ok\", \"cwd\": \".\"}");
    }

    #[test]
    fn tool_input_delta_accumulates_chunked_json() {
        let mut input = String::new();
        append_tool_input(&mut input, "{\"command\":");
        append_tool_input(&mut input, " \"pwd\"}");
        assert_eq!(input, "{\"command\": \"pwd\"}");
    }

    #[test]
    fn converts_tool_roundtrip_messages() {
        let messages = vec![
            ConversationMessage::user_text("hello"),
            ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
                input: "{\"command\":\"pwd\"}".to_string(),
            }]),
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    is_error: false,
                }],
                usage: None,
                pinned: false,
            },
        ];

        let converted = convert_messages(&messages);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[2].role, "user");
    }

    #[test]
    fn maps_runtime_tool_specs_and_respects_max_tokens() {
        let request = ApiRequest {
            system_prompt: vec!["sys".to_string()],
            messages: vec![],
            tools: vec![ToolSpec {
                name: "bash".to_string(),
                description: "run shell".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
        };

        let wire = build_message_request(&request, "deepseek-v4-flash", 8192, true, None);
        assert_eq!(wire.max_tokens, 8192);
        assert_eq!(wire.model, "deepseek-v4-flash");
        let tools = wire.tools.expect("tools should be advertised");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "bash");
        assert_eq!(tools[0].description.as_deref(), Some("run shell"));

        let bare = build_message_request(&request, "m", 4096, false, None);
        assert!(bare.tools.is_none());
        assert!(bare.tool_choice.is_none());
    }

    #[test]
    fn reasoning_effort_maps_to_anthropic_thinking() {
        use api::{Thinking, ThinkingKind};
        // Unset or blank effort leaves thinking off: no default behavior change.
        assert!(anthropic_thinking(None, 8192).is_none());
        assert!(anthropic_thinking(Some("  "), 8192).is_none());
        // Budget is clamped strictly below max_tokens.
        let high = anthropic_thinking(Some("high"), 3000).expect("high should think");
        assert_eq!(high.kind, ThinkingKind::Enabled);
        assert!(high.budget_tokens.expect("budget") < 3000);
        // max_tokens too small to fit a legal budget: skip thinking.
        assert!(anthropic_thinking(Some("high"), 1500).is_none());
        // build_message_request threads it into the wire request and serializes.
        let request = ApiRequest {
            system_prompt: vec![],
            messages: vec![],
            tools: vec![],
        };
        let wire = build_message_request(&request, "m", 8192, false, Some("medium"));
        assert_eq!(wire.thinking, Some(Thinking::enabled(4096)));
        let json = serde_json::to_value(&wire).expect("serialize");
        assert_eq!(json["thinking"]["type"], "enabled");
        assert_eq!(json["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn builds_openai_chat_request_with_system_and_tools() {
        let request = ApiRequest {
            system_prompt: vec!["be brief".to_string()],
            messages: vec![ConversationMessage::user_text("hello")],
            tools: vec![ToolSpec {
                name: "bash".to_string(),
                description: "run shell".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
        };

        let wire = build_chat_request(&request, "deepseek-v4-flash", 8192, true, None);
        assert_eq!(wire.messages.len(), 2);
        assert_eq!(wire.messages[0].role, ChatRole::System);
        assert_eq!(
            wire.messages[0].content.as_ref().map(ChatContent::text),
            Some("be brief".to_string())
        );
        assert_eq!(wire.messages[1].role, ChatRole::User);
        assert_eq!(wire.max_tokens, Some(8192));
        assert_eq!(wire.tools.as_ref().expect("tools").len(), 1);
        assert!(wire.stream);
        assert!(wire.stream_options.is_none());
    }

    #[test]
    fn converts_openai_tool_roundtrip_and_error_markers() {
        let messages = vec![
            ConversationMessage::assistant(vec![
                ContentBlock::Text {
                    text: "running".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: "{\"command\":\"pwd\"}".to_string(),
                },
            ]),
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        tool_name: "bash".to_string(),
                        output: "ok".to_string(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_2".to_string(),
                        tool_name: "bash".to_string(),
                        output: "boom".to_string(),
                        is_error: true,
                    },
                ],
                usage: None,
                pinned: false,
            },
        ];

        let converted = convert_chat_messages(&messages);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, ChatRole::Assistant);
        let calls = converted[0].tool_calls.as_ref().expect("tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_type, ChatToolCallType::Function);
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(converted[1].role, ChatRole::Tool);
        assert_eq!(converted[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            converted[1].content.as_ref().map(ChatContent::text),
            Some("ok".to_string())
        );
        assert_eq!(
            converted[2].content.as_ref().map(ChatContent::text),
            Some("tool error: boom".to_string())
        );
    }

    #[test]
    fn builds_responses_body_with_instructions_tools_and_stream() {
        let request = ApiRequest {
            system_prompt: vec!["be terse".to_string()],
            messages: vec![ConversationMessage::user_text("hi")],
            tools: vec![ToolSpec {
                name: "bash".to_string(),
                description: "run shell".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
        };
        let body = build_responses_body(&request, "deepseek-reasoner", 4096, true);
        assert_eq!(body["model"], "deepseek-reasoner");
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["max_output_tokens"], 4096);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["type"], "function");
        // Without tools, no tools/tool_choice keys are emitted.
        let bare = build_responses_body(&request, "m", 1024, false);
        assert!(bare.get("tools").is_none());
        assert!(bare.get("tool_choice").is_none());
    }

    #[test]
    fn responses_input_items_map_roles_tool_calls_and_outputs() {
        let messages = vec![
            ConversationMessage::user_text("do it"),
            ConversationMessage::assistant(vec![
                ContentBlock::Text {
                    text: "running".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: "{\"command\":\"pwd\"}".to_string(),
                },
            ]),
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    is_error: false,
                }],
                usage: None,
                pinned: false,
            },
        ];
        let items = responses_input_items(&messages);
        // user message, assistant message, function_call, function_call_output
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(items[2]["name"], "bash");
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["output"], "ok");
    }

    fn image_message(role: MessageRole) -> ConversationMessage {
        ConversationMessage {
            role,
            blocks: vec![
                ContentBlock::Text {
                    text: "what is this".to_string(),
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "QUJD".to_string(),
                },
            ],
            usage: None,
            pinned: false,
        }
    }

    #[test]
    fn attaches_images_to_every_dialect() {
        // Responses: an input_image part carrying an inline data URL.
        let items = responses_input_items(&[image_message(MessageRole::User)]);
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][1]["type"], "input_image");
        assert_eq!(
            items[0]["content"][1]["image_url"],
            "data:image/png;base64,QUJD"
        );
        // System-sourced history stays text-only: the provider rejects images
        // outside user turns.
        let system = responses_input_items(&[image_message(MessageRole::System)]);
        assert_eq!(system[0]["content"].as_array().expect("parts").len(), 1);

        // Chat dialect: parts appear only once an image is present.
        let converted = convert_chat_messages(&[image_message(MessageRole::User)]);
        let parts = match converted[0].content.as_ref().expect("content") {
            ChatContent::Parts(parts) => parts,
            ChatContent::Text(_) => panic!("expected image parts"),
        };
        assert!(matches!(parts[1], ChatContentPart::ImageUrl { .. }));
        assert!(matches!(
            chat_content(&[ContentBlock::Text {
                text: "plain".to_string()
            }]),
            Some(ChatContent::Text(_))
        ));

        // Anthropic dialect: base64 source block.
        let converted = convert_messages(&[image_message(MessageRole::User)]);
        let json = serde_json::to_value(&converted[0].content[1]).expect("serialize");
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["type"], "base64");
        assert_eq!(json["source"]["media_type"], "image/png");
        assert_eq!(json["source"]["data"], "QUJD");
    }

    #[test]
    fn reports_responses_terminal_reasons() {
        use api::ResponsesPayload;
        let truncated: ResponsesPayload = serde_json::from_value(serde_json::json!({
            "incomplete_details": { "reason": "max_output_tokens" }
        }))
        .expect("payload");
        assert_eq!(truncated.failure_reason(), "max_output_tokens");

        let failed: ResponsesPayload = serde_json::from_value(serde_json::json!({
            "error": { "code": "server_error", "message": "boom" }
        }))
        .expect("payload");
        assert_eq!(failed.failure_reason(), "server_error: boom");
    }
}
