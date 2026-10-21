//! Session core: the single state layer the UI (line renderer now, ratatui
//! view later) reads from, and the queue/guide logic that must run *inside* a
//! live turn. Pure state transitions only — no rendering, no terminal access —
//! mirroring the repo rule that the renderer must not own logic.

use std::collections::VecDeque;
use std::fmt::Write as _;

use runtime::{ContentBlock, ConversationMessage, MessageRole};

/// FIFO of messages waiting to become the next turn. Roadmap §1 P-A: the old
/// string FIFO became an *entry* queue — every item carries an id, a source
/// and a lifecycle state, which is what per-item delivery (P-B), the queue
/// editing surface and subagent-result absorption all need.
///
/// Semantics locked with the operator (Claude Code queue model): a queued
/// message never interrupts the running turn; once the turn finishes, entries
/// are delivered one at a time in arrival order (pi steering per-item model,
/// D2.2) — the old whole-queue merge survives as an opt-in (`set_merged`)
/// because it is a context-size optimization, not a semantic.
/// Queued items are deliberately kept out of the command history so they can
/// never reappear there while still pending.
#[derive(Debug)]
pub struct FollowUpQueue {
    pending: VecDeque<QueueEntry>,
    recent: VecDeque<QueueEntry>,
    next_id: u64,
    merged: bool,
    limit: usize,
}

/// Hard cap on pending messages; overflowing the cap is a UI bug source, so
/// the queue refuses instead of growing without bounds.
pub const DEFAULT_QUEUE_LIMIT: usize = 32;

/// Terminal-state ledger cap (pi steering keeps a recent-20 window; anything
/// older drops off so `/queue` cannot grow forever).
pub const RECENT_LEDGER_CAP: usize = 20;

/// Where a queued entry came from. `Mission` lands with the roadmap §1
/// mission ledger; `Subagent` entries are worker results absorbed by the
/// supervisor loop (the "监工收到消息才工作" wake path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSource {
    User,
    Guide,
    Subagent,
}

impl QueueSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            QueueSource::User => "user",
            QueueSource::Guide => "guide",
            QueueSource::Subagent => "subagent",
        }
    }
}

/// Lifecycle of one entry. Every id ends in exactly one terminal state in the
/// `recent` ledger (the "条目不丢" invariant the tests pin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEntryState {
    Pending,
    Delivered,
    Withdrawn,
    Failed,
}

impl QueueEntryState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            QueueEntryState::Pending => "pending",
            QueueEntryState::Delivered => "delivered",
            QueueEntryState::Withdrawn => "withdrawn",
            QueueEntryState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: u64,
    pub text: String,
    pub source: QueueSource,
    pub state: QueueEntryState,
}

impl Default for FollowUpQueue {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            recent: VecDeque::new(),
            next_id: 1,
            merged: false,
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
            pending: VecDeque::new(),
            recent: VecDeque::new(),
            next_id: 1,
            merged: false,
            limit: limit.max(1),
        }
    }

    /// Enqueue one message (compat shim over [`Self::send`], TUI path).
    /// Empty lines are rejected; returns `false` when the queue is full so
    /// the caller can tell the user instead of dropping.
    pub fn push(&mut self, text: &str) -> bool {
        self.send(text, QueueSource::User).is_some()
    }

    /// Queue one entry explicitly; returns its id, or `None` when the text is
    /// blank or the pending queue is full. This is the manual "排队" path
    /// (`/queue send`) and the subagent absorption path.
    pub fn send(&mut self, text: &str, source: QueueSource) -> Option<u64> {
        let text = text.trim();
        if text.is_empty() || self.pending.len() >= self.limit {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.pending.push_back(QueueEntry {
            id,
            text: text.to_string(),
            source,
            state: QueueEntryState::Pending,
        });
        Some(id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Pending entries in arrival order, for the status listing.
    #[must_use]
    pub fn items(&self) -> &VecDeque<QueueEntry> {
        &self.pending
    }

    /// Terminal-state ledger, newest first, bounded.
    #[must_use]
    pub fn recent(&self) -> &VecDeque<QueueEntry> {
        &self.recent
    }

    #[must_use]
    pub fn is_merged(&self) -> bool {
        self.merged
    }

    /// Opt-in whole-queue merge at the delivery point (D2.2: merge is a
    /// context optimization, not the default semantic).
    pub fn set_merged(&mut self, merged: bool) {
        self.merged = merged;
    }

    /// Take the oldest pending entry out for delivery. The caller either
    /// finishes it with [`Self::finish_entry`] or hands it back with
    /// [`Self::requeue_front`] — between the two, no entry can be lost.
    pub fn take_next(&mut self) -> Option<QueueEntry> {
        self.pending.pop_front()
    }

    /// Mark a taken entry delivered (or failed) and file it in the ledger.
    pub fn finish_entry(&mut self, mut entry: QueueEntry, state: QueueEntryState) {
        entry.state = state;
        self.file_recent(entry);
    }

    /// Put a taken entry back at the head (delivery failed; per-item retry
    /// instead of the old whole-queue re-queue).
    pub fn requeue_front(&mut self, entry: QueueEntry) {
        if self.pending.len() >= self.limit {
            self.file_recent(QueueEntry {
                state: QueueEntryState::Failed,
                ..entry
            });
            return;
        }
        self.pending.push_front(entry);
    }

    /// Replace the text of a pending entry (queue editing, D2.4).
    pub fn edit(&mut self, id: u64, new_text: &str) -> bool {
        let new_text = new_text.trim();
        if new_text.is_empty() {
            return false;
        }
        for entry in &mut self.pending {
            if entry.id == id {
                entry.text = new_text.to_string();
                return true;
            }
        }
        false
    }

    /// Withdraw any pending entry (supersedes `cancel_last`'s last-only
    /// reach); returns the text so the caller can restore it to the input.
    pub fn withdraw(&mut self, id: u64) -> Option<String> {
        let position = self.pending.iter().position(|entry| entry.id == id)?;
        let mut entry = self.pending.remove(position)?;
        entry.state = QueueEntryState::Withdrawn;
        let text = entry.text.clone();
        self.file_recent(entry);
        Some(text)
    }

    /// Move a pending entry one slot up (towards earlier delivery) or down.
    pub fn move_entry(&mut self, id: u64, up: bool) -> bool {
        let Some(position) = self.pending.iter().position(|entry| entry.id == id) else {
            return false;
        };
        let target = if up {
            match position.checked_sub(1) {
                Some(target) => target,
                None => return false,
            }
        } else {
            let next = position + 1;
            if next >= self.pending.len() {
                return false;
            }
            next
        };
        self.pending.swap(position, target);
        true
    }

    /// Withdraw the most recently queued message (Esc-on-queued semantics);
    /// returns it so the caller can restore it into the input buffer.
    pub fn cancel_last(&mut self) -> Option<String> {
        let last = self.pending.back().map(|entry| entry.id)?;
        self.withdraw(last)
    }

    /// Drain every pending entry into one injected user message, oldest
    /// first, leaving the queue empty (compat: TUI turn boundary + merged
    /// mode). Delivered entries land in the ledger.
    pub fn drain_injection(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let merged = merge_injection(
            &self
                .pending
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
        );
        for mut entry in std::mem::take(&mut self.pending) {
            entry.state = QueueEntryState::Delivered;
            self.file_recent(entry);
        }
        Some(merged)
    }

    fn file_recent(&mut self, entry: QueueEntry) {
        self.recent.push_front(entry);
        self.recent.truncate(RECENT_LEDGER_CAP);
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

/// The runtime-derived half of a guide draft: a compact work log and a one-line
/// current state. Captured as a snapshot (never assembled live during a turn,
/// when the runtime is borrowed by the turn future) so the ratatui Ctrl+G
/// overlay can preview a full draft; `task` is supplied by the operator at
/// confirm time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuideContext {
    pub work_log: String,
    pub state: String,
}

/// Extract the work log + state from a transcript and a pending-task count: the
/// last few human/assistant turns folded into high-density log lines, plus the
/// ledger depth. Shared by the blocking `/guide` command and the ratatui overlay
/// so the two assemble byte-identical drafts. Pure — reads the message slice,
/// no I/O.
#[must_use]
pub fn guide_context(messages: &[ConversationMessage], pending_tasks: usize) -> GuideContext {
    let mut work_log = String::new();
    for message in messages.iter().rev().take(6).rev() {
        let role = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            // System prompt and raw tool turns are noise for re-entry context;
            // the guide summarizes the human/assistant thread only.
            MessageRole::System | MessageRole::Tool => continue,
        };
        let text = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        if !text.trim().is_empty() {
            work_log.push_str(&guide_log_line(role, &text, 160));
            work_log.push('\n');
        }
    }
    GuideContext {
        work_log,
        state: format!("{pending_tasks} pending tasks in the ledger"),
    }
}

/// Assemble the full three-part draft from a snapshot context plus the
/// operator's next task. The single sink for guide text, so the blocking command
/// and the ratatui overlay never drift.
#[must_use]
pub fn guide_draft(context: &GuideContext, task: &str) -> String {
    build_guide(GuideSections {
        work_log: &context.work_log,
        state: &context.state,
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_guide, guide_context, guide_draft, FollowUpQueue, GuideContext, GuideSections,
        HeartModel, QueueEntryState, QueueSource,
    };

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
    fn per_item_delivery_every_id_reaches_a_terminal_state() {
        let mut q = FollowUpQueue::with_limit(4);
        let a = q.send("a", QueueSource::User).expect("a");
        let b = q.send("b", QueueSource::Guide).expect("b");
        assert_ne!(a, b, "ids are unique");

        // Take a, deliver it; take b, fail it back, then deliver on retry.
        let taken = q.take_next().expect("first take");
        assert_eq!(taken.id, a, "oldest first");
        q.finish_entry(taken, QueueEntryState::Delivered);
        let taken = q.take_next().expect("second take");
        assert_eq!(taken.id, b);
        q.requeue_front(taken);
        assert_eq!(q.len(), 1, "failed entry returns to pending");
        let taken = q.take_next().expect("third take");
        q.finish_entry(taken, QueueEntryState::Delivered);

        // 条目不丢 + 递送必终态化: every id lives in recent with a terminal
        // state, nothing remains pending.
        assert!(q.is_empty());
        let recent_ids: std::collections::HashSet<u64> =
            q.recent().iter().map(|entry| entry.id).collect();
        assert!(recent_ids.contains(&a) && recent_ids.contains(&b));
        assert!(q
            .recent()
            .iter()
            .all(|entry| entry.state != QueueEntryState::Pending));
        // Newest-first ledger ordering.
        assert_eq!(q.recent().front().map(|entry| entry.id), Some(b));
    }

    #[test]
    fn queue_edit_withdraw_and_reorder_target_any_entry() {
        let mut q = FollowUpQueue::with_limit(4);
        let a = q.send("alpha", QueueSource::User).expect("a");
        let b = q.send("beta", QueueSource::User).expect("b");
        let c = q.send("gamma", QueueSource::User).expect("c");

        assert!(q.edit(b, "beta v2"), "edit any pending entry");
        assert!(q
            .items()
            .iter()
            .any(|entry| entry.id == b && entry.text == "beta v2"));
        assert!(!q.edit(999, "nope"), "unknown id refuses");
        assert!(!q.edit(c, "   "), "blank replacement refuses");

        // move b up: order becomes b, a, c.
        assert!(q.move_entry(b, true));
        let order: Vec<u64> = q.items().iter().map(|entry| entry.id).collect();
        assert_eq!(order, vec![b, a, c]);
        assert!(!q.move_entry(b, true), "head cannot move up");
        assert!(q.move_entry(b, false));

        // Withdraw the middle entry by id (not just the last).
        assert_eq!(q.withdraw(a).as_deref(), Some("alpha"));
        assert_eq!(q.len(), 2);
        assert!(
            q.recent()
                .iter()
                .any(|entry| entry.id == a && entry.state == QueueEntryState::Withdrawn),
            "withdraw files the terminal state"
        );
    }

    #[test]
    fn requeue_front_at_capacity_files_failed_not_lost() {
        let mut q = FollowUpQueue::with_limit(1);
        let a = q.send("a", QueueSource::User).expect("a");
        let taken = q.take_next().expect("take");
        assert_eq!(taken.id, a);
        let _b = q.send("b", QueueSource::User).expect("b fills the slot");
        // Re-delivery of `a` fails while the queue is full: it must land in
        // the ledger as Failed, never silently vanish.
        q.requeue_front(taken);
        assert!(q
            .recent()
            .iter()
            .any(|entry| entry.id == a && entry.state == QueueEntryState::Failed));
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

    #[test]
    fn guide_draft_wraps_context_and_task_in_order() {
        let context = GuideContext {
            work_log: String::from("user: hi\n"),
            state: String::from("2 pending tasks in the ledger"),
        };
        let draft = guide_draft(&context, "do next");
        assert!(draft.contains("[guide: prior work]\nuser: hi"));
        assert!(draft.contains("[guide: current state]\n2 pending tasks"));
        assert!(draft.ends_with("[guide: next task]\ndo next"));
    }

    #[test]
    fn guide_context_on_empty_transcript_reports_state_only() {
        let context = guide_context(&[], 3);
        assert!(context.work_log.is_empty());
        assert_eq!(context.state, "3 pending tasks in the ledger");
    }
}
