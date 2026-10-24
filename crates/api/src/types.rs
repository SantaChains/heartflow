use serde::{Deserialize, Serialize};
use serde_json::Value;

/// User-configurable sampling knobs shared by every request dialect. Each
/// field is optional and omitted from the wire JSON when unset, so a default
/// `SamplingParams` changes nothing a server can observe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SamplingParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<InputMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    // Flatten sits last so existing keys keep their JSON order; an all-None
    // sampling emits no entries at all.
    #[serde(flatten, default)]
    pub sampling: SamplingParams,
}

impl MessageRequest {
    #[must_use]
    pub fn with_streaming(mut self) -> Self {
        self.stream = true;
        self
    }
}

/// Anthropic extended-thinking control (the request-level `thinking` field).
/// When `Some`, the model reasons before answering and the budget counts
/// against `max_tokens`, so callers must keep `budget_tokens < max_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub kind: ThinkingKind,
    #[serde(rename = "budget_tokens", skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

impl Thinking {
    #[must_use]
    pub fn enabled(budget_tokens: u32) -> Self {
        Self {
            kind: ThinkingKind::Enabled,
            budget_tokens: Some(budget_tokens),
        }
    }
}

/// `thinking.type` selector: enable or disable extended thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingKind {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputMessage {
    pub role: String,
    pub content: Vec<InputContentBlock>,
}

impl InputMessage {
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text { text: text.into() }],
        }
    }

    #[must_use]
    pub fn user_tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![InputContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: vec![ToolResultContentBlock::Text {
                    text: content.into(),
                }],
                is_error,
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultContentBlock>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
}

/// Anthropic image envelope; only inline base64 payloads are modeled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub kind: ImageSourceKind,
    pub media_type: String,
    pub data: String,
}

impl ImageSource {
    #[must_use]
    pub fn base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            kind: ImageSourceKind::Base64,
            media_type: media_type.into(),
            data: data.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageSourceKind {
    Base64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContentBlock {
    Text { text: String },
    Json { value: Value },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: Vec<OutputContentBlock>,
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    pub usage: Usage,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl MessageResponse {
    #[must_use]
    pub fn total_tokens(&self) -> u32 {
        self.usage.total_tokens()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    pub output_tokens: u32,
}

impl Usage {
    #[must_use]
    pub const fn total_tokens(&self) -> u32 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStartEvent {
    pub message: MessageResponse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaEvent {
    pub delta: MessageDelta,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDelta {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockStartEvent {
    pub index: u32,
    pub content_block: OutputContentBlock,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockDeltaEvent {
    pub index: u32,
    pub delta: ContentBlockDelta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    /// Thinking-block signature increments (Anthropic-native; ignored here).
    SignatureDelta {
        #[serde(default)]
        signature: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentBlockStopEvent {
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageStopEvent {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart(MessageStartEvent),
    MessageDelta(MessageDeltaEvent),
    ContentBlockStart(ContentBlockStartEvent),
    ContentBlockDelta(ContentBlockDeltaEvent),
    ContentBlockStop(ContentBlockStopEvent),
    MessageStop(MessageStopEvent),
}

#[cfg(test)]
mod tests {
    use super::{InputMessage, MessageRequest, SamplingParams};

    #[test]
    fn sampling_params_serializes_only_set_fields() {
        let sampling = SamplingParams {
            temperature: Some(0.7),
            seed: Some(42),
            ..SamplingParams::default()
        };
        let json = serde_json::to_value(sampling).expect("sampling should serialize");
        // serde_json widens f32 to f64 inside `Value`, so compare via json!();
        // the wire string itself carries the shortest repr (`0.7`).
        assert_eq!(json["temperature"], serde_json::json!(0.7f32));
        assert_eq!(json["seed"], 42);
        for key in ["top_p", "top_k", "frequency_penalty", "presence_penalty"] {
            assert!(
                json.get(key).is_none(),
                "unset {key} must stay off the wire"
            );
        }
    }

    #[test]
    fn message_request_with_default_sampling_omits_every_sampling_key() {
        let request = MessageRequest {
            model: "m".to_string(),
            max_tokens: 16,
            messages: vec![InputMessage::user_text("hi")],
            system: None,
            tools: None,
            tool_choice: None,
            stream: true,
            thinking: None,
            sampling: SamplingParams::default(),
        };
        let value = serde_json::to_value(&request).expect("request should serialize");
        for key in [
            "temperature",
            "top_p",
            "top_k",
            "frequency_penalty",
            "presence_penalty",
            "seed",
        ] {
            assert!(
                value.get(key).is_none(),
                "unset {key} must stay off the wire"
            );
        }

        // Set fields land flattened at the top level, next to model/messages.
        let sampled = MessageRequest {
            sampling: SamplingParams {
                temperature: Some(0.7),
                top_k: Some(40),
                ..SamplingParams::default()
            },
            ..request
        };
        let value = serde_json::to_value(&sampled).expect("request should serialize");
        assert_eq!(value["temperature"], serde_json::json!(0.7f32));
        assert_eq!(value["top_k"], 40);
        assert_eq!(value["model"], "m");
        let wire = serde_json::to_string(&sampled).expect("wire string");
        assert!(
            wire.contains("\"temperature\":0.7,") || wire.contains("\"temperature\":0.7}"),
            "wire must carry a shortest-repr float: {wire}"
        );
    }
}
