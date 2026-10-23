//! Offline replay of committed, REAL-recorded provider fixtures (roadmap
//! §11.5, the streaming-change iron law's first half).
//!
//! The two fixtures under `tests/fixtures/` were recorded from a live
//! DeepSeek session (`HEARTFLOW_CASSETTE=record:... hf prompt -q ...`) —
//! reasoning deltas, tool round-trip and all. These tests re-drive the *real*
//! `ConversationRuntime` from them with no transport, no network and no
//! credentials, covering the ds4 integration axes:
//!
//! - **model/protocol loading**: the fixture serves adapter-normalized
//!   events, so the recorded stream is exactly what the provider layer
//!   produced for the wire;
//! - **prompt rendering**: `CassetteClient` replays only when the live-built
//!   request is byte-equal to the recorded one, so any drift in system
//!   prompt assembly, replay projection, tool advertisement or message
//!   ordering fails loudly here;
//! - **tool calls**: the loop re-executes the recorded tool call against a
//!   fixture-fed executor that returns the recorded output, so the tool
//!   result in the follow-up request must reproduce exactly;
//! - **session state**: the final session is checked for the pairing
//!   invariants the runtime promises (every tool use answered, no orphans).
//!
//! Re-recording (after an intentional prompt-assembly change, or to refresh
//! the model's behavior): see the record commands in `docs/src/roadmap.md`
//! §11.5 — one `hf prompt` per fixture with `HEARTFLOW_CASSETTE` set.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use provider::{read_entries, CassetteClient, RecordedEntry};
use runtime::{
    AgentEvent, ApiClient, ApiRequest, ContentBlock, ConversationRuntime, MessageRole,
    PermissionMode, PermissionPolicy, RuntimeError, Session, ToolError, ToolExecutor, ToolSpec,
};
use tokio_util::sync::CancellationToken;

/// A transport that can never be reached. `CassetteClient::replay` holds an
/// `Option<C>`; giving it this makes "replay accidentally went live" a hard
/// error instead of a network call.
struct NoTransport;

impl ApiClient for NoTransport {
    fn stream(&mut self, _request: ApiRequest) -> Result<runtime::TurnStream, RuntimeError> {
        Err(RuntimeError::new(
            "fixture replay must not construct a transport",
        ))
    }
}

/// Serves the tool results the fixture recorded, keyed by (tool name, input)
/// — the exact text the real tool produced when the cassette was cut. The
/// executor's realism is not what a fixture protects; the wire/loop is, and
/// a fixed result is what makes replay byte-reproducible across platforms
/// (the recorded `bash` ran under Windows PowerShell; CI replays on Linux).
///
/// Honest limit: a fixture-recorded *error* result cannot be replayed
/// verbatim (the loop formats executor errors itself), so error-round
/// fixtures would need dedicated handling. The committed fixtures contain
/// none.
struct FixtureExecutor {
    results: HashMap<(String, String), String>,
    specs: Vec<ToolSpec>,
}

impl FixtureExecutor {
    /// Scavenge the cassette: tool advertisements from the first request,
    /// inputs from assistant tool-use blocks, outputs from tool messages.
    fn from_entries(entries: &[RecordedEntry]) -> Self {
        // (tool name, input) keys in call order; outputs filled by a second
        // pass over the recorded tool-result messages.
        let mut calls: Vec<((String, String), Option<String>)> = Vec::new();
        for entry in entries {
            for message in &entry.request.messages {
                for block in &message.blocks {
                    if let ContentBlock::ToolUse { name, input, .. } = block {
                        calls.push(((name.clone(), input.clone()), None));
                    }
                }
            }
        }
        for entry in entries {
            for message in &entry.request.messages {
                if message.role != MessageRole::Tool {
                    continue;
                }
                for block in &message.blocks {
                    if let ContentBlock::ToolResult {
                        tool_name,
                        output,
                        is_error: false,
                        ..
                    } = block
                    {
                        // First unanswered call of this tool name — a
                        // fixture round is one call per name.
                        if let Some((_, slot)) = calls
                            .iter_mut()
                            .find(|((name, _), filled)| name == tool_name && filled.is_none())
                        {
                            *slot = Some(output.clone());
                        }
                    }
                }
            }
        }
        Self {
            results: calls
                .into_iter()
                .filter_map(|(key, out)| out.map(|o| (key, o)))
                .collect(),
            specs: entries
                .first()
                .map(|e| e.request.tools.clone())
                .unwrap_or_default(),
        }
    }
}

impl ToolExecutor for FixtureExecutor {
    fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        match self
            .results
            .get(&(tool_name.to_string(), input.to_string()))
        {
            Some(output) if !output.is_empty() => Ok(output.clone()),
            _ => Err(ToolError::new(format!(
                "fixture has no recorded result for `{tool_name}` with input `{input}`"
            ))),
        }
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// The user text of the first recorded request — what to feed `run_turn` so
/// the loop rebuilds the recorded request byte-for-byte.
fn first_user_prompt(entry: &RecordedEntry) -> String {
    entry
        .request
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Re-drive one fixture through the real conversation loop and return the
/// resulting session. Any request drift fails inside the cassette replay
/// with a section-named mismatch report.
async fn replay(path: &Path) -> Session {
    let entries = read_entries(path).expect("fixture is readable and current-format");
    assert!(
        !entries.is_empty(),
        "a fixture with no entries replays nothing"
    );
    let prompt = first_user_prompt(&entries[0]);
    assert!(
        !prompt.trim().is_empty(),
        "fixture must carry a user prompt"
    );

    let mut runtime = ConversationRuntime::new(
        Session::new(),
        CassetteClient::<NoTransport>::replay(path.to_path_buf()),
        FixtureExecutor::from_entries(&entries),
        PermissionPolicy::new(PermissionMode::Allow),
        // The *recorded* system prompt, not a freshly built one: replay
        // compares requests byte-equal, and a live prompt carries today's
        // date and git status.
        entries[0].request.system_prompt.clone(),
    );
    let mut seen = Vec::new();
    runtime
        .run_turn(
            prompt,
            None,
            &mut |event: &AgentEvent| seen.push(event.clone()),
            &CancellationToken::new(),
        )
        .await
        .expect("the recorded session must replay through the real loop");
    runtime.into_session()
}

/// The pairing invariants the runtime promises over any finished session:
/// every tool result answers an earlier tool use, and no tool use is left
/// unanswered.
fn assert_pairing(session: &Session) {
    let mut uses: Vec<String> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for message in &session.messages {
        for block in &message.blocks {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    assert!(!uses.contains(id), "a tool use id may appear exactly once");
                    uses.push(id.clone());
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    assert!(
                        uses.contains(tool_use_id),
                        "tool result {tool_use_id} answers no earlier tool use"
                    );
                    assert!(
                        !results.contains(tool_use_id),
                        "tool use {tool_use_id} was answered twice"
                    );
                    results.push(tool_use_id.clone());
                }
                _ => {}
            }
        }
    }
    for id in &uses {
        assert!(results.contains(id), "tool use {id} has no result");
    }
}

#[tokio::test]
async fn text_fixture_replays_through_the_real_loop() {
    let session = replay(&fixture("text-turn.json")).await;
    // Fresh session + one text turn: exactly [user, assistant].
    assert_eq!(
        session.messages.len(),
        2,
        "user then assistant, nothing else"
    );
    assert_eq!(session.messages[0].role, MessageRole::User);
    assert_eq!(session.messages[1].role, MessageRole::Assistant);
    assert_pairing(&session);
}

#[tokio::test]
async fn tool_fixture_replays_the_round_trip_exactly() {
    let session = replay(&fixture("tool-turn.json")).await;
    // user, assistant(tool_use), tool result, assistant(text).
    assert_eq!(session.messages.len(), 4);
    assert!(matches!(
        session.messages[1].blocks.first(),
        Some(ContentBlock::ToolUse { name, .. }) if name == "bash"
    ));
    assert!(matches!(
        session.messages[2].blocks.first(),
        Some(ContentBlock::ToolResult { .. })
    ));
    assert!(matches!(
        session.messages[3].blocks.first(),
        Some(ContentBlock::Text { .. })
    ));
    assert_pairing(&session);
}

#[test]
fn committed_fixtures_carry_no_secret_values() {
    // Scans for *value shapes*, not env-var names: the system prompt embedded
    // in each fixture legitimately documents variable names like
    // `DEEPSEEK_API_KEY`. A recorded request never carries credentials (keys
    // live in headers the cassette does not capture), so any hit here means
    // a value leaked some other way.
    for name in ["text-turn.json", "tool-turn.json"] {
        let text = std::fs::read_to_string(fixture(name)).expect("fixture present");
        assert!(
            !text.contains("Bearer "),
            "{name}: bearer tokens never belong in a fixture"
        );
        for (index, _) in text.match_indices("sk-") {
            let shaped = text[index..]
                .chars()
                .take(24)
                .filter(|c| c.is_ascii_alphanumeric())
                .count();
            assert!(
                shaped < 20,
                "{name}: an `sk-`-shaped secret value appears in the fixture"
            );
        }
    }
}

#[test]
fn read_entries_reports_missing_and_stale_files_by_path() {
    let missing = read_entries(&fixture("does-not-exist.json"));
    let message = missing
        .expect_err("a missing fixture is an error")
        .to_string();
    assert!(message.contains("does-not-exist.json"), "{message}");

    let stale = std::env::temp_dir().join("hf-cassette-stale-fixture.json");
    std::fs::write(
        &stale,
        r#"{"version":99,"recorded_at_unix_secs":0,"entries":[]}"#,
    )
    .expect("write stale cassette");
    let message = read_entries(&stale)
        .expect_err("an unknown format version is an error")
        .to_string();
    assert!(
        message.contains("format version 99"),
        "the report names the version: {message}"
    );
    let _ = std::fs::remove_file(&stale);
}
