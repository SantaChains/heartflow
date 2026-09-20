//! Performance benchmark for the self-built tool layer. Run with:
//! `cargo run --release -p heartflow-runtime --example bench`
//!
//! Measures the hot paths an agent hits per turn against this repository:
//! `grep_search` (parallel rg-alike), `glob_search`, `search_files` (nucleo
//! fuzzy), and the per-message token estimator. Numbers are medians of
//! repeated runs on a warm cache; absolute values are machine-relative,
//! ratios and shapes are what matter.

use std::time::{Duration, Instant};

use runtime::{
    estimate_tokens_from, glob_search, grep_search, search_files, ConversationMessage,
    GrepSearchInput,
};

fn bench<F: FnMut()>(name: &str, mut f: F) -> Duration {
    // Warm-up once (page cache, lazy statics, allocator pools).
    f();
    let mut samples = Vec::new();
    for _ in 0..RUNS {
        let started = Instant::now();
        f();
        samples.push(started.elapsed());
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    println!("{name:<52} {median:>10.2?}  (median of {RUNS})");
    median
}

const RUNS: usize = 11;

fn main() {
    let cwd = std::env::current_dir().expect("cwd");
    println!(
        "self-built tool benchmark — repo: {}",
        cwd.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    );

    let grep = |pattern: &str, mode: &str, head: Option<usize>| GrepSearchInput {
        pattern: pattern.to_string(),
        path: None,
        glob: None,
        output_mode: Some(mode.to_string()),
        before: None,
        after: None,
        context_short: None,
        context: None,
        line_numbers: None,
        case_insensitive: None,
        file_type: None,
        head_limit: head,
        offset: None,
        multiline: None,
    };

    bench("grep_search: pattern 'fn ' over repo (files mode)", || {
        let output = grep_search(&grep("fn ", "files_with_matches", None)).expect("grep");
        assert!(output.num_files > 0);
    });

    bench("grep_search: regex '(pub|fn).*token' content mode", || {
        grep_search(&grep("(pub|fn).*token", "content", Some(50))).expect("grep");
    });

    bench("glob_search: **/*.rs over repo", || {
        let output = glob_search("**/*.rs", None).expect("glob");
        assert!(output.num_files > 0);
    });

    bench("search_files: fuzzy 'docsearch' (nucleo)", || {
        let output = search_files("docsearch", None, None).expect("search");
        let _ = output.filenames.len();
    });

    // Token estimator: the incremental fast path means every turn pays only
    // the newest message; this measures the per-message cost a full rescan
    // would pay 500 times over.
    let messages: Vec<ConversationMessage> = (0..500)
        .map(|i| ConversationMessage::user_text(format!("message {i}: {MESSAGE_SAMPLE}")))
        .collect();
    let scan = bench("estimate_tokens_from: one message of 500", || {
        let _ = estimate_tokens_from(&messages, messages.len() - 1, 0);
    });
    println!(
        "{:<52} {:>10}  (500-message full rescan ~ {:.1} ms; incremental ~ {:.3} ms/turn)",
        "  extrapolated",
        "",
        scan.as_secs_f64() * 500.0 * 1000.0,
        scan.as_secs_f64() * 1000.0,
    );
}

const MESSAGE_SAMPLE: &str = "The quick brown fox jumps over the lazy dog while the agent tokens estimate runs across the session transcript with mixed case identifiers like searchDocuments and MAX_TOTAL_MATCHES sprinkled through 240 chars of representative prose.";
