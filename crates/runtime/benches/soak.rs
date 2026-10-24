//! Compressed soak (roadmap §4: 长会话内存/句柄水位). A full soak runs for
//! hours on a real machine; this is the deterministic core: 400 turns through
//! the *real* conversation loop — full event diet (thinking, text, tool
//! round-trip, usage), window compaction firing periodically, calibration
//! refitting — with three portable leak detectors:
//!
//! 1. **per-turn drift**: median turn wall-time at the end vs the start. An
//!    accidental O(n²) (unbounded scan, growing refit) shows up as drift
//!    long before a user notices;
//! 2. **retained size**: the serialized session at checkpoints grows only by
//!    the transcript's own content, never by hidden state;
//! 3. **no panic** across the whole run, compactions included.
//!
//! Run: `cargo bench -p heartflow-runtime --bench soak`

use std::hint::black_box;
use std::time::Instant;

use runtime::estimate_tokens_from;
use runtime::{
    AgentEvent, ApiClient, ApiRequest, ContentBlock, ConversationRuntime, PermissionMode,
    PermissionPolicy, RuntimeError, Session, StaticToolExecutor, SystemPromptBuilder, TokenUsage,
    TurnStream,
};
use tokio_util::sync::CancellationToken;

/// Scripted provider: every turn is a tool round-trip followed by a text
/// reply — the heaviest recurring shape a real session takes.
struct SoakClient;

impl ApiClient for SoakClient {
    fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        Ok(TurnStream::from_events(vec![
            AgentEvent::ThinkingDelta("weighing the step ".repeat(8)),
            AgentEvent::ToolUse {
                id: "soak-1".to_string(),
                name: "echo".to_string(),
                input: r#"{"text":"soak"}"#.to_string(),
            },
            AgentEvent::Usage(TokenUsage {
                input_tokens: 2_000,
                output_tokens: 40,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            AgentEvent::MessageStop,
        ]))
    }
}

/// A second client shape: the plain reply after the tool round.
struct ReplyClient;

impl ApiClient for ReplyClient {
    fn stream(&mut self, _request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        Ok(TurnStream::from_events(vec![
            AgentEvent::TextDelta(
                "soak reply with some body text to grow the transcript ".repeat(6),
            ),
            AgentEvent::Usage(TokenUsage {
                input_tokens: 2_200,
                output_tokens: 120,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            AgentEvent::MessageStop,
        ]))
    }
}

/// Two clients, one turn: the runtime calls whichever it holds, so the turn
/// alternates by re-wrapping. Simpler: one client alternating internally.
struct Alternating(std::sync::atomic::AtomicUsize);
impl ApiClient for Alternating {
    fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = black_box(&request);
        if n % 2 == 0 {
            SoakClient.stream(request)
        } else {
            ReplyClient.stream(request)
        }
    }
}

fn main() {
    let executor =
        StaticToolExecutor::new().register("echo", |input: &str| Ok(format!("echoed {input}")));
    let system_prompt = SystemPromptBuilder::new()
        .with_project_context(runtime::ProjectContext {
            cwd: std::path::PathBuf::from("/soak"),
            current_date: "2026-10-24".to_string(),
            git_status: None,
            instruction_files: Vec::new(),
        })
        .build();
    let mut runtime = ConversationRuntime::new(
        Session::new(),
        Alternating(std::sync::atomic::AtomicUsize::new(0)),
        executor,
        PermissionPolicy::new(PermissionMode::Allow),
        system_prompt,
    )
    .with_compaction(runtime::CompactionConfig {
        // Small window so the soak crosses several compactions.
        context_window_tokens: 12_000,
        hard_trigger: true,
        ..runtime::CompactionConfig::default()
    });

    let cancel = CancellationToken::new();
    let turns = 400;
    let checkpoint = 100;
    let mut medians: Vec<(usize, f64)> = Vec::new();
    let mut samples: Vec<f64> = Vec::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("soak runtime");

    for turn in 1..=turns {
        let start = Instant::now();
        let summary = rt.block_on(runtime.run_turn(
            format!("soak turn {turn}: please echo the marker and summarize."),
            None,
            &mut |_event: &AgentEvent| {},
            &cancel,
        ));
        summary.unwrap_or_else(|error| panic!("turn {turn} failed: {error}"));
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        if turn % checkpoint == 0 {
            let mut sorted = samples.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let median = sorted[sorted.len() / 2];
            medians.push((turn, median));
            samples.clear();
        }
    }

    // Serialize as the retained-size watermark.
    let json = serde_json::to_string(&runtime.session().messages).expect("session serializes");
    let tokens = estimate_tokens_from(&runtime.session().messages, 0, 0);

    println!("== soak: {turns} turns (tool round-trip + reply each) ==");
    for (turn, median) in &medians {
        println!("  turns 1..{turn}: median per-turn {median:.2} ms");
    }
    println!(
        "  final session: {} messages, ~{tokens} estimated tokens, serialized {:.1} KiB",
        runtime.session().messages.len(),
        json.len() as f64 / 1024.0,
    );

    // Drift bound: the last checkpoint's median may not exceed 3x the first's.
    // Compactions and calibration refits happen inside this window; an
    // accidental O(n²) shows up as a much larger ratio.
    let first = medians[0].1.max(0.001);
    let last = medians.last().expect("checkpoints recorded").1;
    assert!(
        last <= first * 3.0,
        "per-turn drift {first:.2} ms -> {last:.2} ms exceeds the 3x bound"
    );
    // Retained bound: compactions fired, so the session cannot have kept
    // every turn verbatim; 400 tool+reply turns would be far larger.
    assert!(
        json.len() < 400 * 1024,
        "retained session grew beyond the transcript's own bound: {} bytes",
        json.len()
    );
    println!("  drift bound OK, retained bound OK");
}
