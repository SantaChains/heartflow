use std::fmt::{Display, Formatter};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::TokenUsage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        tool_use_id: String,
        tool_name: String,
        output: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// A pinned message is never folded into a compaction summary; it survives
    /// verbatim. Defaults to `false` and is skipped when unset, so transcripts
    /// without pins stay byte-identical to the previous format.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
}

// Serde's `skip_serializing_if` predicate contract requires a `&bool` argument,
// so taking by value is not an option here despite the trivial copy.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Session {
    pub version: u32,
    pub messages: Vec<ConversationMessage>,
}

#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Format(String),
}

impl Display for SessionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::Format(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: 1,
            messages: Vec::new(),
        }
    }

    /// Persist atomically: write a sibling temp file, then rename over the
    /// target so a crash never leaves a truncated session transcript.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), SessionError> {
        let path = path.as_ref();
        let payload = serde_json::to_string_pretty(self)?;
        let temp = match path.file_name() {
            Some(name) => path.with_file_name(format!(
                ".{}.tmp-{}",
                name.to_string_lossy(),
                std::process::id()
            )),
            None => {
                return Err(SessionError::Format(
                    "session path has no file name".to_string(),
                ))
            }
        };
        if let Err(error) = fs::write(&temp, &payload) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        match fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temp);
                Err(error.into())
            }
        }
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let contents = fs::read_to_string(path)?;
        // Windows editors may leave a UTF-8 BOM; the JSON parser would reject it.
        let contents = contents.strip_prefix('\u{FEFF}').unwrap_or(&contents);
        Self::from_json(&serde_json::from_str(contents)?)
    }

    pub fn from_json(value: &Value) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("session must be an object".to_string()))?;
        let version = object
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| SessionError::Format("missing version".to_string()))?;
        let version = u32::try_from(version)
            .map_err(|_| SessionError::Format("version out of range".to_string()))?;
        let messages = object
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| SessionError::Format("missing messages".to_string()))?
            .iter()
            .map(|message| {
                serde_json::from_value::<ConversationMessage>(message.clone())
                    .map_err(SessionError::from)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { version, messages })
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversationMessage {
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text { text: text.into() }],
            usage: None,
            pinned: false,
        }
    }

    #[must_use]
    pub fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::Assistant,
            blocks,
            usage: None,
            pinned: false,
        }
    }

    #[must_use]
    pub fn assistant_with_usage(blocks: Vec<ContentBlock>, usage: Option<TokenUsage>) -> Self {
        Self {
            role: MessageRole::Assistant,
            blocks,
            usage,
            pinned: false,
        }
    }

    #[must_use]
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                tool_name: tool_name.into(),
                output: output.into(),
                is_error,
            }],
            usage: None,
            pinned: false,
        }
    }

    /// Mark this message pinned so compaction keeps it verbatim.
    #[must_use]
    pub fn with_pinned(mut self, pinned: bool) -> Self {
        self.pinned = pinned;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentBlock, ConversationMessage, MessageRole, Session, SessionError};
    use crate::usage::TokenUsage;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn persists_and_restores_session_json() {
        let mut session = Session::new();
        session
            .messages
            .push(ConversationMessage::user_text("hello"));
        session
            .messages
            .push(ConversationMessage::assistant_with_usage(
                vec![
                    ContentBlock::Text {
                        text: "thinking".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "tool-1".to_string(),
                        name: "bash".to_string(),
                        input: "echo hi".to_string(),
                    },
                ],
                Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 4,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 2,
                }),
            ));
        session.messages.push(ConversationMessage::tool_result(
            "tool-1", "bash", "hi", false,
        ));

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("runtime-session-{nanos}.json"));
        session.save_to_path(&path).expect("session should save");
        let restored = Session::load_from_path(&path).expect("session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        assert_eq!(restored, session);
        assert_eq!(restored.messages[2].role, MessageRole::Tool);
        assert_eq!(
            restored.messages[1].usage.expect("usage").total_tokens(),
            17
        );
    }

    #[test]
    fn loads_legacy_session_layout() {
        // Hand-written by the pre-serde session format; readers must keep
        // accepting it unchanged.
        let legacy = r#"{
            "version": 1,
            "messages": [
                {"role": "user", "blocks": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "blocks": [
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": "ls"}
                ], "usage": {
                    "input_tokens": 3, "output_tokens": 1,
                    "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
                }},
                {"role": "tool", "blocks": [{"type": "tool_result", "tool_use_id": "t1",
                 "tool_name": "bash", "output": "out", "is_error": false}]}
            ]
        }"#;
        let session = Session::from_json(&serde_json::from_str(legacy).expect("legacy json"))
            .expect("legacy session should load");
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[2].role, MessageRole::Tool);
        assert_eq!(session.messages[1].usage.expect("usage").input_tokens, 3);
    }

    fn unique_temp_path(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-{tag}-{nanos}.json"))
    }

    #[test]
    fn rejects_malformed_session_objects() {
        let not_object: serde_json::Value = serde_json::from_str("[]").expect("json array");
        assert!(matches!(
            Session::from_json(&not_object),
            Err(SessionError::Format(_))
        ));

        let missing_version: serde_json::Value =
            serde_json::from_str(r#"{"messages": []}"#).expect("json");
        assert!(matches!(
            Session::from_json(&missing_version),
            Err(SessionError::Format(_))
        ));

        let missing_messages: serde_json::Value =
            serde_json::from_str(r#"{"version": 1}"#).expect("json");
        assert!(matches!(
            Session::from_json(&missing_messages),
            Err(SessionError::Format(_))
        ));

        let bad_version: serde_json::Value =
            serde_json::from_str(r#"{"version": "one", "messages": []}"#).expect("json");
        assert!(matches!(
            Session::from_json(&bad_version),
            Err(SessionError::Format(_))
        ));
    }

    #[test]
    fn round_trips_unicode_and_newlines() {
        let session = Session {
            version: 1,
            messages: vec![ConversationMessage::user_text(
                "\u{4e2d}\u{6587} \u{1f680} line1\r\nline2\tend",
            )],
        };
        let path = unique_temp_path("unicode");
        session.save_to_path(&path).expect("save");
        let restored = Session::load_from_path(&path).expect("load");
        let _ = fs::remove_file(&path);
        assert_eq!(restored, session);
        assert_eq!(
            restored.messages[0].blocks[0],
            ContentBlock::Text {
                text: "\u{4e2d}\u{6587} \u{1f680} line1\r\nline2\tend".to_string()
            }
        );
    }

    #[test]
    fn load_strips_utf8_bom() {
        let path = unique_temp_path("bom");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(r#"{"version":1,"messages":[]}"#.as_bytes());
        fs::write(&path, &bytes).expect("write bom json");
        let session = Session::load_from_path(&path).expect("load should tolerate BOM");
        let _ = fs::remove_file(&path);
        assert_eq!(session.version, 1);
        assert!(
            session.messages.is_empty(),
            "BOM-only preamble should not add messages"
        );
    }

    #[test]
    fn omits_usage_field_when_none() {
        let json = serde_json::to_string(&ConversationMessage::user_text("hi")).expect("serialize");
        assert!(
            !json.contains("usage"),
            "none usage must be skipped: {json}"
        );
    }

    #[test]
    fn save_reports_error_for_path_without_file_name() {
        let error = Session::new()
            .save_to_path(std::path::Path::new(""))
            .expect_err("empty path has no file name");
        assert!(matches!(error, SessionError::Format(_)));
    }
}
