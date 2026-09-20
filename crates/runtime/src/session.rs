use std::fmt::{Display, Formatter};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::TokenUsage;

/// Format version of the append-only segment beside a session snapshot. Bumped
/// only when the segment layout itself changes, so a reader can refuse a body it
/// does not understand instead of guessing.
pub const SEGMENT_VERSION: u64 = 1;

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
    /// Inline image attached by the user. `data` is the base64 payload without
    /// a `data:` prefix; the media type travels alongside so every dialect can
    /// rebuild its own envelope.
    Image {
        media_type: String,
        data: String,
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

/// What happened to the append-only segment found beside a snapshot.
///
/// The snapshot is complete on its own, so every variant other than
/// [`Applied`](Self::Applied) still yields a usable session; they differ only in
/// what the reader should say about the tail it could not keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentRead {
    /// No segment file; the snapshot alone is authoritative.
    Absent,
    /// `appended` trailing messages were folded onto the snapshot.
    Applied { appended: usize },
    /// The segment ended mid-write: all complete records were kept, the torn
    /// tail (`dropped_bytes`) was not.
    Truncated {
        appended: usize,
        dropped_bytes: usize,
    },
    /// The segment could not be placed on the snapshot and was ignored whole.
    Rejected { reason: String },
}

impl SegmentRead {
    /// Reader-facing note when the segment was not fully usable, so a caller can
    /// warn without failing the restore. `None` when nothing went wrong.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::Absent | Self::Applied { .. } => None,
            Self::Truncated {
                appended,
                dropped_bytes,
            } => Some(format!(
                "session segment ended mid-write: kept {appended} appended message(s), dropped {dropped_bytes} trailing byte(s)"
            )),
            Self::Rejected { reason } => Some(format!("ignoring session segment: {reason}")),
        }
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

    /// Load a saved session together with the append-only segment beside it.
    ///
    /// The snapshot (`<id>.json`) stays authoritative, self-contained, and
    /// complete; the segment (`<id>.jsonl`) only carries messages appended after
    /// it, one JSON [`ConversationMessage`] per line behind a header naming the
    /// snapshot length it extends. A missing, unreadable, or untrustworthy
    /// segment degrades to the snapshot and is reported in the returned
    /// [`SegmentRead`], so this call succeeds whenever
    /// [`load_from_path`](Self::load_from_path) would.
    pub fn load_with_segment(path: impl AsRef<Path>) -> Result<(Self, SegmentRead), SessionError> {
        let path = path.as_ref();
        let mut session = Self::load_from_path(path)?;
        let text = match fs::read_to_string(path.with_extension("jsonl")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((session, SegmentRead::Absent));
            }
            Err(error) => {
                return Ok((
                    session,
                    SegmentRead::Rejected {
                        reason: format!("unreadable: {error}"),
                    },
                ));
            }
        };
        if text.trim().is_empty() {
            return Ok((session, SegmentRead::Absent));
        }
        let (tail, dropped) = match parse_segment(&text, session.messages.len()) {
            Ok(parsed) => parsed,
            Err(reason) => return Ok((session, SegmentRead::Rejected { reason })),
        };
        let appended = tail.len();
        session.messages.extend(tail);
        let outcome = match dropped {
            Some(dropped_bytes) => SegmentRead::Truncated {
                appended,
                dropped_bytes,
            },
            None => SegmentRead::Applied { appended },
        };
        Ok((session, outcome))
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

/// Decode a segment body into the messages it appends, or an explanation of why
/// it cannot be trusted.
///
/// Layout: a header line `{"base": <snapshot message count>, "version": 1}` then
/// one JSON `ConversationMessage` per line. Position is the whole safety story —
/// record `k` continues the snapshot at index `base + k` — so a segment is only
/// usable while `base` still equals the snapshot length. A mismatch means the
/// snapshot moved under it (a compaction rewrote a shorter transcript, or a newer
/// snapshot already absorbed the tail) and the tail cannot be placed at all, so
/// it is dropped whole rather than risk duplicating or transposing messages.
///
/// The returned `Option` is the byte count of a final chunk left unterminated by
/// an interrupted append: complete records before it are still good, and a torn
/// JSON object can never decode, so that trailing fragment is the only thing
/// lost.
fn parse_segment(
    text: &str,
    snapshot_len: usize,
) -> Result<(Vec<ConversationMessage>, Option<usize>), String> {
    let mut lines = text.split_inclusive('\n');
    let header = lines.next().ok_or("segment is empty")?;
    let header: Value = serde_json::from_str(header.trim())
        .map_err(|error| format!("header line is not JSON ({error})"))?;
    let base = header
        .get("base")
        .and_then(Value::as_u64)
        .ok_or("header has no base")?;
    let version = header
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or(SEGMENT_VERSION);
    if version != SEGMENT_VERSION {
        return Err(format!("unsupported segment version {version}"));
    }
    if base != snapshot_len as u64 {
        return Err(format!(
            "base {base} does not match the snapshot's {snapshot_len} messages"
        ));
    }

    let mut tail = Vec::new();
    for (index, chunk) in lines.enumerate() {
        let terminated = chunk.ends_with('\n');
        let record = chunk.trim_end_matches(['\n', '\r']);
        if record.trim().is_empty() {
            return Err(format!("blank record at position {index}"));
        }
        match serde_json::from_str::<ConversationMessage>(record) {
            Ok(message) => tail.push(message),
            // Only the last unterminated chunk may be a torn append; a record in
            // the middle that cannot decode means the sequence is broken.
            Err(_) if !terminated => return Ok((tail, Some(chunk.len()))),
            Err(error) => return Err(format!("record {index} is not a message ({error})")),
        }
    }
    Ok((tail, None))
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

    /// User message assembled from arbitrary blocks (text plus attachments).
    #[must_use]
    pub fn user_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::User,
            blocks,
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
    use super::{
        ContentBlock, ConversationMessage, MessageRole, SegmentRead, Session, SessionError,
        SEGMENT_VERSION,
    };
    use crate::usage::TokenUsage;
    use std::fs;
    use std::path::{Path, PathBuf};
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

    fn sample_session() -> Session {
        Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("one"),
                ConversationMessage::user_text("two"),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "three".to_string(),
                }]),
                ConversationMessage::tool_result("t1", "bash", "four", false),
            ],
        }
    }

    /// Write `session` as the snapshot at a fresh temp path so a test can drop a
    /// segment beside it.
    fn write_snapshot(session: &Session, tag: &str) -> PathBuf {
        let path = unique_temp_path(tag);
        session.save_to_path(&path).expect("snapshot should save");
        path
    }

    fn segment_path(path: &Path) -> PathBuf {
        path.with_extension("jsonl")
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(segment_path(path));
    }

    /// Build a segment the way the eventual writer will: a header naming the
    /// snapshot length, then one serialized message per line.
    fn write_segment(path: &Path, base: usize, tail: &[ConversationMessage]) {
        let mut body = format!("{{\"base\": {base}, \"version\": {SEGMENT_VERSION}}}\n");
        for message in tail {
            body.push_str(&serde_json::to_string(message).expect("message should serialize"));
            body.push('\n');
        }
        fs::write(segment_path(path), body).expect("segment should write");
    }

    #[test]
    fn loads_snapshot_unchanged_without_segment() {
        let session = sample_session();
        let path = write_snapshot(&session, "no-segment");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        let plain = Session::load_from_path(&path).expect("plain load");
        cleanup(&path);

        assert_eq!(segment, SegmentRead::Absent);
        assert_eq!(segment.warning(), None);
        assert_eq!(loaded, session);
        assert_eq!(loaded, plain);
    }

    #[test]
    fn applies_segment_back_to_the_exact_session() {
        let full = sample_session();
        let mut snapshot = full.clone();
        let tail = snapshot.messages.split_off(2);
        let path = write_snapshot(&snapshot, "segment-roundtrip");
        write_segment(&path, snapshot.messages.len(), &tail);
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);

        assert_eq!(segment, SegmentRead::Applied { appended: 2 });
        assert_eq!(segment.warning(), None);
        assert_eq!(loaded, full);
    }

    #[test]
    fn reads_a_hand_written_segment() {
        let session = Session {
            version: 1,
            messages: vec![ConversationMessage::user_text("one")],
        };
        let path = write_snapshot(&session, "handwritten-segment");
        fs::write(
            segment_path(&path),
            "{\"base\": 1, \"version\": 1}\n\
             {\"role\": \"assistant\", \"blocks\": [{\"type\": \"text\", \"text\": \"two\"}]}\n\
             {\"role\": \"user\", \"blocks\": [{\"type\": \"text\", \"text\": \"three\"}], \"pinned\": true}\n",
        )
        .expect("hand-written segment should write");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);

        assert_eq!(segment, SegmentRead::Applied { appended: 2 });
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[2].role, MessageRole::User);
        assert!(loaded.messages[2].pinned);
    }

    #[test]
    fn drops_torn_segment_tail_with_a_warning() {
        let session = Session {
            version: 1,
            messages: vec![ConversationMessage::user_text("one")],
        };
        let path = write_snapshot(&session, "torn-tail");
        // A crash mid-append leaves the last chunk without its newline, so the
        // JSON object is incomplete; the record before it must survive.
        fs::write(
            segment_path(&path),
            "{\"base\": 1, \"version\": 1}\n\
             {\"role\": \"user\", \"blocks\": [{\"type\": \"text\", \"text\": \"two\"}]}\n\
             {\"role\": \"assistant\", \"blocks\": [{\"type\": \"tex",
        )
        .expect("torn segment should write");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);

        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1], ConversationMessage::user_text("two"));
        assert!(matches!(
            segment,
            SegmentRead::Truncated { appended: 1, .. }
        ));
        assert!(segment
            .warning()
            .is_some_and(|text| text.contains("mid-write")));
    }

    #[test]
    fn rejects_segment_whose_base_moved() {
        let session = sample_session();
        let path = write_snapshot(&session, "base-moved");
        let tail = vec![ConversationMessage::user_text("extra")];

        // Base ahead of the snapshot: a compaction rewrote a shorter transcript,
        // so the tail would land at the wrong index.
        write_segment(&path, session.messages.len() + 1, &tail);
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        assert!(matches!(segment, SegmentRead::Rejected { .. }));
        assert_eq!(loaded, session);

        // Base behind the snapshot: the tail is already inside it, so applying it
        // would duplicate messages.
        write_segment(&path, 1, &tail);
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);
        assert!(matches!(segment, SegmentRead::Rejected { .. }));
        assert!(segment.warning().is_some());
        assert_eq!(loaded, session);
    }

    #[test]
    fn rejects_segment_with_a_broken_interior_record() {
        let session = sample_session();
        let path = write_snapshot(&session, "broken-interior");
        fs::write(
            segment_path(&path),
            "{\"base\": 4, \"version\": 1}\n\
             {\"role\": \"user\", \"blocks\": [{\"type\": \"text\", \"text\": \"five\"}]}\n\
             not-a-message\n\
             {\"role\": \"user\", \"blocks\": [{\"type\": \"text\", \"text\": \"seven\"}]}\n",
        )
        .expect("segment should write");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);

        assert!(matches!(segment, SegmentRead::Rejected { .. }));
        assert_eq!(loaded, session);
    }

    #[test]
    fn rejects_unknown_segment_version() {
        let session = sample_session();
        let path = write_snapshot(&session, "segment-version");
        fs::write(segment_path(&path), "{\"base\": 4, \"version\": 2}\n")
            .expect("segment should write");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);

        assert!(matches!(segment, SegmentRead::Rejected { .. }));
        assert!(segment
            .warning()
            .is_some_and(|text| text.contains("version")));
        assert_eq!(loaded, session);
    }

    #[test]
    fn treats_blank_segment_as_absent() {
        let session = sample_session();
        let path = write_snapshot(&session, "blank-segment");
        fs::write(segment_path(&path), "").expect("segment should write");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);

        assert_eq!(segment, SegmentRead::Absent);
        assert_eq!(loaded, session);
    }
}
