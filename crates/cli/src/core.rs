//! Session core: the single state layer the UI (line renderer now, ratatui
//! view later) reads from, and the queue/guide logic that must run *inside* a
//! live turn. Pure state transitions only — no rendering, no terminal access —
//! mirroring the repo rule that the renderer must not own logic.

use std::collections::VecDeque;
use std::fmt::Write as _;

/// FIFO of user messages submitted while a turn is running.
///
/// Semantics locked with the operator (Claude Code queue model): a queued
/// message never interrupts the running turn; once the turn finishes, the
/// whole queue is merged into *one* user message injected in arrival order.
/// Queued items are deliberately kept out of the command history so they can
/// never reappear there while still pending.
#[derive(Debug)]
pub struct FollowUpQueue {
    items: VecDeque<String>,
    limit: usize,
}

/// Hard cap on pending messages; overflowing the cap is a UI bug source, so
/// the queue refuses instead of growing without bounds.
pub const DEFAULT_QUEUE_LIMIT: usize = 32;

impl Default for FollowUpQueue {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            limit: DEFAULT_QUEUE_LIMIT,
        }
    }
}

impl FollowUpQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            items: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    /// Enqueue one message. Empty lines are rejected; returns `false` when the
    /// queue is full so the caller can tell the user instead of dropping.
    pub fn push(&mut self, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() || self.items.len() >= self.limit {
            return false;
        }
        self.items.push_back(text.to_string());
        true
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Pending messages in arrival order, for the status listing.
    #[must_use]
    pub fn items(&self) -> &VecDeque<String> {
        &self.items
    }

    /// Withdraw the most recently queued message (Esc-on-queued semantics);
    /// returns it so the caller can restore it into the input buffer.
    pub fn cancel_last(&mut self) -> Option<String> {
        self.items.pop_back()
    }

    /// Drain every pending message into one injected user message, oldest
    /// first, leaving the queue empty. Returns `None` when nothing is queued.
    pub fn drain_injection(&mut self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let merged = merge_injection(&self.items.iter().map(String::as_str).collect::<Vec<_>>());
        self.items.clear();
        Some(merged)
    }
}

/// Merge queued messages into the single injection text. ASCII-only header:
/// Windows Git-Bash mangles non-ASCII in local pre-checks, and the string
/// reaches the model, where stable wording beats prettier wording.
#[must_use]
pub fn merge_injection(messages: &[&str]) -> String {
    let mut out = String::from("[queued follow-ups, oldest first]\n");
    for (index, message) in messages.iter().enumerate() {
        let _ = writeln!(out, "{}. {message}", index + 1);
    }
    out
}

/// The one live-turn state: is a turn running, and what is queued behind it.
/// The queue only fills while a turn runs (under the blocking line editor it
/// is by definition idle), and end-of-turn delivery is *not* modelled here:
/// the REPL drains the queue via [`FollowUpQueue::drain_injection`] itself, so
/// a refused turn (approval denied, error) can re-queue the envelope instead
/// of losing it.
#[derive(Debug, Default)]
pub struct HeartModel {
    running: bool,
    queue: FollowUpQueue,
}

impl HeartModel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            running: false,
            queue: FollowUpQueue::new(),
        }
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Mark a turn as started; idempotent so a nested caller can never
    /// desync the flag.
    pub fn begin_turn(&mut self) {
        self.running = true;
    }

    /// Mark the turn finished; pending queue survives for the caller to drain.
    pub fn end_turn(&mut self) {
        self.running = false;
    }

    /// Queue a message submitted during a running turn; `false` means the
    /// queue is full and the caller must surface the refusal.
    pub fn enqueue(&mut self, text: &str) -> bool {
        self.queue.push(text)
    }

    #[must_use]
    pub fn queue(&self) -> &FollowUpQueue {
        &self.queue
    }

    pub fn queue_mut(&mut self) -> &mut FollowUpQueue {
        &mut self.queue
    }
}

/// Three-part guide draft assembled locally (zero token, no model call):
/// prior work, current state, next task. The section order is the locked
/// design: the task goes last because that is what the operator still edits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuideSections<'a> {
    pub work_log: &'a str,
    pub state: &'a str,
    pub task: &'a str,
}

/// Build the guide injection text; empty sections are skipped so a fresh
/// section collapses to just the task.
#[must_use]
pub fn build_guide(sections: GuideSections<'_>) -> String {
    let mut out = String::new();
    if !sections.work_log.trim().is_empty() {
        out.push_str("[guide: prior work]\n");
        out.push_str(sections.work_log.trim());
        out.push('\n');
    }
    if !sections.state.trim().is_empty() {
        out.push_str("[guide: current state]\n");
        out.push_str(sections.state.trim());
        out.push('\n');
    }
    out.push_str("[guide: next task]\n");
    out.push_str(sections.task.trim());
    out
}

/// High-density work-log line from one transcript message: `role: head ... tail`
/// keeps the anchoring start *and* the most recent detail, which is what a
/// model needs to re-enter a conversation. ASCII ellipsis marker for the same
/// byte-safe reason as the queue header.
#[must_use]
pub fn guide_log_line(role: &str, text: &str, budget: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= budget {
        return format!("{role}: {text}");
    }
    // The budget covers the message body only (head + seam + tail); the role
    // prefix rides on top. Seam is " ... " so an ASCII reader sees the fold.
    let seam = " ... ";
    let tail_len = (budget / 6).clamp(1, count);
    let head_len = budget
        .saturating_sub(tail_len + seam.chars().count())
        .max(1);
    let chars: Vec<char> = text.chars().collect();
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[count - tail_len..].iter().collect();
    format!("{role}: {head}{seam}{tail}")
}

#[cfg(test)]
mod tests {
    use super::{build_guide, FollowUpQueue, GuideSections, HeartModel};

    #[test]
    fn queue_preserves_order_and_drains_once() {
        let mut q = FollowUpQueue::new();
        assert!(q.push("first"));
        assert!(q.push("  second  "));
        assert!(!q.push("   "));
        assert_eq!(q.len(), 2);
        let merged = q.drain_injection().expect("two queued");
        let a = merged.find("first").expect("first present");
        let b = merged.find("second").expect("second present");
        assert!(a < b, "oldest must be injected first");
        assert!(merged.contains("1. first"));
        assert!(merged.contains("2. second"));
        assert!(q.is_empty());
        assert!(q.drain_injection().is_none());
    }

    #[test]
    fn queue_enforces_limit_and_withdraws_last() {
        let mut q = FollowUpQueue::with_limit(2);
        assert!(q.push("a"));
        assert!(q.push("b"));
        assert!(!q.push("c"), "third must be refused at the cap");
        assert_eq!(q.cancel_last().as_deref(), Some("b"));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn model_tracks_turn_lifecycle() {
        let mut m = HeartModel::new();
        assert!(!m.is_running());
        m.begin_turn();
        m.begin_turn(); // idempotent
        assert!(m.is_running());
        assert!(m.enqueue("queued"));
        assert_eq!(m.queue().len(), 1);
        m.end_turn();
        assert!(!m.is_running());
        assert!(!m.queue().is_empty(), "end_turn must not consume the queue");
    }

    #[test]
    fn guide_skips_empty_sections_and_keeps_task_last() {
        let fresh = build_guide(GuideSections {
            task: "  do the thing ",
            ..Default::default()
        });
        assert_eq!(fresh, "[guide: next task]\ndo the thing");

        let full = build_guide(GuideSections {
            work_log: "fixed parser",
            state: "2 tasks pending",
            task: "next",
        });
        assert!(full.contains("[guide: prior work]\nfixed parser\n"));
        assert!(full.contains("[guide: current state]\n2 tasks pending\n"));
        assert!(full.ends_with("[guide: next task]\nnext"));
    }

    #[test]
    fn log_line_anchors_head_and_keeps_tail() {
        let short = super::guide_log_line("user", "hello", 40);
        assert_eq!(short, "user: hello");
        let long = super::guide_log_line("assistant", &"字".repeat(400), 60);
        assert!(
            long.chars().count() <= 60 + 12,
            "budget covers the body; only the role prefix rides on top"
        );
        assert!(long.starts_with("assistant: 字字字"));
        assert!(long.ends_with('字'), "tail must survive the fold");
        assert!(long.contains(" ... "));
    }
}
