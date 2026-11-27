use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::TokenUsage;

/// Format version of the append-only segment beside a session snapshot. Bumped
/// only when the segment layout itself changes, so a reader can refuse a body it
/// does not understand instead of guessing.
///
/// v1 records a bare [`ConversationMessage`] per line; v2 wraps each one in an
/// envelope carrying the millisecond timestamp it was appended at. The reader
/// accepts both so hand-written v1 segments keep working.
pub const SEGMENT_VERSION: u64 = 2;

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

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Session {
    pub version: u32,
    pub messages: Vec<ConversationMessage>,
    /// Accumulated wall time of the turns this conversation ran, in
    /// milliseconds. Persisted with the snapshot so `hf doctor` can report real
    /// working time rather than file age; absent in older snapshots (defaults
    /// to zero) and skipped when still zero, keeping untouched transcripts
    /// byte-compatible with the previous format.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub runtime_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Format(String),
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
            runtime_ms: 0,
        }
    }

    /// Persist atomically: write a sibling temp file, then rename over the
    /// target so a crash never leaves a truncated session transcript.
    ///
    /// # Errors
    /// Fails with [`SessionError::Format`] when the transcript cannot be
    /// serialized or the path has no file name, and with
    /// [`SessionError::Io`] when the temp file cannot be written or the
    /// rename over the target fails (transient `PermissionDenied` on Windows
    /// is retried three times first). The temp file is removed on failure.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), SessionError> {
        let path = path.as_ref();
        if path.file_name().is_none() {
            return Err(SessionError::Format(
                "session path has no file name".to_string(),
            ));
        }
        // simd-json's SIMD string classification dominates serde_json on the
        // MB-scale transcripts this writes every turn.
        let payload = simd_json::serde::to_string_pretty(self)
            .map_err(|error| SessionError::Format(error.to_string()))?;
        write_atomic(path, payload.as_bytes()).map_err(SessionError::from)
    }

    /// Load a session snapshot saved by [`Self::save_to_path`], skipping a
    /// leading UTF-8 BOM.
    ///
    /// # Errors
    /// Fails with [`SessionError::Io`] when the file cannot be read (missing,
    /// permission, non-UTF-8 bytes), [`SessionError::Json`] when its contents
    /// are not valid JSON, and [`SessionError::Format`] when the structure is
    /// not a valid session (see [`Self::from_json`]).
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
    ///
    /// # Errors
    /// Fails exactly when [`load_from_path`](Self::load_from_path) does (the
    /// snapshot is authoritative); segment problems never error — they come
    /// back as a degraded [`SegmentRead`] instead.
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

    /// Rebuild a session from a parsed JSON value: an object with a `version`
    /// and a `messages` array.
    ///
    /// # Errors
    /// Fails with [`SessionError::Format`] when the value is not an object,
    /// has no/invalid `version`, or lacks a `messages` array, and with
    /// [`SessionError::Json`] when any message entry does not deserialize
    /// into a [`ConversationMessage`].
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
        let runtime_ms = object
            .get("runtime_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Ok(Self {
            version,
            messages,
            runtime_ms,
        })
    }
}

/// Atomically replace `path` with `bytes`: write a sibling temp file, tighten
/// its permissions, then rename over the target (retrying the transient
/// Windows `PermissionDenied` window). Shared by the session snapshot and the
/// compaction archive — both are sensitive JSON surfaces that must never be
/// left truncated by a crash.
///
/// # Errors
/// Fails when the path has no file name, the temp write fails, or the rename
/// fails for a non-transient reason. The temp file is removed on failure.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temp = match path.file_name() {
        Some(name) => path.with_file_name(format!(
            ".{}.tmp-{}",
            name.to_string_lossy(),
            std::process::id()
        )),
        None => {
            return Err(std::io::Error::other("path has no file name"));
        }
    };
    if let Err(error) = fs::write(&temp, bytes) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    // Owner-only before the rename (mode survives it): the transcript and the
    // archive are the most sensitive surfaces hf writes (security.dev.md D9).
    crate::private_fs::harden_file(&temp);
    match rename_with_transient_retry(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Retry a couple of milliseconds-wide `rename` window.
///
/// On Windows a real-time AV scanner can open the freshly written sibling temp
/// file for a few milliseconds after `fs::write` returns, which makes an
/// otherwise-atomic rename fail with `ACCESS_DENIED` outright — the transcript
/// write then dies for a reason no caller can act on. Three attempts over
/// ~100 ms clear that window. Only `PermissionDenied` is retried: any other
/// error kind is a real failure and is returned on the first attempt.
fn rename_with_transient_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    const ATTEMPTS: usize = 3;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

    let mut last: Option<std::io::Error> = None;
    for attempt in 0..ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retryable = error.kind() == std::io::ErrorKind::PermissionDenied;
                last = Some(error);
                if !retryable {
                    break;
                }
                if attempt + 1 < ATTEMPTS {
                    std::thread::sleep(BACKOFF);
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("rename was never attempted")))
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// One v2 segment line: the timestamp the message was appended at, plus the
/// message itself. v1 lines carry the message alone (see [`SEGMENT_VERSION`]).
#[derive(Debug, Deserialize)]
struct SegmentRecord {
    #[allow(dead_code)]
    ts: u64,
    message: ConversationMessage,
}

/// Decode a segment body into the messages it appends, or an explanation of why
/// it cannot be trusted.
///
/// Layout: a header line `{"base": <snapshot message count>, "version": 1|2}`
/// then one record per line — a bare `ConversationMessage` for v1, an envelope
/// `{"ts": <millis>, "message": {...}}` for v2. Position is the whole safety
/// story — record `k` continues the snapshot at index `base + k` — so a segment
/// is only usable while `base` still equals the snapshot length. A mismatch
/// means the snapshot moved under it (a compaction rewrote a shorter transcript,
/// or a newer snapshot already absorbed the tail) and the tail cannot be placed
/// at all, so it is dropped whole rather than risk duplicating or transposing
/// messages.
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
    if version != 1 && version != SEGMENT_VERSION {
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
        let parsed: Result<ConversationMessage, serde_json::Error> = if version == 1 {
            serde_json::from_str::<ConversationMessage>(record)
        } else {
            serde_json::from_str::<SegmentRecord>(record).map(|record| record.message)
        };
        match parsed {
            Ok(message) => tail.push(message),
            // Only the last unterminated chunk may be a torn append; a record in
            // the middle that cannot decode means the sequence is broken.
            Err(_) if !terminated => return Ok((tail, Some(chunk.len()))),
            Err(error) => return Err(format!("record {index} is not a message ({error})")),
        }
    }
    Ok((tail, None))
}

/// Producer of the append-only segment beside a snapshot.
///
/// Opened once per writing session and appended to one message at a time; the
/// header names the snapshot length the segment extends, so a reader can place
/// every record deterministically. A segment whose header does not match the
/// requested base is rebuilt from scratch: the snapshot moved (or the process
/// restarted after folding the old tail into it), and appending behind a stale
/// base would make the reader reject the whole file.
pub struct SegmentWriter {
    file: fs::File,
    base: usize,
    appended: usize,
}

impl SegmentWriter {
    /// Open or create the segment at `path` for a snapshot of `base` messages.
    ///
    /// A file whose header already names `base` is resumed after repairing a
    /// torn tail (a crash mid-append leaves a partial line that would otherwise
    /// make the whole segment unreadable). Anything else — missing, unreadable,
    /// older version, different base — is replaced by a fresh header.
    ///
    /// # Errors
    /// Fails when the parent directory cannot be created or the file cannot be
    /// opened for appending after the header was written.
    pub fn open(path: impl AsRef<Path>, base: usize) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            crate::private_fs::harden_dir(parent);
        }
        let usable = match read_segment_header(&path) {
            Ok((version, header_base)) => version == SEGMENT_VERSION && header_base == base as u64,
            Err(_) => false,
        };
        if usable {
            // A torn tail cannot be repaired by the reader (it rejects the whole
            // segment on a bad interior record), so truncate it before appending.
            if !repair_torn_tail(&path)? {
                write_segment_header(&path, base)?;
            }
        } else {
            // Discarding a non-empty segment is data-bearing: it means the
            // snapshot moved past it (records already folded in) or the file is
            // corrupt. Never silent, so a surprising loss is traceable.
            if path.metadata().is_ok_and(|meta| meta.len() > 0) {
                tracing::warn!(
                    target: "heartflow::session",
                    path = %path.display(),
                    base,
                    "discarding a stale session segment and starting a new one"
                );
            }
            write_segment_header(&path, base)?;
        }
        let file = fs::OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            file,
            base,
            appended: 0,
        })
    }

    /// Append one timestamped message as a single v2 record.
    ///
    /// # Errors
    /// Fails when the record cannot be serialized or the write fails; the
    /// segment is left with its previous complete records only.
    pub fn append(&mut self, ts_ms: u64, message: &ConversationMessage) -> std::io::Result<()> {
        let mut line = serde_json::to_string(&serde_json::json!({
            "ts": ts_ms,
            "message": message,
        }))
        .map_err(std::io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.appended += 1;
        Ok(())
    }

    /// Durably sync the segment to disk (turn boundary).
    ///
    /// # Errors
    /// Fails when the OS rejects the sync.
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.file.sync_data()
    }

    /// Snapshot length this segment extends.
    #[must_use]
    pub fn base(&self) -> usize {
        self.base
    }

    /// Records appended through this writer.
    #[must_use]
    pub fn appended(&self) -> usize {
        self.appended
    }
}

/// Reset a segment so the next turn appends against a snapshot of `base`
/// messages. Called only after the snapshot rename succeeded: the old segment
/// then holds nothing the snapshot does not.
///
/// # Errors
/// Fails when the header cannot be written atomically.
pub fn reset_segment(path: impl AsRef<Path>, base: usize) -> std::io::Result<()> {
    write_segment_header(path.as_ref(), base)
}

fn write_segment_header(path: &Path, base: usize) -> std::io::Result<()> {
    let header = format!("{{\"base\": {base}, \"version\": {SEGMENT_VERSION}}}\n");
    write_atomic(path, header.as_bytes())
}

/// Read `(version, base)` from the header line. The first line is bounded by
/// `read_line`, so a multi-MB body is never loaded just to inspect the header.
fn read_segment_header(path: &Path) -> std::io::Result<(u64, u64)> {
    let file = fs::File::open(path)?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line)?;
    let header: Value = serde_json::from_str(line.trim())
        .map_err(|error| std::io::Error::other(format!("header is not JSON: {error}")))?;
    let base = header
        .get("base")
        .and_then(Value::as_u64)
        .ok_or_else(|| std::io::Error::other("header has no base"))?;
    let version = header.get("version").and_then(Value::as_u64).unwrap_or(1);
    Ok((version, base))
}

/// Truncate a segment whose last record was cut mid-write by a crash, so the
/// next append starts on a clean line. Scans backwards in 64 KiB blocks (the
/// header always ends in `\n`, so the search terminates at the first line at
/// worst). Returns false when the file has no newline at all — the header
/// itself is torn, and the caller rewrites it.
fn repair_torn_tail(path: &Path) -> std::io::Result<bool> {
    const CHUNK: u64 = 64 * 1024;
    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    let mut last = [0u8; 1];
    file.seek(SeekFrom::End(-1))?;
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(true);
    }
    let mut end = len;
    loop {
        let start = end.saturating_sub(CHUNK);
        let size = usize::try_from(end - start).unwrap_or(0);
        if size == 0 {
            return Ok(false);
        }
        let mut buffer = vec![0u8; size];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer)?;
        if let Some(position) = buffer.iter().rposition(|byte| *byte == b'\n') {
            let keep = start + u64::try_from(position + 1).unwrap_or(0);
            file.set_len(keep)?;
            file.sync_data()?;
            return Ok(true);
        }
        if start == 0 {
            return Ok(false);
        }
        end = start;
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
        reset_segment, ContentBlock, ConversationMessage, MessageRole, SegmentRead, SegmentWriter,
        Session, SessionError,
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

    /// Only `PermissionDenied` is retried. A destination directory that does
    /// not exist is `NotFound` — a real error — so it must fail on the first
    /// attempt and leave the source in place for the caller to clean up.
    #[test]
    fn rename_does_not_retry_real_errors() {
        let dir = std::env::temp_dir().join(format!("runtime-rename-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let from = dir.join("from.txt");
        std::fs::write(&from, "payload").expect("write");
        let missing = dir.join("no-such-dir").join("to.txt");

        let error = super::rename_with_transient_retry(&from, &missing).expect_err("must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(from.exists(), "failed rename must not consume the source");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_moves_the_file_on_the_first_attempt() {
        let dir = std::env::temp_dir().join(format!("runtime-rename-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let from = dir.join("from.txt");
        let to = dir.join("to.txt");
        std::fs::write(&from, "payload").expect("write");

        super::rename_with_transient_retry(&from, &to).expect("rename");
        assert!(!from.exists());
        assert_eq!(std::fs::read_to_string(&to).expect("read"), "payload");
        let _ = std::fs::remove_dir_all(&dir);
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
            runtime_ms: 0,
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
            runtime_ms: 0,
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

    /// Build a v1 segment (bare messages per line) so the legacy reader path
    /// stays covered; the writer only produces v2.
    fn write_segment(path: &Path, base: usize, tail: &[ConversationMessage]) {
        let mut body = format!("{{\"base\": {base}, \"version\": 1}}\n");
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
            runtime_ms: 0,
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
            runtime_ms: 0,
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
        fs::write(segment_path(&path), "{\"base\": 4, \"version\": 99}\n")
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
    fn writer_round_trips_timestamped_records_and_reset_extends_the_base() {
        let full = sample_session();
        let mut snapshot = full.clone();
        let tail = snapshot.messages.split_off(2);
        let path = write_snapshot(&snapshot, "writer-roundtrip");
        let mut writer = SegmentWriter::open(segment_path(&path), snapshot.messages.len())
            .expect("writer should open");
        assert_eq!(writer.base(), 2);
        for (index, message) in tail.iter().enumerate() {
            writer
                .append(1_000 + index as u64, message)
                .expect("append should succeed");
        }
        writer.sync().expect("sync should succeed");
        assert_eq!(writer.appended(), 2);
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        assert_eq!(segment, SegmentRead::Applied { appended: 2 });
        assert_eq!(loaded, full);

        // A save moves the full transcript into the snapshot and resets the
        // header to its new length; the segment then carries nothing.
        full.save_to_path(&path).expect("snapshot save");
        reset_segment(segment_path(&path), full.messages.len()).expect("reset should succeed");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        assert_eq!(
            segment,
            SegmentRead::Applied { appended: 0 },
            "reset segment carries no records"
        );
        assert_eq!(loaded.messages.len(), 4);
        cleanup(&path);
    }

    #[test]
    fn writer_repairs_a_torn_tail_before_appending() {
        let session = Session {
            version: 1,
            messages: vec![ConversationMessage::user_text("one")],
            runtime_ms: 0,
        };
        let path = write_snapshot(&session, "writer-torn");
        let mut first = SegmentWriter::open(segment_path(&path), 1).expect("open");
        first
            .append(1, &ConversationMessage::user_text("two"))
            .expect("append");
        first.sync().expect("sync");
        drop(first);
        // Simulate a crash mid-append: a partial v2 record without newline.
        let mut torn = fs::read_to_string(segment_path(&path)).expect("read segment");
        torn.push_str("{\"ts\":2,\"message\":{\"role\":\"user\",\"blocks\":[{\"type\":\"tex");
        fs::write(segment_path(&path), torn).expect("write torn");

        let mut writer = SegmentWriter::open(segment_path(&path), 1).expect("reopen");
        writer
            .append(3, &ConversationMessage::user_text("three"))
            .expect("append after repair");
        writer.sync().expect("sync");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);

        assert_eq!(segment, SegmentRead::Applied { appended: 2 });
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[1], ConversationMessage::user_text("two"));
        assert_eq!(loaded.messages[2], ConversationMessage::user_text("three"));
    }

    #[test]
    fn writer_rebuilds_a_segment_whose_base_moved() {
        let session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("one"),
                ConversationMessage::user_text("two"),
            ],
            runtime_ms: 0,
        };
        let path = write_snapshot(&session, "writer-rebase");
        // Stale segment from before a save: base 0 with one record.
        write_segment(&path, 0, &[ConversationMessage::user_text("stale")]);
        let mut writer = SegmentWriter::open(segment_path(&path), 2).expect("open");
        writer
            .append(7, &ConversationMessage::user_text("fresh"))
            .expect("append");
        writer.sync().expect("sync");
        let (loaded, segment) = Session::load_with_segment(&path).expect("load");
        cleanup(&path);

        assert_eq!(segment, SegmentRead::Applied { appended: 1 });
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[2], ConversationMessage::user_text("fresh"));
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
