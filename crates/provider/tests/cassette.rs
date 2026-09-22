//! Cassette integration tests.
//!
//! The final test is the point of the whole mechanism: a recorded session
//! re-drives the *real* `ConversationRuntime` — tool round-trip included — with
//! no transport constructed, no network, and no credentials.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use provider::{CassetteClient, CassetteMode};
use runtime::{
    AgentEvent, ApiClient, ApiRequest, ContentBlock, ConversationMessage, ConversationRuntime,
    PermissionMode, PermissionPolicy, RuntimeError, Session, StaticToolExecutor,
    SystemPromptBuilder, TokenUsage, ToolError, TurnStream,
};
use tokio_util::sync::CancellationToken;

/// A transport whose output is fixed in advance: one canned event list per
/// `stream` call. Stands in for a real endpoint while recording.
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

/// A plain assistant message: some text, usage, and a normal stop.
fn text_turn(text: &str) -> Vec<AgentEvent> {
    vec![
        AgentEvent::TextDelta(text.to_string()),
        AgentEvent::Usage(TokenUsage::default()),
        AgentEvent::MessageStop,
    ]
}

fn request(text: &str) -> ApiRequest {
    ApiRequest {
        system_prompt: vec!["system".to_string()],
        messages: vec![ConversationMessage::user_blocks(vec![ContentBlock::Text {
            text: text.to_string(),
        }])],
        tools: Vec::new(),
    }
}

fn temp_path(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("hf-cassette-{tag}-{nanos}-{id}.json"))
}

async fn drain(stream: &mut TurnStream) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.recv().await {
        events.push(event);
    }
    events
}

/// Drive one full turn through a real runtime and hand back the resulting
/// session. The tool executor is deterministic, which is what makes a multi-turn
/// replay reproducible.
async fn drive<C: ApiClient>(client: C, prompt: &str) -> Session {
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
    );
    runtime
        .run_turn(
            prompt,
            None,
            &mut |_event: &AgentEvent| {},
            &CancellationToken::new(),
        )
        .await
        .expect("turn should run");
    runtime.into_session()
}

#[tokio::test]
async fn recorded_turn_replays_identically() {
    let path = temp_path("roundtrip");
    let scripted = text_turn("hello from the endpoint");
    let inner = ScriptedClient {
        turns: vec![scripted.clone()],
    };
    let mut recorder = CassetteClient::live(inner, CassetteMode::Record(path.clone()));

    let mut stream = recorder.stream(request("ping")).expect("record streams");
    assert_eq!(drain(&mut stream).await, scripted);

    // The write is ordered before end-of-stream, so reading right here is safe.
    let text = std::fs::read_to_string(&path).expect("cassette should be written");
    assert!(
        text.contains("\"version\": 1"),
        "cassette should carry its format version: {text}"
    );

    let mut replayer = CassetteClient::<ScriptedClient>::replay(path.clone());
    let mut stream = replayer.stream(request("ping")).expect("replay streams");
    assert_eq!(drain(&mut stream).await, scripted);

    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn divergent_request_is_reported_not_ignored() {
    let path = temp_path("mismatch");
    let inner = ScriptedClient {
        turns: vec![text_turn("x")],
    };
    let mut recorder = CassetteClient::live(inner, CassetteMode::Record(path.clone()));
    drain(&mut recorder.stream(request("first")).expect("record")).await;

    let mut replayer = CassetteClient::<ScriptedClient>::replay(path.clone());
    // `.err()` rather than `.expect_err()`: `TurnStream` has no `Debug`, so the
    // success arm cannot be formatted for an assertion message.
    let error = replayer
        .stream(request("second"))
        .err()
        .expect("a different request must not replay silently");
    let message = error.to_string();
    assert!(message.contains("does not match"), "{message}");
    assert!(
        message.contains("message 0 differs"),
        "the report should name the first differing section: {message}"
    );

    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn replaying_more_turns_than_recorded_is_an_error() {
    let path = temp_path("exhaust");
    let inner = ScriptedClient {
        turns: vec![text_turn("only")],
    };
    let mut recorder = CassetteClient::live(inner, CassetteMode::Record(path.clone()));
    drain(&mut recorder.stream(request("a")).expect("record")).await;

    let mut replayer = CassetteClient::<ScriptedClient>::replay(path.clone());
    drain(&mut replayer.stream(request("a")).expect("first turn replays")).await;

    let error = replayer
        .stream(request("a"))
        .err()
        .expect("there is no second entry to serve");
    assert!(error.to_string().contains("no entry for turn 2"), "{error}");

    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn off_mode_forwards_without_recording() {
    let inner = ScriptedClient {
        turns: vec![text_turn("direct")],
    };
    let mut client = CassetteClient::live(inner, CassetteMode::Off);

    let mut stream = client.stream(request("x")).expect("streams");
    assert_eq!(drain(&mut stream).await, text_turn("direct"));
    assert_eq!(client.recorded_len(), 0, "Off must not accumulate entries");
}

#[test]
fn mode_parsing_names_the_offending_value() {
    assert_eq!(
        CassetteMode::parse("record:/tmp/a.json").expect("valid"),
        CassetteMode::Record(PathBuf::from("/tmp/a.json"))
    );
    assert!(CassetteMode::parse("wat:/tmp/a.json")
        .expect_err("bad mode")
        .contains("`record` or `replay`"));
    assert!(CassetteMode::parse("record:")
        .expect_err("empty path")
        .contains("empty path"));
    assert!(CassetteMode::parse("nonsense")
        .expect_err("no separator")
        .contains("record:<path>"));
    // Splitting on the first colon only is what keeps a drive letter intact.
    assert_eq!(
        CassetteMode::parse(r"record:C:\tmp\a.json").expect("valid"),
        CassetteMode::Record(PathBuf::from(r"C:\tmp\a.json"))
    );
}

/// The payoff: a recorded session re-runs through the real conversation loop
/// without any transport existing at all.
#[tokio::test]
async fn a_recorded_session_replays_through_the_real_loop() {
    let path = temp_path("loop");
    let scripted = ScriptedClient {
        turns: vec![
            vec![
                AgentEvent::ToolUse {
                    id: "call_1".to_string(),
                    name: "echo".to_string(),
                    input: r#"{"text":"hi"}"#.to_string(),
                },
                AgentEvent::Usage(TokenUsage::default()),
                AgentEvent::MessageStop,
            ],
            text_turn("all done"),
        ],
    };
    let recorded = drive(
        CassetteClient::live(scripted, CassetteMode::Record(path.clone())),
        "please echo hi",
    )
    .await;

    // Replay constructs no `ScriptedClient`, so nothing can reach the network
    // and no provider key is needed.
    let replayed = drive(
        CassetteClient::<ScriptedClient>::replay(path.clone()),
        "please echo hi",
    )
    .await;

    assert_eq!(
        recorded, replayed,
        "replaying a cassette must reproduce the recorded session exactly"
    );
    // user, assistant(tool_use), tool result, assistant(text)
    assert_eq!(recorded.messages.len(), 4);

    std::fs::remove_file(&path).ok();
}
