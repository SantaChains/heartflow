use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

/// Rough cost charged for one attached image while estimating context size.
/// Providers bill by decoded pixels (~1 token per 750 px on a full-resolution
/// tile), so a 1024x1024 attachment lands near this figure.
const IMAGE_TOKEN_ESTIMATE: usize = 1_500;

/// Observations kept for the affine fit. The static prompt is session-stable,
/// so a short rolling window keeps the fit conditioned while still following a
/// mid-session tool-set change.
const CALIBRATION_WINDOW: usize = 16;
/// Below this many samples a least-squares line is exact rather than fitted —
/// two points always define one, however noisy — so the seeded intercept
/// carries the estimate until a third point makes the fit meaningful.
const MIN_FIT_SAMPLES: usize = 3;
/// Spread in the independent variable required for the slope to be determined
/// at all. Consecutive turns with near-identical prompt sizes carry no
/// information about density, only about the intercept.
const MIN_FIT_VARIANCE: f64 = 10_000.0;
/// Bounds on the learned slope and intercept. Both hurt in both directions: an
/// under-estimate lets the prompt grow past the context window (hard provider
/// failure), an over-estimate discards history early (silent quality loss).
const DENSITY_MIN: f64 = 0.5;
const DENSITY_MAX: f64 = 3.0;
const OVERHEAD_MAX_TOKENS: f64 = 200_000.0;
/// Smallest prompt worth learning from. Below this the sample is dominated by
/// per-message framing rather than content, and the ratio is mostly noise.
const MIN_SAMPLE_TOKENS: usize = 256;

/// Online correction for the character-based token heuristic.
///
/// [`estimate_text_tokens`] assumes ~4 ASCII characters per token and one token
/// per non-ASCII glyph, and it counts only `session.messages`. The real request
/// is larger in two *separately measured* ways, and confusing them was the first
/// version's mistake:
///
/// 1. **Additive.** The provider also sees the system prompt, the tool schemas
///    and per-message framing. On 451 real turns from 40 saved sessions the
///    fitted constant is ~12k tokens — an order of magnitude larger than the
///    content error, and it was simply absent from the estimate.
/// 2. **Multiplicative.** No character heuristic can know how the provider's
///    BPE vocabulary splits a given text, and agent transcripts are JSON- and
///    code-heavy, where 4 chars/token is far too optimistic. The measured slope
///    is ~2.2x.
///
/// So the model is affine — `actual ≈ overhead + density * predicted` — rather
/// than a single scale factor. A pure multiplier has to absorb the fixed
/// overhead into the slope, which then over-corrects as the prompt grows and
/// saturates against its clamp (in measurement, the median session pinned the
/// multiplier to the ceiling on the very first sample).
///
/// Fitted by least squares over a rolling window, seeded from the first
/// residual. Measured against the raw heuristic on the same 451 turns: median
/// error 62.4% and bias -57.5% before, 12.0% and -1.5% after.
///
/// Note the direction of the failure this prevents: with a -57% bias, a gate at
/// `context_window / 2` fires only once the true prompt is already past the
/// whole window. The bias was not conservative slack; it was a real overrun.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenCalibration {
    /// Recent `(predicted content tokens, provider-reported prompt tokens)`.
    samples: Vec<(usize, u32)>,
    /// Tokens per predicted token, and the prompt's fixed additive size.
    density: f64,
    overhead: f64,
    /// Whether any sample has been accepted. Until then the estimator is
    /// bit-for-bit the uncorrected heuristic.
    calibrated: bool,
}

impl Default for TokenCalibration {
    fn default() -> Self {
        Self {
            samples: Vec::new(),
            density: 1.0,
            overhead: 0.0,
            calibrated: false,
        }
    }
}

impl TokenCalibration {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one ground-truth sample in: `predicted` is the estimator's figure
    /// for the prompt just sent, `actual` the provider's reported input size.
    /// Degenerate samples are dropped rather than clamped, so a truncated or
    /// absent usage report cannot move the fit at all.
    pub fn observe(&mut self, predicted: usize, actual: u32) {
        if predicted < MIN_SAMPLE_TOKENS || actual == 0 {
            return;
        }
        if !self.calibrated {
            // One residual cannot separate slope from intercept, and the
            // intercept is the larger term. Assuming the heuristic's slope and
            // attributing the whole gap to the fixed prompt makes this first
            // sample exact, which is a far better prior than 0.0 — and it is
            // what removes the warm-up tail (p90 falls from 84% to 53%).
            self.density = 1.0;
            // Clamped like the fitted intercept: a report far *below* the
            // content estimate is a broken report, not a saving, and a negative
            // intercept would forecast an empty prompt and disable compaction.
            self.overhead = (f64::from(actual) - predicted as f64).clamp(0.0, OVERHEAD_MAX_TOKENS);
            self.calibrated = true;
        }

        self.samples.push((predicted, actual));
        if self.samples.len() > CALIBRATION_WINDOW {
            self.samples.remove(0);
        }
        if self.samples.len() >= MIN_FIT_SAMPLES {
            if let Some((density, overhead)) = least_squares(&self.samples) {
                self.density = density;
                self.overhead = overhead;
            }
        }
    }

    /// Predict the provider's prompt size for `estimate` content tokens.
    /// Identity until the first accepted sample, so an uncalibrated runtime
    /// behaves exactly as it did before.
    #[must_use]
    pub fn apply(&self, estimate: usize) -> usize {
        if !self.calibrated {
            return estimate;
        }
        let calibrated = self.overhead + self.density * estimate as f64;
        if calibrated <= 0.0 {
            return 0;
        }
        // Float-to-int casts saturate in Rust, so a wild fit cannot overflow.
        calibrated.round() as usize
    }

    /// Accepted sample count, learned slope, and learned fixed size. Exposed
    /// for status output and for tests that need to see convergence.
    #[must_use]
    pub fn samples(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn density(&self) -> f64 {
        self.density
    }

    #[must_use]
    pub fn overhead_tokens(&self) -> f64 {
        self.overhead
    }
}

/// Least-squares fit of `actual = overhead + density * predicted` over a window.
/// `None` when the window says nothing about the slope, which leaves the
/// previous fit in place rather than inventing one.
fn least_squares(samples: &[(usize, u32)]) -> Option<(f64, f64)> {
    let count = samples.len() as f64;
    let mean_predicted = samples.iter().map(|(x, _)| *x as f64).sum::<f64>() / count;
    let mean_actual = samples.iter().map(|(_, y)| f64::from(*y)).sum::<f64>() / count;
    let variance = samples
        .iter()
        .map(|(x, _)| (*x as f64 - mean_predicted).powi(2))
        .sum::<f64>()
        / count;
    if variance < MIN_FIT_VARIANCE {
        return None;
    }
    let covariance = samples
        .iter()
        .map(|(x, y)| (*x as f64 - mean_predicted) * (f64::from(*y) - mean_actual))
        .sum::<f64>()
        / count;
    // Slope is clamped before the intercept, so the intercept absorbs the
    // leftover rather than the two fighting each other.
    let density = (covariance / variance).clamp(DENSITY_MIN, DENSITY_MAX);
    let overhead = (mean_actual - density * mean_predicted).clamp(0.0, OVERHEAD_MAX_TOKENS);
    Some((density, overhead))
}

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
    // Vectorized all-ASCII check first: megabyte tool results skip the
    // per-char classification entirely and take the byte-division path.
    if text.is_ascii() {
        return text.len() / 4;
    }
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
        format_compact_summary, should_compact, CompactionConfig, TokenCalibration,
    };
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

    #[test]
    fn uncalibrated_estimate_is_identity() {
        let calibration = TokenCalibration::new();
        assert_eq!(calibration.samples(), 0);
        assert_eq!(calibration.apply(12_345), 12_345);
    }

    #[test]
    fn first_sample_is_attributed_to_the_fixed_prompt() {
        let mut calibration = TokenCalibration::new();
        // 20k reported against a 5k prediction: one residual cannot be split
        // into slope and intercept, so it all lands on the fixed prompt — and
        // the prediction for the prompt just measured becomes exact.
        calibration.observe(5_000, 20_000);
        assert_eq!(calibration.samples(), 1);
        assert_eq!(calibration.density(), 1.0);
        assert_eq!(calibration.overhead_tokens(), 15_000.0);
        assert_eq!(calibration.apply(5_000), 20_000);
    }

    #[test]
    fn a_fitted_window_recovers_slope_and_intercept() {
        let mut calibration = TokenCalibration::new();
        // An exact line: actual = 5_000 + 1.5 * predicted.
        for (predicted, actual) in [(1_000_usize, 6_500_u32), (2_000, 8_000), (4_000, 11_000)] {
            calibration.observe(predicted, actual);
        }
        assert_eq!(calibration.samples(), 3);
        assert!((calibration.density() - 1.5).abs() < 1e-9);
        assert!((calibration.overhead_tokens() - 5_000.0).abs() < 1e-6);
        assert_eq!(calibration.apply(3_000), 9_500);
    }

    #[test]
    fn a_wild_sample_cannot_collapse_the_estimate() {
        let mut calibration = TokenCalibration::new();
        // A provider reporting 1 token for a 10k content estimate is a broken
        // report, not a 10k-token saving. Clamping the seeded intercept at zero
        // degrades this to "heuristic unchanged" instead of forecasting an
        // empty context and switching compaction off for the session.
        calibration.observe(10_000, 1);
        assert_eq!(calibration.overhead_tokens(), 0.0);
        assert_eq!(calibration.apply(10_000), 10_000);
    }

    #[test]
    fn a_wild_slope_is_clamped_by_the_band() {
        let mut calibration = TokenCalibration::new();
        // Actual grows 10x with predicted: an absurd slope.
        for (predicted, actual) in [(1_000_usize, 10_000_u32), (2_000, 20_000), (4_000, 40_000)] {
            calibration.observe(predicted, actual);
        }
        assert_eq!(calibration.density(), 3.0);
        // Intercept absorbs whatever the clamped slope leaves over.
        let expected = 70_000.0 / 3.0 - 3.0 * (7_000.0 / 3.0);
        assert!((calibration.overhead_tokens() - expected).abs() < 0.01);
    }

    #[test]
    fn a_flat_window_leaves_the_previous_fit_alone() {
        let mut calibration = TokenCalibration::new();
        // Three samples whose predicted sizes barely differ carry no slope
        // information at all, so the seeded fit must survive them unchanged
        // rather than be replaced by a two-point line fitted to noise.
        for (predicted, actual) in [
            (10_000_usize, 30_000_u32),
            (10_001, 30_002),
            (10_002, 30_004),
        ] {
            calibration.observe(predicted, actual);
        }
        assert_eq!(calibration.samples(), 3);
        assert_eq!(calibration.density(), 1.0);
        assert_eq!(calibration.overhead_tokens(), 20_000.0);
        assert_eq!(calibration.apply(10_000), 30_000);
    }

    #[test]
    fn degenerate_samples_are_ignored() {
        let mut calibration = TokenCalibration::new();
        // A prompt too small to be informative, and an absent usage report.
        // Clamping either into the fit would poison it from one trivial turn.
        calibration.observe(8, 500);
        calibration.observe(10_000, 0);
        assert_eq!(calibration.samples(), 0);
        assert_eq!(calibration.apply(9_999), 9_999);
    }

    #[test]
    fn the_calibrated_estimate_moves_the_compaction_gate() {
        let messages = (0..40)
            .map(|_| ConversationMessage::user_text("x".repeat(400)))
            .collect::<Vec<_>>();
        let session = Session {
            version: 1,
            messages,
        };
        let raw = estimate_session_tokens(&session);
        let config = CompactionConfig {
            preserve_recent_messages: 1,
            // Just above the raw estimate: uncorrected, no compaction yet.
            max_estimated_tokens: raw + 1_000,
            context_window_tokens: 0,
            replay_verbatim_tail: 12,
        };
        assert!(!should_compact(&session, config));

        // The provider reports the real prompt: the raw content it does not
        // count, plus ~8k of system prompt and tool schemas it never counted.
        let mut calibration = TokenCalibration::new();
        calibration.observe(raw, u32::try_from(raw + 8_000).expect("fits u32"));
        assert_eq!(calibration.apply(raw), raw + 8_000);
        assert!(calibration.apply(raw) >= config.max_estimated_tokens);
    }

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
