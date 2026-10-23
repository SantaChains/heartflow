use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

// ---------------------------------------------------------------------------
// Phase 0 instrumentation (docs/src/roadmap.md §4 compaction 改造规格)
//
// Pure observation: no behavioral change. Compaction and orphan-repair events
// are recorded as structured tracing events plus durable counters, so the
// Phase 1 evidence gate ("did a repair actually drop content / does the
// preserved tail outgrow the window") can be answered from data instead of
// speculation. The counters live in a small JSON file next to the sessions
// directory (path injected by the CLI assembly layer; without it only the
// in-process tracing events remain).
// ---------------------------------------------------------------------------

/// Which threshold fired. `ContextWindowHalf` is the Hermes-style
/// `window / 2` gate (window configured); `AbsoluteMax` is the fallback
/// `max_estimated_tokens` gate (window unset, `window == 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdKind {
    ContextWindowHalf,
    AbsoluteMax,
}

/// One compaction execution, emitted by `compact_session_in_place`.
///
/// `estimated_calibrated` carries the raw figure when recorded on the
/// execution side: the calibration fit lives in the conversation runtime
/// (provider-reported usage), which the free function cannot see. The
/// raw-vs-calibrated comparison stays on the gate side (`TokenCalibration`
/// observations), so the two numbers are never conflated here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactEvent {
    pub trigger: ThresholdKind,
    pub estimated_raw: usize,
    pub estimated_calibrated: usize,
    /// Configured context window (`0` = absolute-threshold mode).
    pub window: usize,
    /// Messages actually folded into the summary (pinned survivors excluded).
    pub removed: usize,
    /// Extra messages the boundary walkback pulled into the preserved tail so
    /// it never opens on an orphaned `tool_result` (`0` when no walkback).
    pub walkback_extra: usize,
    /// Estimated tokens of the preserved tail (the verbatim part).
    pub tail_tokens: usize,
}

/// Durable compaction counters, merged across processes from the stats file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionStats {
    pub count: u64,
    pub window_half: u64,
    pub absolute: u64,
    pub repair_dropped: u64,
    pub removed_total: u64,
}

static COMPACT_COUNT: AtomicU64 = AtomicU64::new(0);
static COMPACT_WINDOW_HALF: AtomicU64 = AtomicU64::new(0);
static COMPACT_ABSOLUTE: AtomicU64 = AtomicU64::new(0);
static REPAIR_DROPPED: AtomicU64 = AtomicU64::new(0);
static REMOVED_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Injected by the CLI assembly layer; `None` = tracing-only observation.
/// A poisoned lock degrades to "no path" (counters stay in-process), never to
/// a wrong value — same lock-poisoning rule as the rest of the workspace.
static STATS_PATH: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);
static FLUSH_FAILED: AtomicBool = AtomicBool::new(false);

/// Inject the durable stats file path (CLI assembly layer; typically
/// `~/.heartflow/sessions/compaction-stats.json`). Without it, events remain
/// visible through tracing only.
pub fn set_compact_stats_path(path: std::path::PathBuf) {
    if let Ok(mut guard) = STATS_PATH.write() {
        *guard = Some(path);
    }
}

fn stats_path() -> Option<std::path::PathBuf> {
    STATS_PATH.read().ok()?.clone()
}

/// Process-local counters as a snapshot. Relaxed ordering: these are
/// independent gauges with no cross-variable invariant to protect.
fn process_counters() -> CompactionStats {
    CompactionStats {
        count: COMPACT_COUNT.load(Ordering::Relaxed),
        window_half: COMPACT_WINDOW_HALF.load(Ordering::Relaxed),
        absolute: COMPACT_ABSOLUTE.load(Ordering::Relaxed),
        repair_dropped: REPAIR_DROPPED.load(Ordering::Relaxed),
        removed_total: REMOVED_TOTAL.load(Ordering::Relaxed),
    }
}

/// Read the durable file, tolerating absence and corruption (a torn or
/// hand-mangled file degrades to "no history", never to an error).
fn read_stats_file(path: &std::path::Path) -> Option<CompactionStats> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let field = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    Some(CompactionStats {
        count: field("count"),
        window_half: field("window_half"),
        absolute: field("absolute"),
        repair_dropped: field("repair_dropped"),
        removed_total: field("removed_total"),
    })
}

/// Current totals: durable file history plus this process's counters. The two
/// sources are additive by construction — the file only ever holds other
/// processes' deltas, since a process flushes `file + own counters` and never
/// rewrites the file from its counters alone. Concurrent writers are
/// last-one-wins; acceptable for Phase 0 observation.
#[must_use]
pub fn compaction_stats() -> CompactionStats {
    let process = process_counters();
    match stats_path().as_deref().and_then(read_stats_file) {
        Some(file) => CompactionStats {
            count: file.count + process.count,
            window_half: file.window_half + process.window_half,
            absolute: file.absolute + process.absolute,
            repair_dropped: file.repair_dropped + process.repair_dropped,
            removed_total: file.removed_total + process.removed_total,
        },
        None => process,
    }
}

fn write_stats_file(path: &std::path::Path, stats: &CompactionStats) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = format!(
        "{{\"version\":1,\"count\":{},\"window_half\":{},\"absolute\":{},\"repair_dropped\":{},\"removed_total\":{}}}\n",
        stats.count, stats.window_half, stats.absolute, stats.repair_dropped, stats.removed_total
    );
    // Atomic replace: a torn file must never look like valid stats.
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, json)?;
    std::fs::rename(&temp, path)
}

fn flush_stats() {
    let Some(path) = stats_path() else { return };
    let merged = compaction_stats();
    if let Err(error) = write_stats_file(&path, &merged) {
        // Warn once: a failing disk must not spam every turn (repair drops
        // and compactions are rare, but a loop of them is not impossible).
        if !FLUSH_FAILED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "heartflow::compact",
                path = %path.display(),
                %error,
                "could not persist compaction stats; counters stay in-process only"
            );
        }
    }
}

/// Record one compaction execution: structured event + counters + durable
/// flush (when a stats path is injected).
pub fn record_compact_event(event: &CompactEvent) {
    COMPACT_COUNT.fetch_add(1, Ordering::Relaxed);
    match event.trigger {
        ThresholdKind::ContextWindowHalf => {
            COMPACT_WINDOW_HALF.fetch_add(1, Ordering::Relaxed);
        }
        ThresholdKind::AbsoluteMax => {
            COMPACT_ABSOLUTE.fetch_add(1, Ordering::Relaxed);
        }
    }
    REMOVED_TOTAL.fetch_add(event.removed as u64, Ordering::Relaxed);
    tracing::info!(
        target: "heartflow::compact",
        trigger = ?event.trigger,
        estimated_raw = event.estimated_raw,
        estimated_calibrated = event.estimated_calibrated,
        window = event.window,
        removed = event.removed,
        walkback_extra = event.walkback_extra,
        tail_tokens = event.tail_tokens,
        "compaction applied"
    );
    flush_stats();
}

/// Record orphan `tool_result` blocks dropped by `repair_tool_pairing`.
/// Each drop is silent information loss on the wire path, so it is a warning
/// (visible under the default `warn` filter) and a durable counter.
pub fn record_repair_drops(tool_use_ids: &[String]) {
    if tool_use_ids.is_empty() {
        return;
    }
    REPAIR_DROPPED.fetch_add(tool_use_ids.len() as u64, Ordering::Relaxed);
    for id in tool_use_ids {
        tracing::warn!(
            target: "heartflow::compact",
            tool_use_id = %id,
            "repair_tool_pairing dropped an orphan tool_result (no preceding tool_use advertised it)"
        );
    }
    flush_stats();
}

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

/// Window-based soft-trigger policy (D4.1, roadmap §4). Only consulted when
/// `context_window_tokens > 0`; the absolute `max_estimated_tokens` fallback
/// is unaffected. Defaults to the Hermes-style half-window gate, which keeps
/// the shipped behavior byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftTrigger {
    /// `estimated >= window / 2` — the current default. Compacts more often,
    /// further away from a hard failure.
    HalfWindow,
    /// ds4-style late trigger (`misc/COMPACT.md`): `estimated >= 85% × window`
    /// or `remaining <= min(8192, window / 4)`. The free-token floor is capped
    /// at a quarter of small contexts so tiny test windows do not compact
    /// after every short turn. Safe to run late because the calibrated
    /// estimate's median error is ~-1.5% (was -57% before `TokenCalibration`).
    Ds4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    pub preserve_recent_messages: usize,
    pub max_estimated_tokens: usize,
    /// Model context window in tokens. When non-zero, compaction is triggered
    /// by the [`SoftTrigger`] policy once the estimated session size crosses
    /// it. When zero, the absolute `max_estimated_tokens` threshold governs,
    /// preserving old behavior.
    pub context_window_tokens: usize,
    /// Trailing messages whose tool-result bodies are replayed verbatim to the
    /// provider; older dumps collapse to a stub (see `build_replay_messages`).
    /// A tunable proxy for "how many recent turns stay lossless" (~4 messages
    /// per turn, so 12 keeps roughly the last three turns intact).
    pub replay_verbatim_tail: usize,
    /// Soft-trigger policy inside the window regime. Default is the shipped
    /// half-window gate (rollback spec: new behavior is config-gated, defaults
    /// reproduce the old behavior).
    pub soft_trigger: SoftTrigger,
    /// Token ceiling for the preserved tail (D4.3, roadmap §4). The cut starts
    /// at `preserve_recent_messages` and walks forward — summarizing more —
    /// while the tail outgrows this budget, so a few huge tool results can no
    /// longer occupy the whole window by message count alone. `0` disables the
    /// ceiling (current behavior; count-only cut).
    pub tail_budget_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            preserve_recent_messages: 4,
            max_estimated_tokens: 10_000,
            context_window_tokens: 0,
            replay_verbatim_tail: 12,
            soft_trigger: SoftTrigger::HalfWindow,
            tail_budget_tokens: 0,
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
///
/// `estimated` is the *calibrated* figure at the conversation-runtime gate
/// (`AgentRuntime::estimated_tokens`); the ds4 policy's late trigger leans on
/// that calibration (~-1.5% median error), so callers feeding raw estimates
/// should keep `SoftTrigger::HalfWindow` (the default) whose margin absorbs
/// the raw error.
#[must_use]
pub fn should_compact_with_estimate(
    message_count: usize,
    estimated: usize,
    config: CompactionConfig,
) -> bool {
    if message_count <= config.preserve_recent_messages {
        return false;
    }
    if config.context_window_tokens == 0 {
        return estimated >= config.max_estimated_tokens;
    }
    let window = config.context_window_tokens;
    match config.soft_trigger {
        SoftTrigger::HalfWindow => estimated >= window / 2,
        SoftTrigger::Ds4 => {
            // u128 keeps the percent math overflow-free for any usize input
            // (`as` is lossless: usize is at most 64-bit).
            if (estimated as u128) >= (window as u128) * 85 / 100 {
                return true;
            }
            let free_floor = 8192.min(window / 4);
            window.saturating_sub(estimated) <= free_floor
        }
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
    dropped_token_estimate: usize,
) -> String {
    let mut base = format!(
        "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n{}",
        format_compact_summary(summary)
    );

    if recent_messages_preserved {
        base.push_str("\n\nRecent messages are preserved verbatim.");
    }

    // D4.4 (kimi-code compactionHandoff): state how much was omitted so the
    // model knows the middle of the conversation is gone, not merely distant.
    // The figure is the heuristic estimate over the summarized region, so it
    // is explicitly labeled as an estimate — the calibration fit lives in the
    // conversation runtime and is not visible here.
    if dropped_token_estimate > 0 {
        base.push_str(&format!(
            "\n\nRoughly {dropped_token_estimate} estimated tokens of older conversation were dropped here during compaction."
        ));
    }

    if suppress_follow_up_questions {
        base.push_str("\nContinue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, and do not preface with continuation text.");
    }

    base
}

/// Outcome of [`plan_compaction_cut`]: where the preserved tail starts and how
/// many estimated tokens it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPlan {
    pub keep_from: usize,
    pub tail_tokens: usize,
}

/// Pure cut planner (D4.3, roadmap §4) — offline-testable with no session
/// ownership. Count floor: the cut starts at `preserve_recent_messages` from
/// the end. Token ceiling: while the tail outgrows `tail_budget_tokens`, the
/// cut walks *forward* (summarizing more), swallowing an assistant `tool_use`
/// together with its trailing results so the tail never opens on an orphan
/// tool result. Finally the boundary walks backward over trailing tool
/// results, which only ever keeps more messages than asked. The tail is never
/// left empty; a single oversized message is kept and reported honestly via
/// `tail_tokens` (the Phase 0 event makes the overflow observable).
#[must_use]
pub fn plan_compaction_cut(messages: &[ConversationMessage], config: &CompactionConfig) -> CutPlan {
    let count = messages.len();
    let mut keep_from = count.saturating_sub(config.preserve_recent_messages);

    if config.tail_budget_tokens > 0 {
        while keep_from < count
            && estimate_tokens_from(&messages[keep_from..], 0, 0) > config.tail_budget_tokens
        {
            let next = keep_from + 1;
            if next >= count {
                break; // a single-message tail that still overflows: keep it
            }
            keep_from = next;
            // Advancing past an assistant that advertised tool calls would
            // strand its results at the tail head; swallow the results too.
            while keep_from < count && messages[keep_from].role == MessageRole::Tool {
                keep_from += 1;
            }
            if keep_from >= count {
                keep_from = count - 1; // never return an empty tail
                break;
            }
        }
    }

    while keep_from > 0 && messages[keep_from].role == MessageRole::Tool {
        keep_from -= 1;
    }
    CutPlan {
        keep_from,
        tail_tokens: estimate_tokens_from(&messages[keep_from..], 0, 0),
    }
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

    // Phase 0 observation: the raw estimate is re-scored here rather than
    // trusted from the gate. Compaction is a rare O(n) path (one event per
    // many turns), so the extra scan is free relative to the summary build,
    // and it keeps `/compact` — which bypasses the gate entirely — observed.
    let estimated_raw = estimate_session_tokens(session);

    let naive_keep_from = message_count.saturating_sub(config.preserve_recent_messages);
    let mut messages = std::mem::take(&mut session.messages);
    let plan = plan_compaction_cut(&messages, &config);
    let keep_from = plan.keep_from;
    let preserved = messages.split_off(keep_from);
    let tail_tokens = plan.tail_tokens;

    // A pinned message inside the would-be-summarized window is never folded
    // into the summary: it survives verbatim, placed after the continuation
    // header and before the recent tail. Only unpinned history is condensed.
    //
    // A pinned assistant that advertises `tool_use` drags its results along:
    // the results immediately follow the ad, so if the ad survives and they
    // were summarized, the rebuilt transcript would carry a dangling tool_use
    // the provider rejects (Anthropic/OpenAI both 400 on an unanswered call).
    // While tools keep coming after a pinned ad, each one is pinned too.
    let mut pinned = Vec::new();
    let mut to_summarize = Vec::new();
    let mut pin_following = false;
    for message in messages {
        let advertises = message
            .blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
        let pin_this = message.pinned || (pin_following && message.role == MessageRole::Tool);
        pin_following = if message.role == MessageRole::Tool {
            pin_this // keep pulling while further results trail the pinned ad
        } else {
            message.pinned && advertises
        };
        if pin_this {
            pinned.push(message);
        } else {
            to_summarize.push(message);
        }
    }

    let summary = summarize_messages(&to_summarize);
    // D4.4 omission declaration: the estimate covers the summarized region
    // only — pinned survivors stay verbatim and are excluded.
    let pinned_tokens = estimate_tokens_from(&pinned, 0, 0);
    let dropped_token_estimate = estimated_raw.saturating_sub(tail_tokens + pinned_tokens);
    let continuation = get_compact_continuation_message(
        &summary,
        true,
        !preserved.is_empty() || !pinned.is_empty(),
        dropped_token_estimate,
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
    let result = CompactionResult {
        summary,
        removed_message_count: to_summarize.len(),
    };

    record_compact_event(&compact_event_for(
        estimated_raw,
        config,
        naive_keep_from,
        keep_from,
        tail_tokens,
        result.removed_message_count,
    ));

    result
}

/// Build the observation event for one compaction from values captured
/// before the transcript was rebuilt (the session is owned and mutated by
/// then, so nothing can be re-derived at the call site). Split out of
/// `compact_session_in_place` so every field is unit-testable without a
/// tracing capture. `estimated_calibrated` mirrors the raw figure because
/// the calibration fit lives in the conversation runtime and this free
/// function cannot see it — the raw-vs-calibrated comparison stays on the
/// gate side, so the two figures are never conflated.
#[must_use]
fn compact_event_for(
    estimated_raw: usize,
    config: CompactionConfig,
    naive_keep_from: usize,
    keep_from: usize,
    tail_tokens: usize,
    removed: usize,
) -> CompactEvent {
    CompactEvent {
        trigger: if config.context_window_tokens > 0 {
            ThresholdKind::ContextWindowHalf
        } else {
            ThresholdKind::AbsoluteMax
        },
        estimated_raw,
        estimated_calibrated: estimated_raw,
        window: config.context_window_tokens,
        removed,
        walkback_extra: naive_keep_from.saturating_sub(keep_from),
        tail_tokens,
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
        compact_event_for, compact_session_in_place, compaction_stats, estimate_session_tokens,
        estimate_text_tokens, format_compact_summary, get_compact_continuation_message,
        plan_compaction_cut, read_stats_file, record_compact_event, record_repair_drops,
        set_compact_stats_path, should_compact, should_compact_with_estimate, CompactEvent,
        CompactionConfig, SoftTrigger, ThresholdKind, TokenCalibration,
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
            ..CompactionConfig::default()
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
        // preserve_recent_messages=2 would cut right before the tool_result at
        // index 2; boundary alignment walks back over it so the tail never opens
        // on an orphaned result, pulling the preceding assistant in too. Only the
        // leading user message is therefore folded, not two.
        assert_eq!(result.removed_message_count, 1);

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
    fn compaction_never_orphans_a_tool_result_at_the_boundary() {
        // preserve_recent_messages = 1 would naively cut right before the tool
        // result, folding its assistant(tool_use) into the summary and leaving
        // an orphan the provider rejects with HTTP 400. The boundary must walk
        // back over the trailing tool result to keep the pair together.
        let mut session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("older context ".repeat(80)),
                ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                }]),
                ConversationMessage::tool_result("call_1", "bash", "ok", false),
            ],
        };

        let result = compact_session_in_place(
            &mut session,
            CompactionConfig {
                preserve_recent_messages: 1,
                max_estimated_tokens: 1,
                ..CompactionConfig::default()
            },
        );

        // Only the leading user message is folded; the assistant+result pair is
        // pulled whole into the preserved tail, so no orphan reaches the wire.
        assert_eq!(result.removed_message_count, 1);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(
            session.messages[1].role,
            MessageRole::Assistant,
            "the tool_use assistant survives with its result, never orphaned"
        );
        assert_eq!(session.messages[2].role, MessageRole::Tool);
        assert!(matches!(
            &session.messages[1].blocks[0],
            ContentBlock::ToolUse { id, .. } if id == "call_1"
        ));
    }

    #[test]
    fn soft_trigger_default_reproduces_the_half_window_gate() {
        // Rollback spec: the default `SoftTrigger::HalfWindow` must be
        // byte-equivalent to the pre-D4.1 `estimated >= window / 2` check.
        let config = CompactionConfig {
            preserve_recent_messages: 4,
            context_window_tokens: 1_000,
            ..CompactionConfig::default()
        };
        assert!(!should_compact_with_estimate(10, 499, config));
        assert!(should_compact_with_estimate(10, 500, config));
    }

    #[test]
    fn soft_trigger_ds4_fires_late_by_percent_or_free_floor() {
        // 32k window: free floor = min(8192, 8_000) = 8_000, so the floor
        // branch fires at remaining <= 8_000 (estimated >= 24_000) — before
        // the 85% line at 27_200. Both branches are exercised in binding
        // order.
        let config = CompactionConfig {
            preserve_recent_messages: 4,
            context_window_tokens: 32_000,
            soft_trigger: SoftTrigger::Ds4,
            ..CompactionConfig::default()
        };
        assert!(!should_compact_with_estimate(10, 23_999, config));
        assert!(should_compact_with_estimate(10, 24_000, config));
        assert!(should_compact_with_estimate(10, 27_200, config));
    }

    #[test]
    fn soft_trigger_ds4_caps_the_free_floor_for_small_contexts() {
        // ds4's motivation: a tiny test window must not compact after every
        // short turn. window = 2_000 caps the 8192 floor at window/4 = 500,
        // so the floor branch fires only at estimated >= 1_500.
        let config = CompactionConfig {
            preserve_recent_messages: 1,
            context_window_tokens: 2_000,
            soft_trigger: SoftTrigger::Ds4,
            ..CompactionConfig::default()
        };
        assert!(!should_compact_with_estimate(10, 1_200, config));
        assert!(should_compact_with_estimate(10, 1_500, config));
    }

    #[test]
    fn tail_budget_walks_the_cut_forward_without_orphaning_results() {
        // preserve=4 naively keeps [assistant(tool_use), huge result, u4, u5],
        // whose tail alone outgrows the budget. The cut must walk forward,
        // swallowing the ad+result pair whole, and end on a non-Tool head.
        let big_result = "y".repeat(40_000);
        let messages = vec![
            ConversationMessage::user_text("early filler ".repeat(40)),
            ConversationMessage::user_text("filler two ".repeat(40)),
            ConversationMessage::user_text("filler three ".repeat(40)),
            ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "call_big".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
            }]),
            ConversationMessage::tool_result("call_big", "bash", &big_result, false),
            ConversationMessage::user_text("recent one ".repeat(40)),
            ConversationMessage::user_text("recent two ".repeat(40)),
        ];
        let config = CompactionConfig {
            preserve_recent_messages: 4,
            tail_budget_tokens: 2_000,
            ..CompactionConfig::default()
        };

        let plan = plan_compaction_cut(&messages, &config);

        // The oversized pair is summarized together; the tail is the two
        // recent user messages and opens on a User message, not a Tool.
        assert_eq!(plan.keep_from, 5);
        assert!(plan.tail_tokens <= 2_000);
        assert_eq!(messages[plan.keep_from].role, MessageRole::User);

        // Parity: without a budget the cut is the pure count cut (D4.3
        // rollback default), keeping the oversized pair verbatim.
        let plan_without = plan_compaction_cut(
            &messages,
            &CompactionConfig {
                preserve_recent_messages: 4,
                ..CompactionConfig::default()
            },
        );
        assert_eq!(plan_without.keep_from, 3);
    }

    #[test]
    fn tail_budget_never_yields_an_empty_or_orphaned_tail() {
        // A single oversized ad+result pair that cannot fit any budget: the
        // plan keeps it (never an empty tail), and the backward alignment
        // keeps the pair together instead of opening on the orphan result.
        let messages = vec![
            ConversationMessage::user_text("old context ".repeat(40)),
            ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "call_huge".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
            }]),
            ConversationMessage::tool_result("call_huge", "bash", "y".repeat(40_000), false),
        ];
        let plan = plan_compaction_cut(
            &messages,
            &CompactionConfig {
                preserve_recent_messages: 1,
                tail_budget_tokens: 100,
                ..CompactionConfig::default()
            },
        );
        assert_eq!(plan.keep_from, 1, "the pair is kept whole");
        assert!(plan.tail_tokens > 100, "overflow is reported honestly");
        assert_ne!(messages[plan.keep_from].role, MessageRole::Tool);
    }

    #[test]
    fn continuation_message_declares_omitted_tokens() {
        let without = get_compact_continuation_message("summary body", true, true, 0);
        assert!(!without.contains("estimated tokens"));

        let with = get_compact_continuation_message("summary body", true, true, 1_234);
        assert!(with.contains("Roughly 1234 estimated tokens of older conversation"));
        assert!(with.contains("preserved verbatim"));
    }

    #[test]
    fn pinned_assistant_keeps_its_tool_results_verbatim() {
        // /pin can land on an assistant tool_use message. If its result fell
        // into the summarized region, the surviving ad would dangle on the
        // wire (provider 400), so compaction must pull the results along.
        let mut session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("old context ".repeat(80)),
                ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                    id: "call_pinned".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                }])
                .with_pinned(true),
                ConversationMessage::tool_result("call_pinned", "bash", "ok", false),
                ConversationMessage::user_text("recent filler ".repeat(80)),
                ConversationMessage::user_text("newest".to_string()),
            ],
        };

        let result = compact_session_in_place(
            &mut session,
            CompactionConfig {
                preserve_recent_messages: 1,
                max_estimated_tokens: 1,
                ..CompactionConfig::default()
            },
        );

        // The pinned ad and its result both survive verbatim and adjacent;
        // only the old user context and the recent filler are summarized.
        let surviving_tool_use = session.messages.iter().any(|m| {
            m.role == MessageRole::Assistant
                && m.blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "call_pinned"))
        });
        let surviving_result = session.messages.iter().any(|m| {
            m.role == MessageRole::Tool
                && m.blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_pinned"))
        });
        assert!(
            surviving_tool_use && surviving_result,
            "pair survives together"
        );
        assert_eq!(result.removed_message_count, 2);
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

    // --- Phase 0 instrumentation (roadmap §4 compaction 改造规格) ---

    #[test]
    fn compact_event_fields_mirror_the_execution() {
        let window_config = CompactionConfig {
            context_window_tokens: 200_000,
            ..CompactionConfig::default()
        };
        let event = compact_event_for(9_000, window_config, 6, 5, 1_200, 3);
        assert_eq!(event.trigger, ThresholdKind::ContextWindowHalf);
        assert_eq!(event.window, 200_000);
        assert_eq!(event.estimated_raw, 9_000);
        // Execution-side events carry no calibration fit: the two figures are
        // identical by construction, never a silent guess.
        assert_eq!(event.estimated_calibrated, 9_000);
        assert_eq!(event.walkback_extra, 1);
        assert_eq!(event.tail_tokens, 1_200);
        assert_eq!(event.removed, 3);

        // Window unset: the absolute-threshold gate is what fired.
        let absolute = compact_event_for(500, CompactionConfig::default(), 4, 4, 80, 1);
        assert_eq!(absolute.trigger, ThresholdKind::AbsoluteMax);
        assert_eq!(absolute.window, 0);
        assert_eq!(absolute.walkback_extra, 0, "no boundary walkback");
    }

    #[test]
    fn compaction_counters_track_events_by_trigger() {
        // Process-global counters are shared across tests, so assert deltas
        // (other tests only ever add).
        let before = compaction_stats();
        record_compact_event(&CompactEvent {
            trigger: ThresholdKind::ContextWindowHalf,
            estimated_raw: 1,
            estimated_calibrated: 1,
            window: 10,
            removed: 2,
            walkback_extra: 0,
            tail_tokens: 3,
        });
        record_compact_event(&CompactEvent {
            trigger: ThresholdKind::AbsoluteMax,
            estimated_raw: 1,
            estimated_calibrated: 1,
            window: 0,
            removed: 1,
            walkback_extra: 0,
            tail_tokens: 3,
        });
        let after = compaction_stats();
        assert!(after.count >= before.count + 2);
        assert!(after.window_half > before.window_half);
        assert!(after.absolute > before.absolute);
        assert!(after.removed_total >= before.removed_total + 3);
    }

    #[test]
    fn stats_file_roundtrips_and_tolerates_corruption() {
        let path =
            std::env::temp_dir().join(format!("hf-compact-stats-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        set_compact_stats_path(path.clone());

        let before = compaction_stats();
        record_compact_event(&CompactEvent {
            trigger: ThresholdKind::ContextWindowHalf,
            estimated_raw: 10,
            estimated_calibrated: 10,
            window: 20,
            removed: 5,
            walkback_extra: 1,
            tail_tokens: 30,
        });
        // The durable file holds the merged totals, so the observed event is
        // visible to another process (e.g. `hf doctor`).
        let on_disk = read_stats_file(&path).expect("stats file written");
        assert!(on_disk.count >= 1);
        assert!(on_disk.removed_total >= 5);
        assert!(compaction_stats().count > before.count);

        // A torn or hand-mangled file degrades to "no history" — never an
        // error, never a panic. The corrupt file must be a *private* path:
        // the process-global stats file is concurrently rewritten by other
        // tests' event records, which would heal the corruption mid-assert
        // and turn this into a parallel-test race.
        let torn =
            std::env::temp_dir().join(format!("hf-compact-stats-torn-{}.json", std::process::id()));
        std::fs::write(&torn, "{not json").expect("write corrupt file");
        assert_eq!(read_stats_file(&torn), None);
        let _ = std::fs::remove_file(&torn);
        let _ = compaction_stats();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn repair_drops_are_counted() {
        let before = compaction_stats();
        record_repair_drops(&["call_orphan".to_string(), "call_orphan_2".to_string()]);
        let after = compaction_stats();
        assert!(after.repair_dropped >= before.repair_dropped + 2);
        // Empty input is a no-op, not a zero-drop event.
        record_repair_drops(&[]);
        assert_eq!(compaction_stats().repair_dropped, after.repair_dropped);
    }

    #[test]
    fn real_compaction_feeds_the_counters() {
        // End-to-end through the public entry point: a session past the
        // threshold, compacted, moves the window-half counter.
        let mut session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("old context ".repeat(200)),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "older reply ".repeat(200),
                }]),
                ConversationMessage::user_text("recent".to_string()),
            ],
        };
        let before = compaction_stats();
        let result = compact_session_in_place(
            &mut session,
            CompactionConfig {
                preserve_recent_messages: 1,
                max_estimated_tokens: 1,
                ..CompactionConfig::default()
            },
        );
        assert!(result.removed_message_count >= 1);
        let after = compaction_stats();
        assert!(after.count > before.count);
        assert!(after.removed_total >= before.removed_total + result.removed_message_count as u64);
        assert!(estimate_session_tokens(&session) > 0);
    }
}
