use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

/// Rough cost charged for one attached image while estimating context size.
/// Providers bill by decoded pixels (~1 token per 750 px on a full-resolution
/// tile), so a 1024x1024 attachment lands near this figure.
const IMAGE_TOKEN_ESTIMATE: usize = 1_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    pub preserve_recent_messages: usize,
    pub max_estimated_tokens: usize,
    /// Model context window in tokens. When non-zero, compaction is triggered
    /// as soon as the estimated session size crosses half of this window
    /// (Hermes-style >50% pre-compaction). When zero, the absolute
    /// `max_estimated_tokens` threshold governs, preserving old behavior.
    pub context_window_tokens: usize,
    /// Trailing messages whose tool-result bodies are replayed verbatim to the
    /// provider; older dumps collapse to a stub (see `build_replay_messages`).
    /// A tunable proxy for "how many recent turns stay lossless" (~4 messages
    /// per turn, so 12 keeps roughly the last three turns intact).
    pub replay_verbatim_tail: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            preserve_recent_messages: 4,
            max_estimated_tokens: 10_000,
            context_window_tokens: 0,
            replay_verbatim_tail: 12,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub summary: String,
    pub removed_message_count: usize,
}

#[must_use]
pub fn estimate_session_tokens(session: &Session) -> usize {
    estimate_tokens_from(&session.messages, 0, 0)
}

/// Incremental core of [`estimate_session_tokens`]: `base` is a trusted sum
/// over `messages[..from]`, only the tail is re-scored. Kept beside the
/// per-message estimator so the block rules stay in one place.
#[must_use]
pub fn estimate_tokens_from(messages: &[ConversationMessage], from: usize, base: usize) -> usize {
    base + messages[from..]
        .iter()
        .map(estimate_message_tokens)
        .sum::<usize>()
}

#[must_use]
pub fn should_compact(session: &Session, config: CompactionConfig) -> bool {
    should_compact_with_estimate(
        session.messages.len(),
        estimate_session_tokens(session),
        config,
    )
}

/// [`should_compact`] for callers that already hold an estimate (the
/// amortized-O(1) path): same thresholds, no re-scan of the session.
#[must_use]
pub fn should_compact_with_estimate(
    message_count: usize,
    estimated: usize,
    config: CompactionConfig,
) -> bool {
    if message_count <= config.preserve_recent_messages {
        return false;
    }
    if config.context_window_tokens > 0 {
        estimated >= config.context_window_tokens / 2
    } else {
        estimated >= config.max_estimated_tokens
    }
}

#[must_use]
pub fn format_compact_summary(summary: &str) -> String {
    let without_analysis = strip_tag_block(summary, "analysis");
    let formatted = if let Some(content) = extract_tag_block(&without_analysis, "summary") {
        without_analysis.replace(
            &format!("<summary>{content}</summary>"),
            &format!("Summary:\n{}", content.trim()),
        )
    } else {
        without_analysis
    };

    collapse_blank_lines(&formatted).trim().to_string()
}

#[must_use]
pub fn get_compact_continuation_message(
    summary: &str,
    suppress_follow_up_questions: bool,
    recent_messages_preserved: bool,
) -> String {
    let mut base = format!(
        "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n{}",
        format_compact_summary(summary)
    );

    if recent_messages_preserved {
        base.push_str("\n\nRecent messages are preserved verbatim.");
    }

    if suppress_follow_up_questions {
        base.push_str("\nContinue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, and do not preface with continuation text.");
    }

    base
}

/// Compact the session in place. Ownership over `session.messages` is taken
/// with `mem::take`, so the preserved tail and pinned survivors are *moved*
/// into the compacted transcript instead of cloned - on image-heavy sessions
/// the old clone-everything approach dominated compaction cost entirely.
///
/// The caller gates on the token threshold (`should_compact` or the amortized
/// `should_compact_with_estimate`): re-estimating here would redo the full
/// O(n) scan the runtime's incremental token cache already avoids.
#[must_use]
pub fn compact_session_in_place(
    session: &mut Session,
    config: CompactionConfig,
) -> CompactionResult {
    // Misuse guard: below the preserve window there is nothing to summarize,
    // and "compacting" would still replace the transcript with a stub.
    let message_count = session.messages.len();
    if message_count <= config.preserve_recent_messages {
        return CompactionResult {
            summary: String::new(),
            removed_message_count: 0,
        };
    }

    let keep_from = message_count.saturating_sub(config.preserve_recent_messages);
    let mut messages = std::mem::take(&mut session.messages);
    let preserved = messages.split_off(keep_from);

    // A pinned message inside the would-be-summarized window is never folded
    // into the summary: it survives verbatim, placed after the continuation
    // header and before the recent tail. Only unpinned history is condensed.
    let mut pinned = Vec::new();
    let mut to_summarize = Vec::new();
    for message in messages {
        if message.pinned {
            pinned.push(message);
        } else {
            to_summarize.push(message);
        }
    }

    let summary = summarize_messages(&to_summarize);
    let continuation = get_compact_continuation_message(
        &summary,
        true,
        !preserved.is_empty() || !pinned.is_empty(),
    );

    let mut compacted_messages = vec![ConversationMessage {
        role: MessageRole::System,
        blocks: vec![ContentBlock::Text { text: continuation }],
        usage: None,
        pinned: false,
    }];
    compacted_messages.extend(pinned);
    compacted_messages.extend(preserved);
    session.messages = compacted_messages;

    // Only messages actually folded into the summary count as removed;
    // pinned survivors stay in the transcript.
    CompactionResult {
        summary,
        removed_message_count: to_summarize.len(),
    }
}

/// Recency-weighted summary budgets (in characters per content block). Older
/// turns collapse toward `SUMMARY_MIN_CHARS`; turns nearest the live window
/// keep up to `SUMMARY_MAX_CHARS`, mirroring the progressive-summarization
/// pattern where near-term context is preserved with higher fidelity.
const SUMMARY_MIN_CHARS: usize = 80;
const SUMMARY_MAX_CHARS: usize = 240;

fn summarize_messages(messages: &[ConversationMessage]) -> String {
    let mut lines = vec!["<summary>".to_string(), "Conversation summary:".to_string()];
    let total = messages.len().max(1);
    for (index, message) in messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        // Linear ramp from min (oldest) to max (newest) in integer math: the
        // turn nearest the live window keeps the most detail. `total >= 1`, so
        // the divisor can never be zero and no checked-division guard is needed.
        let budget =
            SUMMARY_MIN_CHARS + (SUMMARY_MAX_CHARS - SUMMARY_MIN_CHARS) * (index + 1) / total;
        let content = message
            .blocks
            .iter()
            .map(|block| summarize_block(block, budget))
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(format!("- {role}: {content}"));
    }
    lines.push("</summary>".to_string());
    lines.join("\n")
}

fn summarize_block(block: &ContentBlock, max_chars: usize) -> String {
    match block {
        ContentBlock::Text { text } => truncate_chars(text, max_chars),
        ContentBlock::ToolUse { name, input, .. } => {
            // Cap the input before joining: the tool's JSON can be megabytes,
            // and a join-then-truncate would materialize all of it first.
            truncate_chars(
                &format!("tool_use {name}({})", truncate_chars(input, max_chars)),
                max_chars,
            )
        }
        ContentBlock::ToolResult {
            tool_name,
            output,
            is_error,
            ..
        } => {
            let header = format!(
                "tool_result {tool_name}: {}",
                if *is_error { "error " } else { "" }
            );
            let output_budget = max_chars.saturating_sub(header.chars().count());
            format!("{header}{}", truncate_chars(output, output_budget))
        }
        ContentBlock::Image { media_type, .. } => format!("image {media_type}"),
    }
}

/// Clamp to `max_chars` code points (CJK-safe) with an ellipsis marker.
///
/// Single pass: iteration stops at `max_chars + 1` characters, so a
/// multi-megabyte tool result costs only `max_chars` characters of scanning,
/// never a full traversal.
#[must_use]
pub fn truncate_chars(content: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.saturating_mul(4));
    for (count, ch) in content.chars().enumerate() {
        if count == max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    // Fewer than max_chars characters: the content fits as-is.
    content.to_string()
}

/// Token estimate that stays honest for CJK. ASCII code points are counted at
/// the usual ~4 characters/token, while every non-ASCII code point counts as a
/// whole token (a Chinese glyph is roughly one token). Byte length would divide
/// a 3-byte UTF-8 glyph by 4 and badly underestimate mixed Chinese text.
#[must_use]
fn estimate_text_tokens(text: &str) -> usize {
    let mut ascii = 0_usize;
    let mut non_ascii = 0_usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii / 4 + non_ascii
}

fn estimate_message_tokens(message: &ConversationMessage) -> usize {
    message
        .blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => estimate_text_tokens(text),
            ContentBlock::ToolUse { name, input, .. } => {
                estimate_text_tokens(name) + estimate_text_tokens(input)
            }
            ContentBlock::ToolResult {
                tool_name, output, ..
            } => estimate_text_tokens(tool_name) + estimate_text_tokens(output),
            // Vision tokens track the decoded bitmap, not the base64 length:
            // charging the payload would overestimate a large image by orders
            // of magnitude and force premature compaction.
            ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
        })
        .sum()
}

fn extract_tag_block(content: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    let start_index = content.find(&start)? + start.len();
    let end_index = content[start_index..].find(&end)? + start_index;
    Some(content[start_index..end_index].to_string())
}

fn strip_tag_block(content: &str, tag: &str) -> String {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    if let (Some(start_index), Some(end_index_rel)) = (content.find(&start), content.find(&end)) {
        let end_index = end_index_rel + end.len();
        let mut stripped = String::new();
        stripped.push_str(&content[..start_index]);
        stripped.push_str(&content[end_index..]);
        stripped
    } else {
        content.to_string()
    }
}

fn collapse_blank_lines(content: &str) -> String {
    let mut result = String::new();
    let mut last_blank = false;
    for line in content.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && last_blank {
            continue;
        }
        result.push_str(line);
        result.push('\n');
        last_blank = is_blank;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        compact_session_in_place, estimate_session_tokens, estimate_text_tokens,
        format_compact_summary, should_compact, CompactionConfig,
    };
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

    #[test]
    fn formats_compact_summary_like_upstream() {
        let summary = "<analysis>scratch</analysis>\n<summary>Kept work</summary>";
        assert_eq!(format_compact_summary(summary), "Summary:\nKept work");
    }

    #[test]
    fn leaves_small_sessions_unchanged() {
        let mut session = Session {
            version: 1,
            messages: vec![ConversationMessage::user_text("hello")],
        };
        let original = session.clone();

        let result = compact_session_in_place(&mut session, CompactionConfig::default());
        assert_eq!(result.removed_message_count, 0);
        assert_eq!(session, original);
        assert_eq!(result.summary, "");
    }

    #[test]
    fn compacts_older_messages_into_a_system_summary() {
        let mut session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("one ".repeat(200)),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "two ".repeat(200),
                }]),
                ConversationMessage::tool_result("1", "bash", "ok ".repeat(200), false),
                ConversationMessage {
                    role: MessageRole::Assistant,
                    blocks: vec![ContentBlock::Text {
                        text: "recent".to_string(),
                    }],
                    usage: None,
                    pinned: false,
                },
            ],
        };
        let estimated_before = estimate_session_tokens(&session);

        let result = compact_session_in_place(
            &mut session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
                ..CompactionConfig::default()
            },
        );
        assert_eq!(result.removed_message_count, 2);

        assert_eq!(session.messages[0].role, MessageRole::System);
        assert!(matches!(
            &session.messages[0].blocks[0],
            ContentBlock::Text { text } if text.contains("Summary:")
        ));
        assert!(should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
                ..CompactionConfig::default()
            }
        ));
        assert!(estimate_session_tokens(&session) < estimated_before);
    }

    #[test]
    fn truncates_long_blocks_in_summary() {
        let summary = super::summarize_block(
            &ContentBlock::Text {
                text: "x".repeat(400),
            },
            160,
        );
        assert!(summary.ends_with('…'));
        assert!(summary.chars().count() <= 161);
    }

    #[test]
    fn pinned_messages_survive_compaction_verbatim() {
        // A pinned early message must not be folded into the summary: it stays
        // in the transcript verbatim and is excluded from the removed count.
        let mut session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("important constraint ".repeat(40))
                    .with_pinned(true),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "filler one ".repeat(120),
                }]),
                ConversationMessage::user_text("filler two ".repeat(120)),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "recent tail".to_string(),
                }]),
                ConversationMessage::user_text("newest".to_string()),
            ],
        };

        let result = compact_session_in_place(
            &mut session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
                ..CompactionConfig::default()
            },
        );

        // keep_from = 3; of the 3 removed messages only the 2 unpinned ones are
        // summarized, so the pinned constraint is not counted as removed.
        assert_eq!(result.removed_message_count, 2);
        let messages = &session.messages;
        // [continuation, pinned constraint, filler(summarized away? no - it is
        // the preserved recent tail set)] -> the pinned text must be present.
        assert!(
            messages
                .iter()
                .any(|m| m.pinned && m.blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("important constraint")))),
            "pinned message must survive verbatim"
        );
        // The continuation summary must not have absorbed the pinned content.
        let summary_header = match &messages[0].blocks[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        };
        assert!(!summary_header.contains("important constraint"));
    }

    #[test]
    fn summary_weights_recent_turns_with_more_detail() {
        // The newest summarized turn gets the larger budget, the oldest the
        // smaller one. Two 400-char turns: the first truncates earlier.
        let messages = vec![
            ConversationMessage::user_text("a".repeat(400)),
            ConversationMessage::user_text("b".repeat(400)),
        ];
        let summary = super::summarize_messages(&messages);
        let a_len = summary.matches('a').count();
        let b_len = summary.matches('b').count();
        assert!(
            b_len > a_len,
            "recent turn should keep more detail: recent={b_len} older={a_len}"
        );
    }

    #[test]
    fn window_half_triggers() {
        // 4 messages, each ~200 tokens => ~800 estimated. A 1600-token window
        // crosses its half (800) and triggers; a 3200-token window does not.
        let session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("x".repeat(800)),
                ConversationMessage::user_text("x".repeat(800)),
                ConversationMessage::user_text("x".repeat(800)),
                ConversationMessage::user_text("x".repeat(800)),
            ],
        };
        assert!(should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                context_window_tokens: 1600,
                ..CompactionConfig::default()
            }
        ));
        assert!(!should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                context_window_tokens: 3200,
                ..CompactionConfig::default()
            }
        ));
    }

    #[test]
    fn window_zero_falls_back_to_absolute() {
        let session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("x".repeat(400)),
                ConversationMessage::user_text("x".repeat(400)),
                ConversationMessage::user_text("x".repeat(400)),
            ],
        };
        // context_window_tokens = 0: the absolute threshold governs even though
        // a huge window would not have triggered.
        assert!(should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 100,
                context_window_tokens: 0,
                ..CompactionConfig::default()
            }
        ));
        assert!(!should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 100_000,
                context_window_tokens: 0,
                ..CompactionConfig::default()
            }
        ));
    }

    #[test]
    fn estimate_counts_cjk_per_code_point() {
        // 100 Chinese glyphs are 300 UTF-8 bytes but ~100 tokens; the old
        // byte/4 estimate would have said 75 and underestimated pressure.
        assert_eq!(estimate_text_tokens(&"字".repeat(100)), 100);
        // Pure ASCII still collapses at the ~4-chars-per-token heuristic.
        assert_eq!(estimate_text_tokens(&"a".repeat(100)), 25);
        // Mixed text adds both contributions.
        assert_eq!(estimate_text_tokens("abcd字"), 1 + 1);
    }
}
