//! Hot-path benchmarks (roadmap §9 基准套件, fed by real recorded fixtures —
//! 合成数据会漂，真实形状不会). The estimator is the per-request hot path:
//! it scores every appended message, so its constant factors bound the cost
//! of a turn boundary.
//!
//! Deliberately dependency-free: warmup + fixed iterations + median over
//! `std::hint::black_box`, printed as a stable table. Run with
//! `cargo bench -p heartflow-runtime`. A framework adds a dependency tree
//! for statistics we do not act on yet; when tuning decisions need variance
//! analysis, criterion can replace this harness without touching callers.

use std::hint::black_box;
use std::time::{Duration, Instant};

use runtime::estimate_tokens_from;
use runtime::{ContentBlock, ConversationMessage, MessageRole, Session};

fn fixture_transcript() -> Vec<ConversationMessage> {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../provider/tests/fixtures/text-turn.json"
    ))
    .expect("committed fixture present");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("fixture parses");
    let mut messages = Vec::new();
    for section in value["entries"][0]["request"]["system_prompt"]
        .as_array()
        .expect("system prompt array")
    {
        messages.push(ConversationMessage {
            role: MessageRole::System,
            blocks: vec![ContentBlock::Text {
                text: section.as_str().expect("string section").to_string(),
            }],
            usage: None,
            pinned: false,
        });
    }
    for message in value["entries"][0]["request"]["messages"]
        .as_array()
        .expect("messages array")
    {
        let mut blocks = Vec::new();
        for block in message["blocks"].as_array().expect("blocks") {
            if let Some(text) = block["Text"]["text"].as_str() {
                blocks.push(ContentBlock::Text {
                    text: text.to_string(),
                });
            }
        }
        messages.push(ConversationMessage {
            role: MessageRole::User,
            blocks,
            usage: None,
            pinned: false,
        });
    }
    messages
}

/// Median over `iters` runs after a short warmup; medians shrug off the
/// scheduler noise that would otherwise dominate a single-shot reading.
fn measure<F: FnMut()>(mut f: F, iters: usize) -> Duration {
    for _ in 0..3 {
        f();
    }
    let mut samples: Vec<Duration> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        f();
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    samples[iters / 2]
}

fn main() {
    let transcript = fixture_transcript();
    let session = Session {
        version: 1,
        messages: transcript.clone(),
    };

    // Session-size sanity: a fixture change must not silently shrink the
    // benchmarked input.
    let total = estimate_tokens_from(&session.messages, 0, 0);
    assert!(total > 8_000, "fixture shrank: {total} tokens");

    // Worst case for the per-char branch: megabyte-scale mixed CJK/ASCII,
    // the shape a large tool result takes.
    let ascii_block = "the quick brown fox jumps over the lazy dog ".repeat(12_000);
    let cjk_block = "心流是渐近线上的工程学 ".repeat(6_000);
    let mixed = vec![
        ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::Text { text: ascii_block }],
            usage: None,
            pinned: false,
        },
        ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::Text { text: cjk_block }],
            usage: None,
            pinned: false,
        },
    ];

    let from = transcript.len().saturating_sub(1);
    let base = estimate_tokens_from(&transcript[..from], 0, 0);

    println!("== heartflow hot paths (median of 200 runs) ==");
    let full = measure(
        || {
            let _ = black_box(estimate_tokens_from(black_box(&transcript), 0, 0));
        },
        200,
    );
    println!(
        "estimate/full_real_fixture   {:>10.1} µs   ({total} tokens over {} messages)",
        full.as_nanos() as f64 / 1000.0,
        transcript.len()
    );
    let tail = measure(
        || {
            let _ = black_box(estimate_tokens_from(
                black_box(&transcript),
                from,
                black_box(base),
            ));
        },
        200,
    );
    println!(
        "estimate/amortized_tail      {:>10.1} ns   (one appended message)",
        tail.as_nanos() as f64
    );
    let meg = measure(
        || {
            let _ = black_box(estimate_tokens_from(black_box(&mixed), 0, 0));
        },
        20,
    );
    println!(
        "estimate/megabyte_mixed_cjk  {:>10.2} ms   (~1 MB tool result)",
        meg.as_secs_f64() * 1000.0
    );
}
