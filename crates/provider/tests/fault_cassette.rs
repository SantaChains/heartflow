//! Fault-injection drills (ST-1): recorded cassettes mutated to carry failure
//! shapes, replayed through the real conversation loop. A cassette that can
//! only replay the happy path proves nothing about how the loop dies — these
//! tests cut the stream mid-flight and pin down what must survive: the
//! transcript keeps every delta the user already saw, and the turn fails with
//! the right error class instead of hanging or silently discarding work.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use provider::{CassetteClient, CassetteMode};
use runtime::{
    AgentEvent, ApiClient, ApiRequest, ContentBlock, ConversationRuntime, ErrorKind,
    PermissionMode, PermissionPolicy, RuntimeError, Session, StaticToolExecutor,
    SystemPromptBuilder, TokenUsage, ToolError, TurnStream,
};
use tokio_util::sync::CancellationToken;

struct ScriptedClient {
    turns: Vec<Vec<AgentEvent>>,
}

impl ApiClient for ScriptedClient {
    fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        if self.turns.is_empty() {
            return Err(RuntimeError::new("scripted client has no turn left"));
        }
        Ok(TurnStream::from_events(self.turns.remove(0)))
    }
}

fn temp_path(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("hf-fault-{tag}-{nanos}-{id}.json"))
}

/// Drive one full turn through a real runtime, returning the outcome and the
/// session either way (`expect` would erase the fault shapes under test).
async fn try_drive<C: ApiClient>(
    client: C,
    prompt: &str,
) -> (
    Result<runtime::TurnSummary, RuntimeError>,
    Session,
    Vec<AgentEvent>,
) {
    let executor = StaticToolExecutor::new()
        .register("echo", |input: &str| -> Result<String, ToolError> {
            Ok(format!("echoed {input}"))
        });
    let mut runtime = ConversationRuntime::new(
        Session::new(),
        client,
        executor,
        PermissionPolicy::new(PermissionMode::Allow),
        SystemPromptBuilder::new().build(),
    )
    // Step-level transport retry (ST-2) would consume cassette turns the
    // fixture does not have: these drills pin the S7 partial-block semantics
    // on the *final* failure, so the retry layer is off here.
    .with_transport_retry_policy(0, std::time::Duration::ZERO);
    let mut events = Vec::new();
    let result = runtime
        .run_turn(
            prompt,
            None,
            &mut |event: &AgentEvent| events.push(event.clone()),
            &CancellationToken::new(),
        )
        .await;
    (result, runtime.into_session(), events)
}

/// Overwrite `entries[0].events` with `keep` events: the mutation step of the
/// fault drill, mirroring what an operator does with `jq` on a real cassette.
fn mutate_cassette(path: &PathBuf, keep: usize) {
    let text = std::fs::read_to_string(path).expect("cassette should exist");
    let mut cassette: serde_json::Value =
        serde_json::from_str(&text).expect("cassette should parse");
    let events = cassette["entries"][0]["events"]
        .as_array()
        .expect("events array")
        .clone();
    assert!(events.len() > keep, "mutation must actually cut events");
    cassette["entries"][0]["events"] = serde_json::Value::Array(events[..keep].to_vec());
    std::fs::write(path, serde_json::to_string(&cassette).expect("serialize"))
        .expect("cassette rewrite");
}

fn recorded_turn() -> Vec<AgentEvent> {
    vec![
        AgentEvent::ThinkingDelta("pondering".to_string()),
        AgentEvent::TextDelta("partial answer".to_string()),
        AgentEvent::Usage(TokenUsage::default()),
        AgentEvent::MessageStop,
    ]
}

fn partial_text_of(session: &Session) -> Option<String> {
    session.messages.iter().rev().find_map(|message| {
        if message.role != runtime::MessageRole::Assistant {
            return None;
        }
        message.blocks.iter().find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
    })
}

/// A stream that dies silently (no Usage, no MessageStop) fails the turn as a
/// transport fault and the deltas the user already watched stay in the
/// transcript.
#[tokio::test]
async fn fault_truncated_cassette_preserves_partial_blocks() {
    let path = temp_path("truncate");
    let inner = ScriptedClient {
        turns: vec![recorded_turn()],
    };
    let (recorded, _, _) = try_drive(
        CassetteClient::live(inner, CassetteMode::Record(path.clone())),
        "fault drill",
    )
    .await;
    assert!(recorded.is_ok(), "the recording pass must be healthy");

    // Cut after the thinking + one text delta: Usage and MessageStop gone.
    mutate_cassette(&path, 2);

    let (result, session, _) = try_drive(
        CassetteClient::<ScriptedClient>::replay(path.clone()),
        "fault drill",
    )
    .await;
    let error = result.expect_err("a truncated stream must fail the turn");
    assert_eq!(error.kind(), ErrorKind::Transport, "{error}");
    assert!(
        error
            .to_string()
            .contains("ended without a message stop event"),
        "{error}"
    );
    assert_eq!(
        partial_text_of(&session).as_deref(),
        Some("partial answer"),
        "deltas the user watched must survive in the transcript"
    );

    std::fs::remove_file(&path).ok();
}

/// A mid-stream error event fails the turn with the classified kind, and the
/// partial content streamed before the error lands in the transcript (S7: it
/// used to be discarded with the failed turn).
#[tokio::test]
async fn fault_error_event_cassette_preserves_partial_blocks() {
    let path = temp_path("error-event");
    let inner = ScriptedClient {
        turns: vec![recorded_turn()],
    };
    let (recorded, _, _) = try_drive(
        CassetteClient::live(inner, CassetteMode::Record(path.clone())),
        "fault drill",
    )
    .await;
    assert!(recorded.is_ok(), "the recording pass must be healthy");

    // Keep the first two events, then append a mid-stream error — the shape
    // `spawn_stream_bridge` produces when the upstream transport dies.
    mutate_cassette(&path, 2);
    let text = std::fs::read_to_string(&path).expect("cassette exists");
    let mut cassette: serde_json::Value = serde_json::from_str(&text).expect("cassette parses");
    cassette["entries"][0]["events"].as_array_mut().expect("array").push(
        serde_json::json!({"Error": {"message": "api returned 502: bad gateway", "hint": null}}),
    );
    std::fs::write(&path, serde_json::to_string(&cassette).expect("serialize"))
        .expect("cassette rewrite");

    let (result, session, _) = try_drive(
        CassetteClient::<ScriptedClient>::replay(path.clone()),
        "fault drill",
    )
    .await;
    let error = result.expect_err("a mid-stream error must fail the turn");
    assert_eq!(error.kind(), ErrorKind::Transport, "{error}");
    assert_eq!(
        partial_text_of(&session).as_deref(),
        Some("partial answer"),
        "S7: content streamed before the error must reach the transcript"
    );

    std::fs::remove_file(&path).ok();
}
