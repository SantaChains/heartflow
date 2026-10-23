//! Full-screen `ratatui` carrier layer (Phase 2, increment 2b).
//!
//! Three-region full-screen shell that replaces the stdout direct-write path
//! (`TurnRenderer`): a top tab bar, a scrollable central transcript, and a
//! bottom input + status bar. It is gated behind the `HEARTFLOW_TUI=1`
//! environment flag so the default blocking-line REPL stays the proven
//! fallback while the carrier layer is proven out (dual-backend strategy).
//!
//! Architecture is Elm-style single-directional data flow: [`App`] is the model,
//! [`Msg`] the only way to change it (`update`), and [`view`] a pure projection
//! of the model onto a [`Frame`].
//!
//! Concurrency shape (roadmap §11.0, three-phase loop): one task owns the
//! whole shell and drives turns as a **loop-scope future it pumps**, never an
//! inline `await` that parks the loop. Each iteration runs three phases —
//! render (draw when dirty), turn driving (start a queued turn / poll the live
//! one), input collection (`select!` over the turn's completion and the shared
//! `Msg` channel) — so keys, streaming events, permission decisions and
//! cancellation all interleave with redraws *while a turn is in flight*. The
//! turn future is boxed (`Pin<Box<dyn Future>>`) so the loop can hold it
//! without naming the `impl Future` type; the cancel token lives *outside* the
//! future so the input phase can cancel a turn without borrowing it.
//! `ConversationRuntime` carries a `Cell` token counter, so the turn future is
//! not `Send` and cannot live on `tokio::spawn` — the `block_on` path imposes
//! no such bound, so pumping it here is both correct and borrow-clean. The
//! loop is generic over the API client and tool executor, and takes its draw +
//! save side effects as injected seams, so tests drive the real loop with a
//! scripted client and no terminal. crossterm events are pumped from a blocking
//! thread into the shared channel; the loop is the sole consumer and drawer.
//!
//! Permissions (increment 2c): `PermissionPrompter::decide` is async, and
//! [`TuiPermissionPrompter`] raises a [`Msg::PermissionRequest`] over the shared
//! channel then parks on a decision channel while the overlay renders and keeps
//! taking keys. No second terminal reader (inquire) ever fights raw mode.
//!
//! Real-machine gaps deferred to later increments (the shell compiles and the
//! reducer is unit-tested, but these need a live terminal to verify):
//! - Flushed assistant output renders markdown through the shared IR projector
//!   ([`crate::markdown::project_ratatui`], A2b); the transient streaming tail
//!   stays plain text on purpose, since re-parsing the growing buffer every
//!   frame would cost more than it is worth. The transcript virtualizes to the
//!   visible window (A3b), measuring wrapped row heights with ratatui's own
//!   composer so scroll math matches the render; store-side paging for very
//!   long transcripts stays deferred. Tool output already folds interactively
//!   (collapsed marker + `Tab` to expand, see [`Item::Tool`]).
//!
//! Hard constraints honored: no `unsafe`, no `unwrap`/`expect` on fallible
//! paths (the terminal is always restored, even on panic), single crossterm
//! 0.28 backend.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tui_textarea::{CursorMove, TextArea};

use runtime::{
    auto_record_turn, turn_reminder, AgentEvent, ApiClient, ConversationRuntime, MissionLedger,
    PermissionPromptDecision, PermissionPrompter, PermissionRequest, RuntimeError, SteerMailbox,
    TokenUsage, ToolExecutor, TurnSummary,
};

use crate::config::ConfigWatcher;
use crate::core::{
    guide_context, guide_draft, DeliveryMode, GuideContext, HeartModel, QueueSource,
};
use crate::editor::{tui_input, History};
use crate::keymap::{Action, Keymap};
use crate::markdown;
use crate::settings::Settings;
use crate::theme::Theme;
use crate::{expand_attachments, AgentRuntime, DiskSaver, SessionShared, TurnSaver};

type Backend = CrosstermBackend<io::Stdout>;

/// The only way to mutate [`App`]. Key/Resize come from the crossterm pump;
/// Event/TurnStarted/TurnFinished come from a turn — tagged with the section
/// that turn belongs to (§11.4: a background section's turn must feed its own
/// transcript, never the active one's).
pub(crate) enum Msg {
    /// A key press from the crossterm event stream.
    Key(KeyEvent),
    /// A bracketed-paste block, inserted whole into the editor so embedded
    /// newlines stay a multi-line draft instead of each firing a submit.
    Paste(String),
    /// The terminal was resized; forces a relayout + repaint.
    Resize,
    /// One streamed event from the running turn in `section`.
    Event { section: usize, event: AgentEvent },
    /// A submission was picked up and a turn is now live in `section`.
    TurnStarted { section: usize },
    /// The turn in `section` ended; `note` is a short tail (save path, error)
    /// for that section's status.
    TurnFinished {
        section: usize,
        ok: bool,
        note: String,
    },
    /// A tool needs interactive approval; raises the permission overlay.
    PermissionRequest(PermissionRequest),
}

/// Which section a message belongs to (§11.4). Turn traffic carries its own
/// tag; everything else (`None`) belongs to whichever section is active.
fn msg_section(msg: &Msg) -> Option<usize> {
    match msg {
        Msg::Event { section, .. } | Msg::TurnStarted { section } => Some(*section),
        Msg::TurnFinished { section, .. } => Some(*section),
        _ => None,
    }
}

/// What the permission overlay resolved to. Carried back to the prompter over a
/// channel; `AllowAll` also flips the prompter's session-wide auto-approve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionChoice {
    AllowOnce,
    AllowAll,
    Deny,
}

/// The overlay's live state: the request being approved and the highlighted
/// option. Rendered by [`render_permission_overlay`], driven by
/// [`App::permission_key`].
struct PendingPermission {
    request: PermissionRequest,
    options: [&'static str; 3],
    selected: usize,
}

impl PendingPermission {
    fn new(request: PermissionRequest) -> Self {
        Self {
            request,
            options: ["Allow once", "Allow all for this session", "Deny"],
            selected: 0,
        }
    }
}

/// The queue panel's live state (§11.1 P-C): just the selected row — the rows
/// themselves are read live from the follow-up queue, so a settle-time drain
/// can never leave the panel pointing at stale data. Rendered by
/// [`render_queue_overlay`], driven by [`App::queue_panel_key`].
struct QueuePanel {
    selected: usize,
}

impl QueuePanel {
    fn new() -> Self {
        Self { selected: 0 }
    }

    /// Keep the selection on a live row after any mutation that may have
    /// shrunk the queue.
    fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }
}

/// The guide overlay's live state: the next-task text captured from the input
/// and the assembled draft in an **editable** editor (T6 two-column confirm).
/// The draft is built once at open time from the [`GuideContext`] snapshot;
/// whatever the editor holds at confirm time is exactly what is sent.
struct PendingGuide {
    task: String,
    /// The draft body, editable inside the overlay: confirm sends *this*
    /// text, not the originally assembled one.
    editor: TextArea<'static>,
}

/// Full-screen permission prompter. Instead of grabbing the terminal with
/// inquire (which fights raw mode + the alt screen), it raises a
/// [`Msg::PermissionRequest`] over the shared channel and parks on `choice_rx`
/// until the render loop returns the user's selection. This keeps a single
/// terminal reader and lets the overlay repaint while the turn is suspended.
struct TuiPermissionPrompter {
    allow_all: bool,
    msg_tx: mpsc::UnboundedSender<Msg>,
    choice_rx: mpsc::UnboundedReceiver<PermissionChoice>,
}

impl PermissionPrompter for TuiPermissionPrompter {
    fn decide<'a>(
        &'a mut self,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
        Box::pin(async move {
            if self.allow_all {
                return PermissionPromptDecision::Allow;
            }
            if self
                .msg_tx
                .send(Msg::PermissionRequest(request.clone()))
                .is_err()
            {
                return PermissionPromptDecision::Deny {
                    reason: "the UI closed before the request could be shown".to_string(),
                };
            }
            match self.choice_rx.recv().await {
                Some(PermissionChoice::AllowOnce) => PermissionPromptDecision::Allow,
                Some(PermissionChoice::AllowAll) => {
                    self.allow_all = true;
                    PermissionPromptDecision::Allow
                }
                // A closed channel means the loop tore down; deny rather than
                // hang the turn forever.
                Some(PermissionChoice::Deny) | None => PermissionPromptDecision::Deny {
                    reason: "user denied the tool call".to_string(),
                },
            }
        })
    }
}

/// Elm model for the full-screen shell: everything [`view`] renders and
/// everything `update` may change lives here, so the render pass stays pure.
pub(crate) struct App {
    /// Finished transcript entries: output lines plus foldable process entries
    /// (reasoning, tool results).
    transcript: Vec<Item>,
    /// The bottom input editor.
    input: TextArea<'static>,
    /// Lines scrolled *up* from the bottom of the transcript (0 == pinned to
    /// the newest line).
    scroll_up: u16,
    /// Set when `/exit`-equivalent keys ask the loop to tear down.
    quit: bool,
    /// Transient one-line status (last action, hints, errors).
    status_note: String,
    /// This section's tab title. The shell owns the section list and the active
    /// index; a section knows only its own name, which the shell's tab bar
    /// renders (prefixed with `*` while this section's turn runs).
    title: String,
    /// True between `TurnStarted` and `TurnFinished`; drives the live tail and
    /// re-routes Ctrl+C/Esc so a running turn is never killed by accident.
    turn_active: bool,
    /// Assistant text streamed so far this message segment (flushed on
    /// `MessageStop` / before a tool call).
    assistant_buf: String,
    /// Reasoning deltas streamed so far, rendered dimmed+italic.
    thinking_buf: String,
    /// Name of the tool currently executing, if any.
    running_tool: Option<String>,
    /// Number of tools started in the current turn. Resets to 0 when a new
    /// turn begins; used by the status-bar step indicator so users can see
    /// at a glance how far along a multi-tool turn is.
    tool_step: usize,
    /// Most recent usage sample, surfaced in the status bar.
    last_usage: Option<TokenUsage>,
    /// Ctrl+C double-tap arming for cancelling a running turn.
    ctrl_c_armed: bool,
    /// Set by `submit()` when idle; drained by the loop and run as the next
    /// turn. A submit *during* a turn goes to `model`'s follow-up queue instead.
    pending_submit: Option<String>,
    /// Set by `submit_steer()` while a turn runs; drained by the loop into
    /// the live turn's steer mailbox (§11.2). One staged steer at a time —
    /// the loop drains on every processed key, so a second press stages
    /// fresh only after the first has left the reducer.
    pending_steer: Option<String>,
    /// Set by the second Ctrl+C tap; drained by the loop to cancel the turn.
    pending_cancel: bool,
    /// True from the confirming interrupt tap until the turn settles. While
    /// set (or while the double-tap is armed) the rendered status note shows
    /// the cancel state as a sticky override — stream events rewrite the
    /// transient note on every tool start, which used to erase the hint
    /// before it could be read (KI-002).
    cancel_requested: bool,
    /// Shared session core: the follow-up queue plus the turn-running flag.
    /// Submitting while a turn runs enqueues here (silently, never
    /// interrupting); the loop merges the whole queue into one injection at the
    /// turn boundary. Held on the model — not a process singleton — so the
    /// Phase-5 multi-section shell gives each section its own queue.
    model: HeartModel,
    /// Runtime-derived half of a guide draft (work log + state), captured as a
    /// snapshot where the runtime is free — startup and turn boundaries — never
    /// mid-turn, when the pinned turn future holds the `&mut` borrow. Ctrl+G
    /// folds this plus the operator's task into the previewed draft.
    guide_context: GuideContext,
    /// Live permission overlay, if a tool is awaiting interactive approval.
    permission: Option<PendingPermission>,
    /// The overlay's resolved choice, drained by the loop and returned to the
    /// waiting prompter.
    permission_choice: Option<PermissionChoice>,
    /// Live guide overlay, if the operator is previewing a next-task draft.
    guide: Option<PendingGuide>,
    /// True while the read-only key-reference overlay is raised. A plain bool
    /// (not an `Option`) because the overlay has no payload: it renders straight
    /// from the live keymap, so it always shows the current bindings.
    help_open: bool,
    /// Live queue panel, if the operator is inspecting/editing the pending
    /// follow-ups (§11.1 P-C).
    queue_panel: Option<QueuePanel>,
    /// Resolved key bindings: defaults plus layered `keymap.toml` overrides.
    /// Held on the model rather than a process-wide singleton so a turn-boundary
    /// hot-reload can swap it without unsafe global state.
    keymap: Keymap,
    /// Persistent input history, shared with the blocking REPL via the same
    /// `~/.heartflow/history.txt` file so Up/Down recall behaves identically on
    /// both surfaces. Hermetic construction leaves it empty and in-memory-only;
    /// `run_shell` injects the real backing path.
    history: History,
    /// The file backing [`Self::history`]. Empty until `run_shell` sets it, so
    /// a test-built section never touches the filesystem on submit.
    history_path: PathBuf,
    /// Behavior knobs (scroll stride, fold threshold, redraw throttle) from
    /// layered `settings.toml`, held on the model for the same hot-reload reason.
    settings: Settings,
    /// Bumped on every transcript mutation (push or fold toggle). Paired with
    /// [`Theme::generation`] it keys [`Self::render_cache`], so a redraw reuses
    /// the projected lines unless the transcript or the palette actually changed.
    transcript_version: u64,
    /// Cached projection of `transcript` into owned display lines. A streaming
    /// redraw only grows the live tail, never the frozen transcript, so reusing
    /// this cache makes a frame O(visible rows) instead of O(transcript).
    /// `RefCell` because [`view`] is a pure `&App` projection (Elm-style) yet
    /// must memoize; the render loop is single-threaded, so the borrow never
    /// contends. Rebuilt at most once per transcript/theme change.
    render_cache: RefCell<Vec<Line<'static>>>,
    /// The `(transcript_version, theme_generation)` [`Self::render_cache`] was
    /// built at. A mismatch triggers a rebuild on the next frame.
    render_key: Cell<(u64, u64)>,
}

impl App {
    /// A fresh `main` section with a short orientation notice.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::titled(String::from("main"))
    }

    /// A section with a given tab title and otherwise empty state. The shell
    /// names each section as it is spawned (`main`, then `2`, `3`, …).
    /// Construction stays hermetic (no disk I/O): tests build a section with the
    /// shipped default bindings and settings, and `run_shell` layers the
    /// on-disk `keymap.toml`/`settings.toml` on top for a real session.
    fn titled(title: String) -> Self {
        let keymap = Keymap::default();
        let input = editor(&keymap);
        Self {
            title,
            transcript: Vec::new(),
            input,
            scroll_up: 0,
            quit: false,
            status_note: String::from("full-screen shell · the input bar lists the active keys"),
            turn_active: false,
            assistant_buf: String::new(),
            thinking_buf: String::new(),
            running_tool: None,
            tool_step: 0,
            last_usage: None,
            ctrl_c_armed: false,
            pending_submit: None,
            pending_steer: None,
            pending_cancel: false,
            cancel_requested: false,
            model: HeartModel::new(),
            guide_context: GuideContext::default(),
            permission: None,
            permission_choice: None,
            guide: None,
            help_open: false,
            queue_panel: None,
            keymap,
            history: History::empty(),
            history_path: PathBuf::new(),
            settings: Settings::default(),
            transcript_version: 0,
            render_cache: RefCell::new(Vec::new()),
            // A sentinel that never matches a real (version, generation) pair,
            // so the first frame always builds the cache.
            render_key: Cell::new((u64::MAX, u64::MAX)),
        }
    }

    /// Fold one message into the model. Returns whether a repaint is needed so
    /// the loop can skip drawing on no-op ticks.
    fn update(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Resize => true,
            Msg::Key(key) => self.on_key(key),
            Msg::Paste(text) => self.on_paste(text),
            // The section tag is loop-level routing; the reducer is already
            // talking to the right section when it sees these.
            Msg::Event { event, .. } => self.apply_event(&event),
            Msg::TurnStarted { .. } => {
                self.turn_active = true;
                self.assistant_buf.clear();
                self.thinking_buf.clear();
                self.running_tool = None;
                self.tool_step = 0;
                self.ctrl_c_armed = false;
                self.cancel_requested = false;
                let hint = self.cancel_key_hint();
                self.status_note = format!("turn running · {hint} twice to cancel");
                true
            }
            Msg::TurnFinished { ok, note, .. } => {
                self.finish_turn(ok, &note);
                true
            }
            Msg::PermissionRequest(request) => {
                self.scroll_up = 0;
                // A pending approval supersedes guide editing and the queue
                // panel; drop those overlays so they cannot stay stuck
                // behind the permission popup.
                self.guide = None;
                self.queue_panel = None;
                self.permission = Some(PendingPermission::new(request));
                self.status_note =
                    String::from("permission requested · ↑/↓ select · Enter confirm · Esc deny");
                true
            }
        }
    }

    /// Route a key: resolve it to a semantic [`Action`] through the keymap, then
    /// dispatch. A key no binding claims falls through to the input editor.
    /// Returns whether the model changed.
    fn on_key(&mut self, key: KeyEvent) -> bool {
        // A raised permission overlay swallows every key until it resolves, so
        // a pending approval can never be clobbered by stray editor input.
        if self.permission.is_some() {
            return self.permission_key(key);
        }
        // The guide overlay, like the permission one, swallows keys until it
        // resolves so a confirmation is never clobbered by stray editor input.
        if self.guide.is_some() {
            return self.guide_key(key);
        }
        // The key-reference overlay is read-only and modal while open: it
        // swallows every key so a stray keystroke cannot reach the editor behind
        // it, and only its own close keys dismiss it.
        if self.help_open {
            return self.help_key(key);
        }
        // The queue panel is modal while open (§11.1 P-C): it owns the keys
        // so navigation/edit chords never leak into the editor behind it.
        if self.queue_panel.is_some() {
            return self.queue_panel_key(key);
        }
        // A modified Enter (shift/alt) inserts a line instead of submitting,
        // mirroring the REPL editor's "bare Enter sends, Shift/Alt+Enter
        // continues" (KI-001). Alt+Enter is also the chord a host may never
        // deliver — Windows Terminal binds it to fullscreen by default and
        // consumes it before the app sees a key — so Shift+Enter is the
        // reliable continuation key there.
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            // Any key other than the interrupt binding disarms the double-tap.
            self.ctrl_c_armed = false;
            self.input.insert_newline();
            return true;
        }
        let action = self.keymap.resolve(&key);
        // Any key other than the interrupt binding disarms the double-tap, so a
        // cancel really requires two consecutive presses of that key.
        if !matches!(action, Some(Action::Interrupt)) {
            self.ctrl_c_armed = false;
        }
        match action {
            Some(Action::Interrupt) => {
                if self.turn_active {
                    if self.ctrl_c_armed {
                        self.pending_cancel = true;
                        self.cancel_requested = true;
                        self.ctrl_c_armed = false;
                    } else {
                        self.ctrl_c_armed = true;
                    }
                } else {
                    self.quit = true;
                }
                true
            }
            Some(Action::Escape) => {
                if self.turn_active {
                    // Escape never kills a running turn; point at the cancel path.
                    let hint = self.cancel_key_hint();
                    self.status_note = format!("turn running · {hint} twice to cancel");
                } else {
                    self.quit = true;
                }
                true
            }
            // Fold/unfold the most recent process entry (reasoning or tool
            // result) between its collapsed marker and its full body.
            Some(Action::ToggleFold) => self.toggle_last_process(),
            Some(Action::ScrollUp) => {
                self.scroll_up = self.scroll_up.saturating_add(self.settings.scroll_step);
                true
            }
            Some(Action::ScrollDown) => {
                self.scroll_up = self.scroll_up.saturating_sub(self.settings.scroll_step);
                true
            }
            Some(Action::Guide) => {
                self.open_guide();
                true
            }
            // Toggle the pending-queue panel. Safe mid-turn: queue mutations
            // only matter at the settle boundary, never to the live turn.
            Some(Action::QueuePanel) => {
                self.toggle_queue_panel();
                true
            }
            // Raise the read-only key reference. It changes no conversation
            // state, so unlike section navigation it is not gated on
            // `turn_active` — the operator can consult it mid-turn.
            Some(Action::Help) => {
                self.help_open = true;
                self.status_note = String::from("key reference · Esc closes");
                true
            }
            Some(Action::Submit) => {
                self.submit();
                true
            }
            // Steer (§11.2): into the live turn, not behind it. Falls back to
            // a plain submit while idle, so the chord is never a dead key.
            Some(Action::SteerSubmit) => {
                self.submit_steer();
                true
            }
            // Section navigation belongs to the shell and is handled at the
            // loop level (safe mid-turn since §11.4's section-tagged routing).
            // This arm is defense in depth for a nav chord that ever reaches
            // the reducer: consume it with a guard note rather than letting a
            // stray key fall into the editor.
            Some(Action::NextSection | Action::PrevSection | Action::NewSection) => {
                self.status_note =
                    String::from("finish or cancel the turn before switching sections");
                true
            }
            // No shell binding claims this key. Plain Up/Down at the input's
            // top/bottom edge walk history first (mirroring the REPL); every
            // other key — and arrows inside a multi-line draft — belong to the
            // editor (typing, Home/End, Ctrl+A/E, backspace, yank…).
            None => {
                if self.history_nav(key) {
                    true
                } else {
                    self.input.input(tui_input(key))
                }
            }
        }
    }

    /// Insert a bracketed-paste block as literal text. The whole block arrives
    /// as one event, so an embedded newline becomes a multi-line draft rather
    /// than a submit — the fix for "pasting text auto-runs the turn". A modal
    /// overlay swallows paste exactly as it swallows keys, so a stray block can
    /// never reach the editor behind it.
    fn on_paste(&mut self, text: String) -> bool {
        // The guide overlay's draft editor takes the paste (T6); the other
        // modal overlays swallow it exactly as they swallow keys.
        if let Some(pending) = &mut self.guide {
            return pending.editor.insert_str(text);
        }
        if self.permission.is_some() || self.help_open || self.queue_panel.is_some() {
            return false;
        }
        self.input.insert_str(text)
    }

    /// The interrupt key's display name, for status hints that must stay
    /// truthful after a remap.
    fn cancel_key_hint(&self) -> String {
        self.keymap.hint_for(Action::Interrupt)
    }

    /// The note the status bar renders this frame. The cancel states are
    /// sticky overrides derived from state — a stream event rewrites the
    /// transient note on every tool start, which used to erase the
    /// armed/cancelling hint before it could be read (KI-002) — so the hint
    /// survives exactly as long as the state it describes: until another key
    /// disarms the double-tap, or the turn settles.
    fn rendered_status_note(&self) -> Cow<'_, str> {
        if self.cancel_requested {
            Cow::Borrowed("cancelling turn…")
        } else if self.turn_active && self.ctrl_c_armed {
            Cow::Owned(format!(
                "press {} again to cancel the turn",
                self.cancel_key_hint()
            ))
        } else {
            Cow::Borrowed(&self.status_note)
        }
    }

    /// Rebuild the input bar's title from the current keymap, used after a load
    /// or hot-reload so the hint matches the live bindings. Only the block
    /// decoration changes; any text already typed is preserved.
    fn refresh_input_title(&mut self) {
        let block = input_block(&self.keymap);
        self.input.set_block(block);
    }

    /// Take the editor's text as one submission, clear the editor, and route it.
    /// Empty input is a no-op (the editor is still cleared).
    fn submit(&mut self) {
        let text = self.input.lines().join("\n");
        self.input = editor(&self.keymap);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        // Record the submission for Up/Down recall (in-memory, plus the shared
        // history file once `run_shell` has wired one). `history` and
        // `history_path` are disjoint fields, so the borrows do not conflict.
        self.history.push(&self.history_path, trimmed);
        self.route_submission(trimmed, trimmed.to_string());
    }

    /// Steer submit (§11.2): while a turn runs, the text joins the *live*
    /// conversation at its next iteration boundary (staged for the loop to
    /// push into the turn's steer mailbox) instead of waiting behind it as a
    /// follow-up. While idle there is nothing to steer — it behaves exactly
    /// like [`Self::submit`] so the chord is never a dead key.
    fn submit_steer(&mut self) {
        let text = self.input.lines().join("\n");
        self.input = editor(&self.keymap);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        self.history.push(&self.history_path, trimmed);
        if !self.model.is_running() {
            self.route_submission(trimmed, trimmed.to_string());
            return;
        }
        self.scroll_up = 0;
        self.push_line(Line::from(Span::styled(
            format!("❯ {trimmed} (steering)"),
            Style::default().fg(Theme::current().muted().ratatui()),
        )));
        if self
            .model
            .queue_mut()
            .send_with_mode(trimmed, QueueSource::User, DeliveryMode::Steer)
            .is_some()
        {
            self.pending_steer = Some(trimmed.to_string());
            self.status_note = String::from("steering the running turn · joins at the next step");
        } else {
            self.status_note = String::from("follow-up queue is full · withdraw one first");
        }
    }

    /// Up/Down history recall at the input buffer's edges, mirroring the
    /// blocking REPL. `Up` on the top line loads an older entry; `Down` on the
    /// bottom line loads a newer one. Inside a multi-line draft (or on a
    /// modified arrow) this returns `false` so the key falls through to the
    /// editor and just moves the cursor. Unlike a naive mirror, `Down` past the
    /// newest entry is a no-op — it never blanks a live draft. Returns whether
    /// the key was consumed as history navigation.
    fn history_nav(&mut self, key: KeyEvent) -> bool {
        // A modified arrow (ctrl/alt/super) is an editor motion, not history.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return false;
        }
        let (row, _) = self.input.cursor();
        let last_row = self.input.lines().len().saturating_sub(1);
        match key.code {
            KeyCode::Up if row == 0 => match self.history.prev().map(str::to_string) {
                Some(entry) => {
                    let loaded = editor_with_text(&self.keymap, &entry);
                    self.input = loaded;
                    true
                }
                None => false,
            },
            KeyCode::Down if row >= last_row => match self.history.next().map(str::to_string) {
                Some(entry) => {
                    let loaded = editor_with_text(&self.keymap, &entry);
                    self.input = loaded;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// Route one submission. `label` is what the transcript echoes; `body` is
    /// what actually runs. They differ for a guide draft, whose label names the
    /// task while the body is the assembled three-part injection. Routing
    /// mirrors the blocking REPL (`main.rs`): while a turn runs, the body is
    /// echoed as *queued* and pushed into the shared follow-up queue (never
    /// interrupting the turn); the loop merges that queue into one injection at
    /// the turn boundary. While idle it is staged to run as the next turn.
    fn route_submission(&mut self, label: &str, body: String) {
        self.scroll_up = 0;
        if self.model.is_running() {
            self.push_line(Line::from(Span::styled(
                format!("❯ {label} (queued)"),
                Style::default().fg(Theme::current().muted().ratatui()),
            )));
            self.status_note = if self.model.enqueue(&body) {
                format!(
                    "{} follow-up(s) queued · merged after this turn",
                    self.model.queue().len()
                )
            } else {
                String::from("follow-up queue is full · withdraw one first")
            };
            return;
        }
        self.push_line(Line::from(Span::styled(
            format!("❯ {label}"),
            Style::default().fg(Theme::current().accent().ratatui()),
        )));
        self.pending_submit = Some(body);
    }

    /// Open the guide overlay from the current input text: capture it as the
    /// next task, fold it with the [`GuideContext`] snapshot into a draft, and
    /// preview that draft. The draft is built once here so what is previewed is
    /// exactly what [`Self::guide_key`] sends on confirm. Empty input hints
    /// instead of opening, so Ctrl+G never previews a task-less draft.
    fn open_guide(&mut self) {
        let task = self.input.lines().join("\n");
        let trimmed = task.trim();
        if trimmed.is_empty() {
            self.status_note = String::from("type the next task first, then Ctrl+G to guide it");
            return;
        }
        // T6 two-column confirm: the draft loads into an editable editor, so
        // the operator can reshape the assembled injection before sending —
        // what the editor holds at confirm time is exactly what is sent.
        let draft = guide_draft(&self.guide_context, trimmed);
        let mut editor = TextArea::from(draft.split('\n').map(str::to_string).collect::<Vec<_>>());
        editor.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title(" draft · edit freely · Enter sends ")
                .border_style(Style::default().fg(Theme::current().accent().ratatui())),
        );
        self.guide = Some(PendingGuide {
            task: trimmed.to_string(),
            editor,
        });
        self.status_note =
            String::from("guide draft ready · edit · Enter sends · Esc keeps the task");
    }

    /// Keys while the guide overlay is raised. Enter confirms (routes the
    /// *edited* draft as a submission, clearing the input); Esc or Ctrl+C
    /// cancels and leaves the task in the input for editing. Every other key
    /// edits the draft in place (T6), so stray input never reaches the editor
    /// behind the overlay.
    fn guide_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
            self.guide = None;
            self.status_note = String::from("guide cancelled · the task stays in the input");
            return true;
        }
        if key.code == KeyCode::Enter {
            if let Some(pending) = self.guide.take() {
                let label = format!("guide: {}", pending.task);
                let body = pending.editor.lines().join("\n");
                self.input = editor(&self.keymap);
                self.route_submission(&label, body);
            }
            return true;
        }
        match &mut self.guide {
            Some(pending) => pending.editor.input(tui_input(key)),
            None => false,
        }
    }

    /// Keys while the key-reference overlay is raised. Esc or the help binding
    /// itself closes it; every other key is swallowed so stray input cannot
    /// reach the editor behind the overlay. Read-only, so nothing else changes
    /// and a swallowed key needs no repaint.
    fn help_key(&mut self, key: KeyEvent) -> bool {
        let closes =
            key.code == KeyCode::Esc || matches!(self.keymap.resolve(&key), Some(Action::Help));
        if closes {
            self.help_open = false;
            self.status_note = String::from("key reference closed");
            return true;
        }
        false
    }

    /// Toggle the queue panel (§11.1 P-C). Opening on an empty queue hints
    /// instead, so the chord teaches rather than pops a dead panel.
    fn toggle_queue_panel(&mut self) {
        if self.queue_panel.is_some() {
            self.queue_panel = None;
            self.status_note = String::from("queue panel closed");
            return;
        }
        if self.model.queue().is_empty() {
            self.status_note = String::from("queue is empty · submits during a turn queue here");
            return;
        }
        self.queue_panel = Some(QueuePanel::new());
        self.status_note =
            String::from("queue panel · ↑/↓ select · e edit · x withdraw · +/- reorder · l lane");
    }

    /// Keys while the queue panel is raised. The rows are the *live* pending
    /// queue; every mutation re-clamps the selection so it always sits on a
    /// real entry. Swallows every key so chords never reach the editor.
    fn queue_panel_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let closes = key.code == KeyCode::Esc
            || (ctrl && key.code == KeyCode::Char('c'))
            || matches!(self.keymap.resolve(&key), Some(Action::QueuePanel));
        if closes {
            self.queue_panel = None;
            self.status_note = String::from("queue panel closed");
            return true;
        }
        let len = self.model.queue().len();
        let selected_id = match self.queue_panel.as_ref() {
            Some(panel) => self
                .model
                .queue()
                .items()
                .get(panel.selected.min(len.saturating_sub(1)))
                .map(|entry| entry.id),
            // Only `on_key` calls here with the panel raised; treat any other
            // entry as a no-op rather than inventing a selection.
            None => None,
        };
        let Some(id) = selected_id else {
            // The queue drained underneath the panel (settle-time delivery);
            // there is nothing left to edit.
            self.queue_panel = None;
            self.status_note = String::from("queue drained · panel closed");
            return true;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(panel) = &mut self.queue_panel {
                    panel.selected = panel.selected.saturating_sub(1);
                }
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(panel) = &mut self.queue_panel {
                    panel.selected = (panel.selected + 1).min(len.saturating_sub(1));
                }
                true
            }
            // Edit: pull the entry out and park its text in the input editor
            // for rework (D2.4 "取出回输入框回炉").
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(text) = self.model.queue_mut().withdraw(id) {
                    self.input = editor_with_text(&self.keymap, &text);
                    self.status_note = format!("entry #{id} back in the input · edit and resend");
                }
                self.close_panel_if_empty();
                true
            }
            // Withdraw without the round trip to the input (the ledger keeps
            // the text visible under the panel's recent section).
            KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('x') | KeyCode::Char('d') => {
                if self.model.queue_mut().withdraw(id).is_some() {
                    self.status_note = format!("entry #{id} withdrawn");
                }
                self.close_panel_if_empty();
                true
            }
            // Reorder towards earlier / later delivery.
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.model.queue_mut().move_entry(id, true);
                self.panel_clamp();
                true
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                self.model.queue_mut().move_entry(id, false);
                self.panel_clamp();
                true
            }
            // Re-lane: cycle main → existing lanes (D2.4 改派). New lanes are
            // born via `/queue send --lane`; the panel can only move an entry
            // among lanes that already exist.
            KeyCode::Char('l') => {
                let mut lanes = vec![crate::core::MAIN_LANE.to_string()];
                lanes.extend(self.model.queue().lane_names());
                let current = self
                    .model
                    .queue()
                    .items()
                    .iter()
                    .find(|entry| entry.id == id)
                    .map(|entry| entry.lane.clone())
                    .unwrap_or_else(|| crate::core::MAIN_LANE.to_string());
                let next = match lanes.iter().position(|lane| *lane == current) {
                    Some(index) => lanes[(index + 1) % lanes.len()].clone(),
                    None => lanes[0].clone(),
                };
                if self.model.queue_mut().reassign_lane(id, &next) {
                    self.status_note = format!("entry #{id} → lane {next}");
                }
                true
            }
            _ => false,
        }
    }

    fn close_panel_if_empty(&mut self) {
        if self.model.queue().is_empty() {
            self.queue_panel = None;
        } else {
            self.panel_clamp();
        }
    }

    fn panel_clamp(&mut self) {
        if let Some(panel) = &mut self.queue_panel {
            panel.clamp(self.model.queue().len());
        }
    }

    /// The pure `AgentEvent -> transcript` reducer. This is the heart of the
    /// carrier layer and is exercised directly by unit tests without a
    /// terminal.
    fn apply_event(&mut self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::TextDelta(delta) => {
                self.assistant_buf.push_str(delta);
                true
            }
            AgentEvent::ThinkingDelta(delta) => {
                self.thinking_buf.push_str(delta);
                true
            }
            AgentEvent::ToolUse { name, .. } => {
                // A tool call ends the current assistant segment; flush what
                // streamed so far so ordering in the transcript stays honest.
                self.flush_assistant();
                self.tool_step += 1;
                self.running_tool = Some(name.clone());
                self.status_note = format!("tool {}: `{name}`", self.tool_step);
                true
            }
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
                ..
            } => {
                self.running_tool = None;
                self.push_tool(name.clone(), *is_error, output);
                true
            }
            AgentEvent::Usage(usage) => {
                self.last_usage = Some(*usage);
                true
            }
            AgentEvent::MessageStop => {
                self.flush_assistant();
                true
            }
            AgentEvent::Truncated(reason) => {
                self.push_line(muted_line(format!(
                    "· truncated by the provider ({reason})"
                )));
                true
            }
            AgentEvent::Steered(text) => {
                // The live turn absorbed the steer; claim its queue entry (by
                // oldest equal text) so the ledger records the delivery and
                // the queued count drops. An unclaimed event means the entry
                // was withdrawn mid-flight — nothing to do.
                self.model.queue_mut().claim_steered(text);
                self.push_line(muted_line(format!("· steered: {text}")));
                true
            }
            AgentEvent::Error { message, hint } => {
                self.push_line(Line::from(Span::styled(
                    format!("✘ {message}"),
                    Style::default().fg(Theme::current().error().ratatui()),
                )));
                if let Some(h) = hint {
                    self.push_line(Line::from(Span::styled(
                        format!("  hint: {h}"),
                        Style::default()
                            .fg(Theme::current().muted().ratatui())
                            .add_modifier(Modifier::DIM),
                    )));
                }
                true
            }
        }
    }

    /// End the live turn: flush any dangling stream, drop the running marker,
    /// and record the outcome in the status bar.
    fn finish_turn(&mut self, ok: bool, note: &str) {
        self.flush_assistant();
        self.turn_active = false;
        self.running_tool = None;
        self.ctrl_c_armed = false;
        self.cancel_requested = false;
        self.status_note = if ok {
            if note.is_empty() {
                String::from("idle")
            } else {
                note.to_string()
            }
        } else {
            format!("turn failed · {note}")
        };
    }

    /// Move the streamed reasoning + assistant buffers into the transcript.
    /// Reasoning is process, so it lands folded *before* the answer it
    /// produced; the answer is parsed once into the shared markdown IR and
    /// projected to styled ratatui rows (A2b), so the transcript renders real
    /// markdown instead of plain text. Both buffers drain here so a later flush
    /// in the same turn never duplicates them.
    fn flush_assistant(&mut self) {
        let thinking = std::mem::take(&mut self.thinking_buf);
        if !thinking.trim().is_empty() {
            self.transcript.push(Item::Thinking {
                text: thinking,
                // Reasoning is process, so it defaults to folded; a
                // `fold_thinking = false` setting opens it inline instead.
                expanded: !self.settings.fold_thinking,
            });
            self.transcript_version += 1;
        }
        if self.assistant_buf.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.assistant_buf);
        // Parse + project once at the segment boundary (never per frame): the
        // resulting owned rows are stored and re-borrowed on each redraw.
        let nodes = markdown::parse(&text);
        for line in markdown::project_ratatui(&nodes, &Theme::current()) {
            self.push_line(line);
        }
    }

    /// Append one finished plain line to the transcript.
    fn push_line(&mut self, line: Line<'static>) {
        self.transcript.push(Item::Line(line));
        self.transcript_version += 1;
    }

    /// Record a tool result as a foldable transcript item. Short outputs start
    /// expanded; longer ones collapse to a one-line marker (Tab toggles).
    fn push_tool(&mut self, name: String, is_error: bool, output: &str) {
        let lines: Vec<String> = output
            .trim_end_matches('\n')
            .lines()
            .map(str::to_string)
            .collect();
        let expanded = lines.len() <= self.settings.tool_inline_lines;
        self.transcript.push(Item::Tool {
            name,
            is_error,
            output: lines,
            expanded,
        });
        self.transcript_version += 1;
    }

    /// Toggle the most recent process entry's fold (reasoning or tool result).
    /// Returns whether anything changed, so a transcript with no process
    /// entries is a no-op (no repaint).
    fn toggle_last_process(&mut self) -> bool {
        let Some(item) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|item| item.is_process())
        else {
            return false;
        };
        let changed = item.toggle();
        if changed {
            self.transcript_version += 1;
        }
        changed
    }

    /// Drain a staged submission, if any.
    fn take_submit(&mut self) -> Option<String> {
        self.pending_submit.take()
    }

    /// Drain a staged steer (§11.2); the loop forwards it into the live
    /// turn's mailbox.
    fn take_steer(&mut self) -> Option<String> {
        self.pending_steer.take()
    }

    /// Drain a staged cancel request.
    fn take_cancel(&mut self) -> bool {
        std::mem::take(&mut self.pending_cancel)
    }

    /// Route a key to the permission overlay: Up/Down (or k/j/Tab) move the
    /// selection, Enter confirms it, Esc/Ctrl+C deny. Resolving the overlay
    /// stages a [`PermissionChoice`] for the loop to return to the prompter.
    fn permission_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || (ctrl && matches!(key.code, KeyCode::Char('c'))) {
            self.permission = None;
            self.permission_choice = Some(PermissionChoice::Deny);
            self.status_note = String::from("permission denied");
            return true;
        }
        let Some(pending) = self.permission.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                pending.selected = pending.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                pending.selected = (pending.selected + 1) % pending.options.len();
            }
            KeyCode::Enter => {
                let choice = match pending.selected {
                    0 => PermissionChoice::AllowOnce,
                    1 => PermissionChoice::AllowAll,
                    _ => PermissionChoice::Deny,
                };
                self.permission_choice = Some(choice);
                self.permission = None;
                self.status_note = if matches!(choice, PermissionChoice::Deny) {
                    String::from("permission denied")
                } else {
                    String::from("permission granted")
                };
            }
            _ => {}
        }
        true
    }

    /// Drain the overlay's resolved choice, if any.
    fn take_permission_choice(&mut self) -> Option<PermissionChoice> {
        self.permission_choice.take()
    }
}

/// One transcript entry. Output entries (`Line`) are the answer and always
/// show; process entries (`Thinking`, `Tool`) are the machinery that produced
/// it and fold to a one-row marker so the real output stands out. Keeping
/// process bodies structured (rather than pre-flattened into `Line`s) is what
/// makes folding a pure view toggle that never loses text.
enum Item {
    /// A finished output line: submission echo, notice, or flushed assistant text.
    Line(Line<'static>),
    /// Folded reasoning that preceded an answer. Process, not output, so it
    /// starts collapsed; `text` retains the full body so `expanded` can flip
    /// without re-fetching.
    Thinking { text: String, expanded: bool },
    /// A tool result. `output` retains the full body so `expanded` can flip
    /// without re-fetching; `expanded` seeds from the length heuristic.
    Tool {
        name: String,
        is_error: bool,
        output: Vec<String>,
        expanded: bool,
    },
}

impl Item {
    /// Process entries (reasoning, tool results) fold; output lines do not.
    fn is_process(&self) -> bool {
        matches!(self, Item::Thinking { .. } | Item::Tool { .. })
    }

    /// Flip this entry's fold. Only process entries fold, so an output line is
    /// a no-op returning `false`.
    fn toggle(&mut self) -> bool {
        match self {
            Item::Thinking { expanded, .. } | Item::Tool { expanded, .. } => {
                *expanded = !*expanded;
                true
            }
            Item::Line(_) => false,
        }
    }
}

/// Re-borrow a stored `'static` transcript line for one frame without cloning
/// its strings. The transcript owns its `Line<'static>`; the per-frame
/// projection only reads it, so borrowing keeps a redraw allocation-free
/// instead of deep-cloning every span (A3).
fn reborrow_line<'a>(line: &'a Line<'static>) -> Line<'a> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|span| {
                let content: &str = &span.content;
                Span {
                    style: span.style,
                    content: Cow::Borrowed(content),
                }
            })
            .collect(),
    }
}

/// Flatten transcript items into owned display lines: a `Line` is one row; a
/// `Tool` is its marker row plus, when expanded, its indented body rows. The
/// result is `'static` so it can be memoized in [`App::render_cache`]; a redraw
/// then re-borrows only the visible window from that cache instead of
/// re-projecting the whole transcript every frame.
fn render_items(items: &[Item]) -> Vec<Line<'static>> {
    let theme = Theme::current();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::Line(line) => out.push(line.clone()),
            Item::Thinking { text, expanded } => {
                let process = Style::default()
                    .fg(theme.muted().ratatui())
                    .add_modifier(Modifier::DIM | Modifier::ITALIC);
                let n = text.lines().count();
                let hint = match (*expanded, n) {
                    (_, 0) => String::new(),
                    (true, n) => format!("  ▾ {n} lines (Tab collapse)"),
                    (false, n) => format!("  ▸ {n} lines (Tab expand)"),
                };
                out.push(Line::from(Span::styled(
                    format!("· thinking{hint}"),
                    process,
                )));
                if *expanded {
                    for line in text.lines() {
                        out.push(Line::from(Span::styled(format!("    {line}"), process)));
                    }
                }
            }
            Item::Tool {
                name,
                is_error,
                output,
                expanded,
            } => {
                let head = if *is_error {
                    format!("✘ `{name}` failed")
                } else {
                    format!("· `{name}` done")
                };
                let hint = match (*expanded, output.len()) {
                    (_, 0) => String::new(),
                    (true, n) => format!("  ▾ {n} lines (Tab collapse)"),
                    (false, n) => format!("  ▸ {n} lines (Tab expand)"),
                };
                let style = if *is_error {
                    Style::default().fg(theme.error().ratatui())
                } else {
                    Style::default().fg(theme.muted().ratatui())
                };
                out.push(Line::from(Span::styled(format!("{head}{hint}"), style)));
                if *expanded {
                    for line in output {
                        out.push(Line::from(Span::styled(
                            format!("    {line}"),
                            Style::default()
                                .fg(theme.muted().ratatui())
                                .add_modifier(Modifier::DIM),
                        )));
                    }
                }
            }
        }
    }
    out
}

/// Composed-row height of one source line at `width`, measured with ratatui's
/// own `WordWrapper` (via `Paragraph::line_count`) so the scroll math matches
/// exactly what the transcript renderer draws. Zero width composes to nothing.
/// Measuring a single line at a time is what lets [`visible_window`] stay
/// O(visible rows) instead of composing the whole transcript every frame.
fn wrapped_height(line: &Line, width: u16) -> usize {
    if width < 1 {
        return 0;
    }
    // No block on the measuring paragraph: `line_count` would otherwise add the
    // border rows we are not measuring here.
    Paragraph::new(Text::from(line.clone()))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Locate the visible window inside a variable-height line list, counting from
/// the bottom where the newest content lives.
///
/// Returns `(start_src, sub_offset)`: the source index to slice from, and the
/// number of composed rows to skip *within* that first source line. `height_at`
/// lazily yields each source line's composed height, so near the bottom this
/// touches only the visible rows; scrolling past the top clamps to `(0, 0)`.
/// Pure and allocation-free, so it is unit-testable without a terminal.
///
/// `scroll_up` is in composed rows: 0 pins the newest row to the viewport
/// bottom, each unit lifts the window one wrapped row.
fn visible_window<F>(
    len: usize,
    height_at: F,
    body_height: usize,
    scroll_up: usize,
) -> (usize, usize)
where
    F: Fn(usize) -> usize,
{
    // Rows the window must span from the bottom: the visible body plus how
    // far we have scrolled up above it.
    let need = scroll_up.saturating_add(body_height);
    let mut acc = 0usize;
    for i in (0..len).rev() {
        // A blank source line still occupies one composed row, so the walk
        // never stalls on it.
        let h = height_at(i).max(1);
        if acc + h >= need {
            // This source line straddles the window's top edge; skip the rows
            // of it that sit above the viewport.
            return (i, (acc + h).saturating_sub(need));
        }
        acc += h;
    }
    // Scrolled further up than the transcript is tall: clamp to the very top.
    (0, 0)
}

/// A dimmed secondary line (notices, markers).
fn muted_line(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default()
            .fg(Theme::current().muted().ratatui())
            .add_modifier(Modifier::DIM),
    ))
}

/// The input editor's border + title block. The hint lists the *bound* keys, so
/// it stays truthful after a `keymap.toml` remap instead of naming dead keys.
fn input_block(keymap: &Keymap) -> Block<'static> {
    Block::default().borders(Borders::ALL).title(format!(
        " input  ({} submit · shift+enter newline · {} fold · {}/{} scroll · {} keys · {} quit) ",
        keymap.hint_for(Action::Submit),
        keymap.hint_for(Action::ToggleFold),
        keymap.hint_for(Action::ScrollUp),
        keymap.hint_for(Action::ScrollDown),
        keymap.hint_for(Action::Help),
        keymap.hint_for(Action::Interrupt),
    ))
}

/// Apply the shared border/title + placeholder decoration to an input editor.
/// The placeholder advertises only affordances the full-screen shell actually
/// implements: the key reference (through its *bound* key, so it stays truthful
/// after a remap) and `@path` image attachment (expanded by `run_one_turn`). It
/// deliberately omits slash commands — the shell runs every submission as a
/// turn and dispatches none, so naming them here would be a dead hint.
fn decorate(mut input: TextArea<'static>, keymap: &Keymap) -> TextArea<'static> {
    input.set_block(input_block(keymap));
    input.set_placeholder_text(format!(
        "Ask anything… · {} for keys · @path attaches an image",
        keymap.hint_for(Action::Help)
    ));
    input.set_placeholder_style(
        Style::default()
            .fg(Theme::current().muted().ratatui())
            .add_modifier(Modifier::DIM),
    );
    input
}

/// A fresh, empty input editor with the shared decoration. Recreating is the
/// cheapest reliable clear across tui-textarea versions.
fn editor(keymap: &Keymap) -> TextArea<'static> {
    decorate(TextArea::default(), keymap)
}

/// An input editor preloaded with `text` (a recalled history entry), cursor
/// parked at the end. Newlines are split into rows so a multi-line entry loads
/// faithfully, matching the REPL's `textarea_from`.
fn editor_with_text(keymap: &Keymap, text: &str) -> TextArea<'static> {
    let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut input = decorate(TextArea::from(lines), keymap);
    input.move_cursor(CursorMove::Bottom);
    input.move_cursor(CursorMove::End);
    input
}

/// Upper bound on open sections. Each section carries its own runtime, MCP
/// connections and session file, so the cap keeps a stray key from spawning
/// unbounded resources; nine single-digit tabs also keep the bar readable.
const MAX_SECTIONS: usize = 9;

/// Process-wide header context. Every section shares one provider selection, so
/// the model, working directory and context budget are shell-level, not
/// per-section. `Default` keeps `Shell::new()` (and the reducer tests) hermetic;
/// `run_shell` fills these from the live selection.
#[derive(Clone, Debug, Default)]
pub(crate) struct Chrome {
    /// Active model name, shown in the header.
    pub(crate) model: String,
    /// Working directory (home-shortened), shown in the header.
    pub(crate) cwd: String,
    /// Context window in tokens; `0` disables the budget readout.
    pub(crate) context_window: usize,
}

/// The multi-section shell: a list of independent conversation sections plus
/// which one is active. Each section is a self-contained [`App`] with its own
/// transcript, input, follow-up queue and turn state, so sections never share
/// conversation data. The shell owns the header/tab bar and section navigation;
/// a section owns everything drawn inside its body.
pub(crate) struct Shell {
    sections: Vec<App>,
    active: usize,
    /// Header context (model · cwd · context budget), shared by all sections.
    chrome: Chrome,
    /// Where the §11.4 rotation scan resumes. Sections are the delivery
    /// swimlanes (D2.1's verdict): when the loop goes idle and a fresh submit
    /// is not waiting, pending queue entries run one turn at a time,
    /// round-robin across the sections that have them, so work queued in a
    /// background section progresses even while another section is on screen.
    /// The cursor advances past each section the moment it is served, which
    /// is what makes the rotation fair rather than front-loaded.
    delivery_cursor: usize,
}

impl Shell {
    /// A shell with a single `main` section and empty header context.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            sections: vec![App::new()],
            active: 0,
            chrome: Chrome::default(),
            delivery_cursor: 0,
        }
    }

    /// The clamped active index. Invariant: `sections` is never empty and
    /// `active` stays in range, so the `min` only guards a stale index from
    /// panicking the render loop. The event loop reads it to pick the matching
    /// runtime/state from its parallel vectors.
    fn active_index(&self) -> usize {
        self.active.min(self.sections.len().saturating_sub(1))
    }

    /// The active section.
    fn section(&self) -> &App {
        let idx = self.active_index();
        &self.sections[idx]
    }

    /// The active section, mutably.
    fn section_mut(&mut self) -> &mut App {
        let idx = self.active_index();
        &mut self.sections[idx]
    }

    /// The section at `idx`, clamped like [`Self::active_index`]. The turn
    /// settle phase routes to the section whose turn just finished — identical
    /// to the active one while nav is blocked mid-turn, but carried explicitly
    /// so the routing never depends on that invariant.
    fn section_index_mut(&mut self, idx: usize) -> &mut App {
        let clamped = idx.min(self.sections.len().saturating_sub(1));
        &mut self.sections[clamped]
    }

    /// The next section the rotation should serve (§11.4): scan from just
    /// past the cursor, wrapping, and stop at the first idle section holding
    /// at least one deliverable (non-Steer) pending entry. `None` when no
    /// section has queued work. Delivery *within* a section stays arrival
    /// order — lanes remain an observation/editing dimension (D2.1/D2.2
    /// discipline); the rotation is across sections.
    fn next_queue_section(&self) -> Option<usize> {
        let len = self.sections.len();
        if len == 0 {
            return None;
        }
        let cursor = self.delivery_cursor % len;
        for offset in 0..len {
            let index = (cursor + offset) % len;
            let section = &self.sections[index];
            if !section.model.is_running()
                && section
                    .model
                    .queue()
                    .items()
                    .iter()
                    .any(|entry| entry.mode != crate::core::DeliveryMode::Steer)
            {
                return Some(index);
            }
        }
        None
    }

    /// Advance to the next section, wrapping at the end. A no-op with a single
    /// section so the tab bar never flickers on a solo session.
    fn next_section(&mut self) {
        let n = self.sections.len();
        if n > 1 {
            self.active = (self.active_index() + 1) % n;
        }
    }

    /// Step back to the previous section, wrapping at the start.
    fn prev_section(&mut self) {
        let n = self.sections.len();
        if n > 1 {
            self.active = (self.active_index() + n - 1) % n;
        }
    }

    /// Append a freshly titled section and make it active. The caller pushes the
    /// matching runtime and state so the three vectors stay index-aligned.
    fn push_section(&mut self, title: String) {
        self.sections.push(App::titled(title));
        self.active = self.sections.len().saturating_sub(1);
    }

    /// Pure projection: a context header across the top, then (only when more
    /// than one section is open) the tab bar, then the active section's body
    /// below. A solo session shows no lonely tab; the `*` marker externalizes
    /// which section has a live turn so a running turn stays visible from the
    /// bar.
    fn view(&self, frame: &mut Frame) {
        let theme = Theme::current();
        let section = self.section();
        let show_tabs = self.sections.len() > 1;

        let mut constraints = vec![Constraint::Length(1)]; // header
        if show_tabs {
            constraints.push(Constraint::Length(1)); // tab bar
        }
        constraints.push(Constraint::Min(1)); // body
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());

        self.render_header(areas[0], section, frame, &theme);

        let body = if show_tabs {
            let titles: Vec<Line> = self
                .sections
                .iter()
                .map(|section| {
                    let marker = if section.turn_active { "*" } else { "" };
                    Line::from(Span::raw(format!("{marker}{}", section.title)))
                })
                .collect();
            let tabs = Tabs::new(titles)
                .select(self.active)
                .divider(Span::styled(
                    " │ ",
                    Style::default().fg(theme.muted().ratatui()),
                ))
                .highlight_style(
                    Style::default()
                        .fg(theme.accent().ratatui())
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_widget(tabs, areas[1]);
            areas[2]
        } else {
            areas[1]
        };
        view(section, body, frame, Some(self.chrome.context_window));
    }

    /// The top context bar: identity (`heartflow · model · cwd`) on the left,
    /// the context-budget readout right-aligned. Splitting the row into two
    /// cells lets the budget hug the right edge without padding math.
    fn render_header(&self, area: Rect, section: &App, frame: &mut Frame, theme: &Theme) {
        let budget = self.context_budget(section, theme);
        let right_w = budget
            .as_ref()
            .map_or(0, |(text, _)| text.chars().count() as u16);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(right_w)])
            .split(area);

        let mut spans = vec![Span::styled(
            "heartflow",
            Style::default()
                .fg(theme.heading().ratatui())
                .add_modifier(Modifier::BOLD),
        )];
        let sep = Span::styled(" · ", Style::default().fg(theme.muted().ratatui()));
        if !self.chrome.model.is_empty() {
            spans.push(sep.clone());
            spans.push(Span::styled(
                self.chrome.model.clone(),
                Style::default().fg(theme.accent().ratatui()),
            ));
        }
        if !self.chrome.cwd.is_empty() {
            spans.push(sep);
            spans.push(Span::styled(
                self.chrome.cwd.clone(),
                Style::default().fg(theme.muted().ratatui()),
            ));
        }
        frame.render_widget(Line::from(spans), cols[0]);
        if let Some((text, style)) = budget {
            frame.render_widget(
                Line::from(Span::styled(text, style)).alignment(Alignment::Right),
                cols[1],
            );
        }
    }

    /// Context-budget readout `[ctx NN%]` from the active section's last usage
    /// sample against the configured window. The prompt footprint is the
    /// Anthropic total input (fresh + cache-read + cache-write). Colored by
    /// pressure: success under half, accent to 80%, error at/over the
    /// compaction threshold. `None` when no window is configured or no turn has
    /// reported usage yet.
    fn context_budget(&self, section: &App, theme: &Theme) -> Option<(String, Style)> {
        if self.chrome.context_window == 0 {
            return None;
        }
        let usage = section.last_usage?;
        let used = usize::try_from(
            usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens),
        )
        .unwrap_or(usize::MAX);
        let pct = (used * 100 / self.chrome.context_window).min(999);
        let color = if pct >= 80 {
            theme.error()
        } else if pct >= 50 {
            theme.accent()
        } else {
            theme.success()
        };
        Some((
            format!("[ctx {pct}%]"),
            Style::default().fg(color.ratatui()),
        ))
    }
}

/// Pure projection of one section onto `area`: transcript, input editor, status
/// bar. The shell renders the tab bar above this and hands the active section
/// the body below it, so a section never draws its own chrome. Overlays are
/// modal and float over the whole frame.
fn view(app: &App, area: Rect, frame: &mut Frame, context_window: Option<usize>) {
    let theme = Theme::current();
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // transcript
            Constraint::Length(3), // input editor (with its border)
            Constraint::Length(1), // status bar
        ])
        .split(area);

    // -- Center: scrollable transcript --------------------------------------
    // Finished history (memoized) plus a synthetic live tail (streaming text,
    // thinking, running-tool marker). The transcript projection is cached and
    // rebuilt only when the transcript or the palette changes, so a streaming
    // redraw — which grows just the tail, never the frozen transcript — reuses
    // it. `visible_window` then measures wrapped heights over the virtual
    // concatenation `cache ⧺ tail` from the bottom and the paragraph is handed
    // only the rows that can show, so a frame is O(visible), not O(transcript).
    let body_height = areas[0].height.saturating_sub(2) as usize; // minus block borders
    let key = (app.transcript_version, Theme::generation());
    if app.render_key.get() != key {
        *app.render_cache.borrow_mut() = render_items(&app.transcript);
        app.render_key.set(key);
    }
    let cache = app.render_cache.borrow();

    // The live tail is small and rebuilt every frame; it is never cached because
    // it changes on each streaming delta.
    let mut tail: Vec<Line<'static>> = Vec::new();
    if app.turn_active {
        if !app.thinking_buf.is_empty() {
            tail.push(Line::from(Span::styled(
                app.thinking_buf.clone(),
                Style::default()
                    .fg(theme.muted().ratatui())
                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
            )));
        }
        if !app.assistant_buf.is_empty() {
            tail.push(Line::from(Span::raw(app.assistant_buf.clone())));
        }
        if let Some(tool) = &app.running_tool {
            tail.push(Line::from(Span::styled(
                format!("· running `{tool}`"),
                Style::default().fg(theme.accent().ratatui()),
            )));
        }
    }

    // Visible-window virtualization over `cache ⧺ tail`: measure wrapped heights
    // from the bottom with ratatui's own composer, then materialize only the
    // rows that can show. Counting composed rows (not source lines) is what
    // keeps the newest wrapped output pinned to the viewport bottom.
    let inner_width = areas[0].width.saturating_sub(2); // minus block borders
    let cache_len = cache.len();
    let (start_src, sub_offset) = visible_window(
        cache_len + tail.len(),
        |i| {
            let line = if i < cache_len {
                &cache[i]
            } else {
                &tail[i - cache_len]
            };
            wrapped_height(line, inner_width)
        },
        body_height,
        usize::from(app.scroll_up),
    );
    let window: Vec<Line> = if start_src < cache_len {
        cache[start_src..]
            .iter()
            .map(reborrow_line)
            .chain(tail.iter().map(reborrow_line))
            .collect()
    } else {
        tail[start_src - cache_len..]
            .iter()
            .map(reborrow_line)
            .collect()
    };
    let transcript = Paragraph::new(Text::from(window))
        .wrap(Wrap { trim: false })
        .scroll((sub_offset as u16, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" transcript ")
                .border_style(Style::default().fg(theme.muted().ratatui())),
        );
    frame.render_widget(transcript, areas[0]);

    // -- Bottom: input editor + status bar ----------------------------------
    frame.render_widget(&app.input, areas[1]);
    // Park the real cursor at the editor's caret so the terminal draws IME
    // candidates in the right place (native composition, never self-drawn).
    let (row, col) = app.input.cursor();
    let caret_x = areas[1].x.saturating_add(1).saturating_add(col as u16);
    let caret_y = areas[1].y.saturating_add(1).saturating_add(row as u16);
    frame.set_cursor_position((caret_x, caret_y));

    let mut status_spans = vec![Span::styled(
        if app.turn_active { " ● " } else { " ○ " },
        Style::default().fg(if app.turn_active {
            theme.accent().ratatui()
        } else {
            theme.success().ratatui()
        }),
    )];
    let status_note = app.rendered_status_note();
    status_spans.push(Span::styled(
        status_note.as_ref(),
        Style::default().fg(theme.muted().ratatui()),
    ));
    if let Some(usage) = app.last_usage {
        status_spans.push(Span::styled(
            format!(
                "  [in {} / out {} / cache {}]",
                usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
            ),
            Style::default()
                .fg(theme.muted().ratatui())
                .add_modifier(Modifier::DIM),
        ));
        // Context-pressure warning: when cumulative usage exceeds 70% of the
        // window, show a soft hint; past 90% make it loud. Helps users notice
        // they're approaching a compaction cliff before the model starts
        // truncating context silently.
        if let Some(window) = context_window {
            // Same accounting as the header's context budget above: all input
            // (fresh + cache read + cache creation), excluding output. Using
            // raw `input_tokens` alone misses cache hits, which dominate
            // multi-turn sessions and would make the warning never fire.
            let total = usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens);
            let pct = if window > 0 {
                f64::from(total) / window as f64
            } else {
                0.0
            };
            if pct >= 0.9 {
                status_spans.push(Span::styled(
                    format!("  [context: {:.0}% — near limit]", pct * 100.0),
                    Style::default()
                        .fg(theme.error().ratatui())
                        .add_modifier(Modifier::BOLD),
                ));
            } else if pct >= 0.7 {
                status_spans.push(Span::styled(
                    format!("  [context: {:.0}%]", pct * 100.0),
                    Style::default().fg(theme.accent().ratatui()),
                ));
            }
        }
    }
    let queued = app.model.queue().len();
    if queued > 0 {
        // Split the count by delivery mode (§11.1 P-C): a steer in flight is
        // a different promise than a follow-up waiting its turn.
        let steering = app
            .model
            .queue()
            .items()
            .iter()
            .filter(|entry| entry.mode == crate::core::DeliveryMode::Steer)
            .count();
        let label = if steering > 0 {
            format!("  [{} queued · {} steering]", queued - steering, steering)
        } else {
            format!("  [{queued} queued]")
        };
        status_spans.push(Span::styled(
            label,
            Style::default().fg(theme.accent().ratatui()),
        ));
    }
    frame.render_widget(Line::from(status_spans), areas[2]);

    // Overlays float above every region. Layering, bottom to top: the read-only
    // key reference, then the queue panel, then the guide draft, then the
    // permission popup, so an approval always wins the topmost layer.
    render_help_overlay(app, frame);
    render_queue_overlay(app, frame);
    render_guide_overlay(app, frame);
    render_permission_overlay(app, frame);
}

/// Draw the centered approval popup when a tool is waiting on a decision. The
/// highlighted option mirrors `pending.selected`; Enter confirms it.
fn render_permission_overlay(app: &App, frame: &mut Frame) {
    let Some(pending) = &app.permission else {
        return;
    };
    let theme = Theme::current();
    let area = frame.area();
    // The policy's reason for stopping is worth a row of its own: it turns the
    // popup from "approve this?" into "approve this because it leaves the
    // workspace".
    let reason = pending.request.reason.as_deref();
    let width = (area.width * 3 / 5).clamp(40, 80).min(area.width);
    let height = (if reason.is_some() { 11u16 } else { 9u16 }).min(area.height);
    let x = area.x + (area.width.saturating_sub(width) / 2);
    let y = area.y + (area.height.saturating_sub(height) / 2);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let preview: String = pending.request.input.chars().take(160).collect();
    let mut lines = vec![
        Line::from(Span::styled(
            format!("Allow `{}`?", pending.request.tool_name),
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            preview,
            Style::default().fg(theme.muted().ratatui()),
        )),
    ];
    if let Some(reason) = reason {
        lines.push(Line::from(Span::styled(
            format!("why: {reason}"),
            Style::default().fg(theme.error().ratatui()),
        )));
    }
    lines.push(Line::from(""));
    for (index, option) in pending.options.iter().enumerate() {
        let selected = index == pending.selected;
        let prefix = if selected { "❯ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!("{prefix}{option}"), style)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" permission ")
        .border_style(Style::default().fg(theme.accent().ratatui()));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: true })
            .block(block),
        popup,
    );
}

/// Draw the centered guide confirm while the operator shapes a next-task
/// draft. T6 two-column view: the left column keeps the task and the assembly
/// note; the right column is the **editable** draft editor — what it holds at
/// confirm time is exactly what [`App::guide_key`] sends.
fn render_guide_overlay(app: &App, frame: &mut Frame) {
    let Some(pending) = &app.guide else {
        return;
    };
    let theme = Theme::current();
    let area = frame.area();
    let width = (area.width * 3 / 4).clamp(50, 110).min(area.width);
    let height = 16u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width) / 2);
    let y = area.y + (area.height.saturating_sub(height) / 2);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(Rect {
            x: popup.x + 1,
            y: popup.y + 1,
            width: popup.width.saturating_sub(2),
            height: popup.height.saturating_sub(2),
        });
    let mut left = vec![
        Line::from(Span::styled(
            "guide · next task",
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            pending.task.clone(),
            Style::default().fg(theme.muted().ratatui()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "the draft was assembled from the work log, the current state and the task; edit the right column freely — Enter sends it as-is.",
            Style::default().fg(theme.muted().ratatui()).add_modifier(Modifier::DIM),
        )),
    ];
    left.push(Line::from(""));
    left.push(Line::from(Span::styled(
        "Enter sends · Esc keeps the task · queued behind a running turn",
        Style::default().fg(theme.accent().ratatui()),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(left)).wrap(Wrap { trim: true }),
        columns[0],
    );
    frame.render_widget(&pending.editor, columns[1]);
    // A border around the whole popup gives the two columns one shared frame.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" guide ")
        .border_style(Style::default().fg(theme.accent().ratatui()));
    frame.render_widget(block, popup);
}

/// Draw the centered queue panel (§11.1 P-C): the live pending queue with the
/// selected row highlighted, plus the editing keys. Rows are read at render
/// time, so a settle-time delivery cannot leave the panel showing ghosts.
fn render_queue_overlay(app: &App, frame: &mut Frame) {
    let Some(panel) = &app.queue_panel else {
        return;
    };
    let theme = Theme::current();
    let area = frame.area();
    let entries = app.model.queue().items();
    let rows = entries.len().max(1);
    // Title + one line per entry + a blank + the footer + two borders.
    let height = (rows as u16 + 5).min(area.height);
    let width = (area.width * 3 / 4).clamp(50, 110).min(area.width);
    let x = area.x + (area.width.saturating_sub(width) / 2);
    let y = area.y + (area.height.saturating_sub(height) / 2);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let mut lines = vec![Line::from(Span::styled(
        format!(
            "pending queue · {} (oldest first) · recent ledger {}",
            entries.len(),
            app.model.queue().recent().len()
        ),
        Style::default()
            .fg(theme.accent().ratatui())
            .add_modifier(Modifier::BOLD),
    ))];
    for (index, entry) in entries.iter().enumerate() {
        let selected = index == panel.selected.min(entries.len().saturating_sub(1));
        let mode = if entry.mode == crate::core::DeliveryMode::Steer {
            " ·steer"
        } else {
            ""
        };
        let lane = if entry.lane == crate::core::MAIN_LANE {
            String::new()
        } else {
            format!(" @{}", entry.lane)
        };
        let text: String = entry.text.chars().take(90).collect();
        let row = format!(
            "{}#{} [{}{lane}{mode}] {text}",
            if selected { "❯ " } else { "  " },
            entry.id,
            entry.source.label()
        );
        let style = if selected {
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted().ratatui())
        };
        lines.push(Line::from(Span::styled(row, style)));
    }
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "(empty)",
            Style::default().fg(theme.muted().ratatui()),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ select · e edit in input · x withdraw · +/- reorder · l re-lane · Esc close",
        Style::default().fg(theme.accent().ratatui()),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" queue ")
        .border_style(Style::default().fg(theme.accent().ratatui()));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(block),
        popup,
    );
}

/// Draw the centered key-reference overlay while the operator consults the
/// bindings. Each row is one action's *live* key (via `hint_for`) beside its
/// description, so the overlay stays truthful after a `keymap.toml` remap and
/// surfaces bindings the input bar has no room to list (guide, sections). Rows
/// are not wrapped: the table height is then exactly one line per action, and a
/// description too long for a narrow popup is clipped rather than pushing the
/// footer out of the box.
fn render_help_overlay(app: &App, frame: &mut Frame) {
    if !app.help_open {
        return;
    }
    let theme = Theme::current();
    let area = frame.area();
    let rows = app.keymap.help_rows();
    // Title + one line per action + a blank + a footer + two borders.
    let height = (rows.len() as u16 + 5).min(area.height);
    let width = (area.width * 3 / 5).clamp(46, 84).min(area.width);
    let x = area.x + (area.width.saturating_sub(width) / 2);
    let y = area.y + (area.height.saturating_sub(height) / 2);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    // Pad the key column to the widest binding so the descriptions align.
    let key_w = rows
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled(
        "key reference",
        Style::default()
            .fg(theme.accent().ratatui())
            .add_modifier(Modifier::BOLD),
    ))];
    for (key, desc) in rows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{key:<key_w$}"),
                Style::default()
                    .fg(theme.heading().ratatui())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(desc, Style::default().fg(theme.muted().ratatui())),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "{} closes · rebind any key in keymap.toml",
            app.keymap.hint_for(Action::Escape)
        ),
        Style::default().fg(theme.accent().ratatui()),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" keys ")
        .border_style(Style::default().fg(theme.accent().ratatui()));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);
}

/// Enter the alternate screen + raw mode, installing a panic hook that always
/// restores the terminal so a crash never leaves the user's shell wedged in
/// raw/alt-screen state.
fn enter() -> io::Result<Terminal<Backend>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        previous(info);
    }));
    Ok(terminal)
}

/// Restore the ordinary terminal. Best-effort: each step is ignored on error so
/// a failure to leave the alt screen never masks the real result.
fn exit(terminal: &mut Terminal<Backend>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()
}

/// Draw one frame bracketed by DEC 2026 synchronized-update sequences, so a
/// supporting terminal composites the whole frame atomically — no tearing or
/// flicker between the clear and the redraw. Terminals without DEC 2026 ignore
/// the bracketing escapes, so this is a pure quality wrapper with no downside.
/// ratatui 0.29's `CrosstermBackend` does not emit them itself, so the shell
/// adds them around every draw. Generic over the writer so the emitted byte
/// sequence is unit-testable against an in-memory `Vec<u8>`.
fn draw_synced<W, F>(terminal: &mut Terminal<CrosstermBackend<W>>, render: F) -> io::Result<()>
where
    W: io::Write,
    F: FnOnce(&mut Frame),
{
    queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    terminal.draw(render)?;
    queue!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    terminal.flush()
}

/// Run the full-screen carrier shell until the user quits. Owns the runtime and
/// drives turns inline on this task (not spawned): `ConversationRuntime`
/// carries a `Cell` token counter, so the turn future is not `Send` and cannot
/// live on `tokio::spawn` — but `block_on` (the `#[tokio::main]` path) imposes
/// no `Send` bound, so running turns here is both simpler and borrow-clean.
/// crossterm events are pumped from a blocking thread into one shared `Msg`
/// channel; the render loop is the sole consumer and sole drawer.
pub(crate) async fn run_shell<F>(
    state: &SessionShared,
    runtime: AgentRuntime,
    make_section: F,
    chrome: Chrome,
) -> io::Result<()>
where
    F: FnMut() -> Result<(SessionShared, AgentRuntime), Box<dyn std::error::Error>>,
{
    let mut terminal = enter()?;
    let mut shell = Shell::new();
    shell.chrome = chrome;
    // Layer the on-disk keymap and settings over the defaults, and reflect the
    // keymap in the input bar. The shell is the single terminal reader, so this
    // is also where a turn-boundary hot-reload refreshes the same fields.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home = crate::home_dir();
    let section = shell.section_mut();
    section.keymap = Keymap::load(&cwd, &home);
    section.settings = Settings::load(&cwd, &home);
    // Share the blocking REPL's history file so Up/Down recall spans both
    // surfaces and prior sessions.
    let history_path = home.join(".heartflow").join("history.txt");
    section.history = History::load(&history_path);
    section.history_path = history_path;
    section.refresh_input_title();
    // Baseline the watcher after the initial load so the first turn does not see
    // a spurious change; event_loop reloads theme/keymap/settings at each turn
    // boundary from the same four surfaces the blocking REPL watches.
    let mut watcher = ConfigWatcher::new(&cwd, &home);
    // The shell starts as one section: the runtime + state handed in (which may
    // carry a resumed conversation). `make_section` builds each *additional*
    // section's independent runtime + state on demand, so sections never share
    // conversation data or a save path.
    let result = event_loop(
        &mut terminal,
        &mut shell,
        vec![state.clone()],
        vec![runtime],
        make_section,
        &mut watcher,
        &cwd,
        &home,
    )
    .await;
    // Always restore, even if the loop errored, then surface the error.
    exit(&mut terminal)?;
    result
}

/// The channel plumbing one loop instance owns: the shared message bus (keys
/// from the pump, events from the turn) plus the permission decision pair the
/// prompter parks on. Grouped so [`drive_loop`]'s parameter list stays
/// readable.
struct LoopChannels {
    msg_tx: mpsc::UnboundedSender<Msg>,
    msg_rx: mpsc::UnboundedReceiver<Msg>,
    choice_tx: mpsc::UnboundedSender<PermissionChoice>,
    choice_rx: mpsc::UnboundedReceiver<PermissionChoice>,
}

/// What a finished turn hands back: the runtime and prompter it owned while
/// running, so the loop can use them for the next turn, plus the outcome.
struct TurnResult<C, T>
where
    C: ApiClient + 'static,
    T: ToolExecutor,
{
    runtime: ConversationRuntime<C, T>,
    prompter: TuiPermissionPrompter,
    outcome: Result<TurnSummary, RuntimeError>,
}

/// One live turn: the pinned turn future plus the section it drives.
///
/// The future is `'static` — it *owns* the section's runtime and the prompter
/// for the turn's duration (handed back in [`TurnResult`]) — so the loop holds
/// no borrow on the runtime vector while a turn runs. That is what keeps the
/// loop free to switch sections, spawn new ones and keep rendering mid-turn,
/// and it is the same ownership shape a future would need to be spawned later.
/// Boxed because the loop cannot name the `impl Future` type the async block
/// below returns.
struct LiveTurn<C, T>
where
    C: ApiClient + 'static,
    T: ToolExecutor,
{
    future: Pin<Box<dyn Future<Output = TurnResult<C, T>>>>,
    /// Which section's runtime this turn is driving. Section navigation is
    /// blocked while a turn is live, so this equals the active index; carrying
    /// it explicitly lets the settle phase route without relying on that.
    section: usize,
}

/// Drive the live turn to completion, borrowing it *in place*: a race lost to
/// the input arm must never drop a running turn, so the future is only taken
/// after this branch completes. Returns `None` when no turn is live — the
/// `select!` guard keeps that branch from being polled in that case.
async fn poll_live_turn<C, T>(turn: &mut Option<LiveTurn<C, T>>) -> Option<TurnResult<C, T>>
where
    C: ApiClient + 'static,
    T: ToolExecutor,
{
    match turn.as_mut() {
        Some(live) => Some(live.future.as_mut().await),
        None => None,
    }
}

/// Own the terminal, the event pump and the teardown; delegate the loop itself
/// to [`drive_loop`], which is generic — and unit-tested — over the API client
/// and tool executor and over the draw/save seams.
#[allow(clippy::too_many_arguments)]
async fn event_loop<F>(
    terminal: &mut Terminal<Backend>,
    shell: &mut Shell,
    states: Vec<SessionShared>,
    runtimes: Vec<AgentRuntime>,
    make_section: F,
    watcher: &mut ConfigWatcher,
    cwd: &std::path::Path,
    home: &std::path::Path,
) -> io::Result<()>
where
    F: FnMut() -> Result<(SessionShared, AgentRuntime), Box<dyn std::error::Error>>,
{
    // One channel carries every Msg: keys/resize from the pump, turn events and
    // permission requests from the turn. The loop is the sole consumer/drawer.
    let (msg_tx, msg_rx) = mpsc::unbounded_channel::<Msg>();
    // Permission overlay plumbing: the prompter raises a
    // `Msg::PermissionRequest` over `msg_tx` and parks on `choice_rx`; the loop
    // returns the user's selection over `choice_tx`. Keeping this off inquire
    // means raw mode + the alt screen never fight a second terminal reader.
    let (choice_tx, choice_rx) = mpsc::unbounded_channel::<PermissionChoice>();

    // crossterm event pump on a blocking thread; stopped by the shutdown flag
    // (or when the receiver drops so `send` fails).
    let shutdown = Arc::new(AtomicBool::new(false));
    let budget = Duration::from_millis(shell.section().settings.frame_budget_ms);
    let pump = tokio::task::spawn_blocking({
        let tx = msg_tx.clone();
        let shutdown = shutdown.clone();
        move || pump_events(tx, &shutdown, budget)
    });

    // The production draw seam: one frame of the shell through the shared
    // terminal, bracketed by the synchronized-update escapes (see draw_synced).
    let draw = |shell: &Shell| draw_synced(terminal, |frame| shell.view(frame));
    let result = drive_loop(
        shell,
        states,
        // Slots: a section's runtime is `None` exactly while its turn owns it.
        runtimes.into_iter().map(Some).collect(),
        make_section,
        watcher,
        cwd,
        home,
        LoopChannels {
            msg_tx,
            msg_rx,
            choice_tx,
            choice_rx,
        },
        draw,
        DiskSaver,
    )
    .await;

    // Teardown: stop the pump and reap it. drive_loop dropped its channel ends
    // on return, so the pump's next `send` fails even mid-poll-cycle; the flag
    // bounds how long a parked poll keeps it alive.
    shutdown.store(true, Ordering::Relaxed);
    let _ = pump.await;
    result
}

/// The three-phase loop (roadmap §11.0): **render** (draw when dirty),
/// **turn driving** (start a queued turn, poll the live one), **input
/// collection** (`select!` over turn completion and the shared channel).
///
/// One loop replaces the old outer loop plus the `select!` nested inside
/// `run_one_turn`: the loop keeps pumping input and rendering *while a turn is
/// in flight* instead of parking inside it. That reachability is what unlocks
/// steer injection, mission accounting, in-flight cancellation and graceful
/// shutdown (roadmap §11.2/§11.3) — each hangs off a phase below.
///
/// Generic over the API client `C` and tool executor `T` so tests script turns
/// without a provider; `draw` and `save` are injected so the loop runs without
/// a terminal or the real home directory.
#[allow(clippy::too_many_arguments)]
async fn drive_loop<C, T, F, D, S>(
    shell: &mut Shell,
    mut states: Vec<SessionShared>,
    mut runtimes: Vec<Option<ConversationRuntime<C, T>>>,
    mut make_section: F,
    watcher: &mut ConfigWatcher,
    cwd: &std::path::Path,
    home: &std::path::Path,
    mut channels: LoopChannels,
    mut draw: D,
    mut save: S,
) -> io::Result<()>
where
    C: ApiClient + 'static,
    T: ToolExecutor,
    F: FnMut() -> Result<(SessionShared, ConversationRuntime<C, T>), Box<dyn std::error::Error>>,
    D: FnMut(&Shell) -> io::Result<()>,
    S: TurnSaver,
{
    let mut prompter = Some(TuiPermissionPrompter {
        allow_all: false,
        msg_tx: channels.msg_tx.clone(),
        choice_rx: channels.choice_rx,
    });
    // Turn inputs waiting to run, oldest first. An idle submit stages one turn
    // here (fresh submits keep first priority and always run in the active
    // section); a submit *during* a turn goes into that section's follow-up
    // queue and is picked up one entry per turn by the §11.4 rotation.
    let mut queue: VecDeque<String> = VecDeque::new();
    // Seed the guide snapshot while the runtime is free (before the first turn)
    // so Ctrl+G has a work log to preview from the first idle moment.
    let seed = shell.active_index();
    if let Some(runtime) = &runtimes[seed] {
        shell.section_mut().guide_context = guide_snapshot(runtime);
    }
    let mut dirty = true;
    // The live turn, pinned at loop scope: the loop pumps it instead of
    // awaiting it inline. `turn_cancel` lives outside the future so the input
    // phase can cancel without borrowing the turn.
    let mut turn: Option<LiveTurn<C, T>> = None;
    let mut turn_cancel: Option<CancellationToken> = None;
    // The live turn's steer mailbox (§11.2), same lifetime discipline as the
    // cancel token: created per turn, loop-held while it runs, dropped at
    // settle. The runtime gets its clone before moving into the future.
    let mut turn_steer: Option<SteerMailbox> = None;
    // The queue entry this turn is delivering, when the turn came from the
    // rotation (§11.4). Terminalized at settle: Delivered on success, one
    // re-queue on failure, `Failed` in the ledger if the *same* entry fails a
    // second consecutive time — an auto-rotating queue must not retry a
    // poisoned entry forever.
    let mut turn_entry: Option<crate::core::QueueEntry> = None;
    let mut last_failed_entry: Option<u64> = None;

    while !shell.section().quit {
        // ---- Phase R: render ----
        if dirty {
            draw(shell)?;
            dirty = false;
        }
        // ---- Phase T: turn driving (start) ----
        // Fresh submits come first and always run in the active section: a
        // submit is drained and run before any key (including a section
        // switch) is processed, and a live turn blocks nav. When nothing is
        // staged, the §11.4 rotation picks the next section with pending
        // queue work (round-robin from the cursor) and runs its oldest
        // deliverable entry there — one entry per turn, merged mode aside.
        if turn.is_none() {
            let pickup: Option<(usize, String)> = if let Some(input) = queue.pop_front() {
                turn_entry = None;
                Some((shell.active_index(), input))
            } else if let Some(section) = shell.next_queue_section() {
                let model = &mut shell.section_index_mut(section).model;
                let picked = if model.queue().is_merged() {
                    // Merged mode (D2.2 opt-in): the section's whole pending
                    // queue is one combined injection, terminalized on the
                    // spot by the drain — nothing left to settle.
                    model
                        .queue_mut()
                        .drain_injection()
                        .map(|injection| (injection, None))
                } else {
                    // Default per-entry delivery (D2.2): one entry, one turn —
                    // the raw text, exactly like the REPL's per-item path (no
                    // merged-injection header around a single message); the
                    // entry terminalizes at settle.
                    model.queue_mut().take_next().map(|entry| {
                        let injection = entry.text.clone();
                        (injection, Some(entry))
                    })
                };
                picked.map(|(injection, entry)| {
                    shell.delivery_cursor = (section + 1) % shell.sections.len();
                    turn_entry = entry;
                    (section, injection)
                })
            } else {
                None
            };
            if let Some((active, input)) = pickup {
                // Set the queue-routing flag synchronously (before any await) so
                // a submit from the very first instant of the turn enqueues
                // rather than racing the async `Msg::TurnStarted`.
                shell.section_index_mut(active).model.begin_turn();
                let cancel = CancellationToken::new();
                let _ = channels.msg_tx.send(Msg::TurnStarted { section: active });
                // Take the section's runtime and the shared prompter out of the
                // loop's hands: the turn future owns them while it runs, which
                // is what keeps this loop free to switch sections, spawn new
                // ones and keep rendering mid-turn. Both takes pair with a
                // restore in the settle phase; the guard arms are unreachable
                // (a live turn blocks this phase) and restore + re-queue rather
                // than dropping the input if they ever fire.
                let Some(runtime) = runtimes[active].take() else {
                    // The rotation's entry was already taken out of the
                    // section queue; a plain input push-back would lose it
                    // between ledgers. Restore it to the front so the next
                    // pickup retakes it.
                    if let Some(entry) = turn_entry.take() {
                        shell
                            .section_index_mut(active)
                            .model
                            .queue_mut()
                            .requeue_front(entry);
                    }
                    queue.push_front(input);
                    continue;
                };
                let Some(turn_prompter) = prompter.take() else {
                    runtimes[active] = Some(runtime);
                    if let Some(entry) = turn_entry.take() {
                        shell
                            .section_index_mut(active)
                            .model
                            .queue_mut()
                            .requeue_front(entry);
                    }
                    queue.push_front(input);
                    continue;
                };
                // Arm this turn's steer mailbox before the runtime moves into
                // the future: the loop keeps the clone to push into while the
                // turn runs.
                let steer = SteerMailbox::default();
                let mut runtime = runtime;
                runtime.set_steer_mailbox(steer.clone());
                // D1.3 mission reminder: same per-turn projection the REPL
                // injects — the on-disk ledger rendered as one system-prompt
                // section, rebuilt fresh at every turn boundary.
                let missions_root = crate::storage::sessions_dir().join("missions");
                runtime
                    .set_turn_context(turn_reminder(&MissionLedger::load(&missions_root).list()));
                let tx = channels.msg_tx.clone();
                let mut notify = move |event: &AgentEvent| {
                    let _ = tx.send(Msg::Event {
                        section: active,
                        event: event.clone(),
                    });
                };
                let blocks = expand_attachments(&input);
                // The loop keeps its own token clone so the input phase can
                // cancel without borrowing the turn future.
                let future_cancel = cancel.clone();
                let future = async move {
                    let mut runtime = runtime;
                    let mut prompter = turn_prompter;
                    let outcome = runtime
                        .run_turn_with_blocks(
                            blocks,
                            Some(&mut prompter as &mut dyn PermissionPrompter),
                            &mut notify,
                            &future_cancel,
                        )
                        .await;
                    TurnResult {
                        runtime,
                        prompter,
                        outcome,
                    }
                };
                turn = Some(LiveTurn {
                    future: Box::pin(future),
                    section: active,
                });
                turn_cancel = Some(cancel);
                turn_steer = Some(steer);
            }
        }
        // ---- Phase I: input collection (and turn completion) ----
        // Biased with the turn arm first: a completed turn settles before
        // pending input is read, keeping the settle boundary (save, queue
        // merge, guide refresh) ahead of whatever was typed meanwhile. Event
        // ordering converges either way — the runtime queues every streamed
        // event before its future resolves, so a burst drained after
        // `TurnFinished` is a no-op flush or a status-bar update.
        tokio::select! {
            biased;
            outcome = poll_live_turn(&mut turn), if turn.is_some() => {
                // The select! guard proved a turn was live when this branch
                // completed; the `else` arm is unreachable.
                let Some(live) = turn.take() else {
                    continue;
                };
                turn_cancel = None;
                turn_steer = None;
                let section = live.section;
                // The finished future drops here; its output is already in
                // `outcome`.
                drop(live);
                let Some(TurnResult {
                    mut runtime,
                    prompter: finished_prompter,
                    outcome: turn_outcome,
                }) = outcome
                else {
                    continue;
                };
                // The loop owns the runtime and prompter again; drop the dead
                // turn's mailbox handle so the next turn arms a fresh one.
                runtime.set_steer_mailbox(SteerMailbox::default());
                runtime.set_turn_context(None);
                prompter = Some(finished_prompter);
                let mut mission_attention: Option<String> = None;
                let (ok, err_note) = match turn_outcome {
                    // The summary's contents (iteration count, tool results)
                    // are already folded into the session and the transcript;
                    // the TUI settle only needs success vs. failure.
                    Ok(summary) => {
                        // Auto-account against the single active mission
                        // (§11.3): success only — a failed turn is not
                        // productive work and must not reset a blocked
                        // streak.
                        let tokens = u64::from(summary.usage.input_tokens)
                            .saturating_add(u64::from(summary.usage.output_tokens));
                        mission_attention =
                            auto_record_turn(&crate::storage::sessions_dir().join("missions"), tokens);
                        (true, None)
                    }
                    Err(error) => (false, Some(error.to_string())),
                };
                // With the runtime back in hand, persist the turn — success or
                // failure, the transcript on disk stays the source of truth.
                let save_note = match save.save(&states[section], runtime.session()).await {
                    Ok(path) => format!("saved {}", path.display()),
                    Err(_) => String::from("save failed"),
                };
                let guide = guide_snapshot(&runtime);
                runtimes[section] = Some(runtime);
                let app = shell.section_index_mut(section);
                if !ok {
                    // Surface the failure in the transcript, mirroring
                    // run_turn_interactive.
                    let text = err_note
                        .clone()
                        .unwrap_or_else(|| String::from("turn failed"));
                    app.apply_event(&AgentEvent::Error {
                        message: text,
                        hint: None,
                    });
                }
                // Turn boundary (locked FollowUpQueue semantics): drop the
                // running flag so a submit racing the save routes to the next
                // turn. Steers the turn never consumed are demoted to
                // follow-ups (pi's "child completed before consuming
                // steering" recovery) and *stay* in the section's queue —
                // the §11.4 rotation picks them up like any pending entry,
                // one turn each, instead of the old whole-queue merged
                // injection.
                app.model.end_turn();
                if let Some(attention) = mission_attention.take() {
                    app.push_line(muted_line(format!("· {attention}")));
                }
                let recovered = app.model.queue_mut().recover_pending_steers();
                if !recovered.is_empty() {
                    app.push_line(muted_line(format!(
                        "· {} late steer(s) queued as follow-up(s)",
                        recovered.len()
                    )));
                }
                // Terminalize the rotation's ledger entry (§11.4): Delivered
                // on success; one automatic re-queue on failure; the *same*
                // entry failing a second consecutive time files as `Failed` —
                // an auto-rotating queue must not retry a poisoned entry
                // forever.
                if let Some(entry) = turn_entry.take() {
                    let retried = last_failed_entry == Some(entry.id);
                    let state = if ok {
                        last_failed_entry = None;
                        crate::core::QueueEntryState::Delivered
                    } else if retried {
                        last_failed_entry = None;
                        crate::core::QueueEntryState::Failed
                    } else {
                        last_failed_entry = Some(entry.id);
                        crate::core::QueueEntryState::Pending
                    };
                    if state == crate::core::QueueEntryState::Pending {
                        app.model.queue_mut().requeue_front(entry);
                    } else {
                        app.model.queue_mut().finish_entry(entry, state);
                    }
                }
                let note = err_note.unwrap_or(save_note);
                let _ = channels
                    .msg_tx
                    .send(Msg::TurnFinished { section, ok, note });
                // Turn boundary: refresh the guide snapshot to fold in the turn
                // that just finished, and pick up config surfaces edited on disk
                // while it ran. theme swaps the global palette (the next frame
                // reads it); keymap/settings re-load the App-owned values and
                // refresh the input bar. The TUI does not rebuild the provider
                // runtime here (that is the multi-runtime section work), so a
                // config.toml edit has its stamp refreshed but is otherwise left
                // for a restart.
                app.guide_context = guide;
                let changed = watcher.changed();
                if changed.any() {
                    if changed.theme {
                        Theme::reload(cwd, home);
                    }
                    if changed.keymap {
                        for app in &mut shell.sections {
                            app.keymap = Keymap::load(cwd, home);
                            app.refresh_input_title();
                        }
                    }
                    if changed.settings {
                        for app in &mut shell.sections {
                            app.settings = Settings::load(cwd, home);
                        }
                    }
                }
                dirty = true;
            }
            maybe = channels.msg_rx.recv() => match maybe {
                Some(msg) => {
                    // Section navigation is a shell concern. Since §11.4 gave
                    // turn traffic its own section tag, switching while a
                    // turn runs is safe: the live turn keeps its runtime (the
                    // slot it owns is keyed by *its* section, not the active
                    // one) and its events route to their own transcript —
                    // which is what unlocks "work continues in a background
                    // section while you switch to another" (the original
                    // queue-mode ask).
                    let nav = match &msg {
                        Msg::Key(key) => shell.section().keymap.resolve(key),
                        _ => None,
                    };
                    match nav {
                        Some(Action::NextSection) => {
                            shell.next_section();
                            dirty = true;
                        }
                        Some(Action::PrevSection) => {
                            shell.prev_section();
                            dirty = true;
                        }
                        Some(Action::NewSection) => {
                            dirty = true;
                            if shell.sections.len() >= MAX_SECTIONS {
                                shell.section_mut().status_note =
                                    format!("section limit reached ({MAX_SECTIONS} max)");
                            } else if let Ok((state, runtime)) = make_section() {
                                // Snapshot the fresh runtime's (empty) guide context
                                // before moving it into the parallel vector.
                                let guide = guide_snapshot(&runtime);
                                let title = (shell.sections.len() + 1).to_string();
                                states.push(state);
                                runtimes.push(Some(runtime));
                                shell.push_section(title);
                                let section = shell.section_mut();
                                section.keymap = Keymap::load(cwd, home);
                                section.settings = Settings::load(cwd, home);
                                // A new section joins the same shared history file.
                                let history_path = home.join(".heartflow").join("history.txt");
                                section.history = History::load(&history_path);
                                section.history_path = history_path;
                                section.refresh_input_title();
                                section.guide_context = guide;
                            } else {
                                shell.section_mut().status_note =
                                    String::from("could not open a new section");
                            }
                        }
                        // Every other key (and any non-key message) belongs to
                        // a section's reducer. Turn traffic (§11.4 tags)
                        // routes to the section that owns the turn; keys go
                        // to the active section — except interrupt/escape
                        // while a *background* turn runs, which the running
                        // section must see (its own `turn_active` drives the
                        // double-press cancel and keeps Esc from quitting the
                        // shell mid-turn).
                        _ => {
                            let mut routed = msg_section(&msg);
                            if let (Some(live), Msg::Key(key)) = (turn.as_ref(), &msg) {
                                let action = shell.section().keymap.resolve(key);
                                if live.section != shell.active_index()
                                    && matches!(
                                        action,
                                        Some(Action::Interrupt) | Some(Action::Escape)
                                    )
                                {
                                    routed = Some(live.section);
                                }
                            }
                            let target = routed.unwrap_or_else(|| shell.active_index());
                            dirty |= shell.section_index_mut(target).update(msg);
                            // A resolved permission overlay returns to the
                            // prompter, which is parked inside the suspended
                            // turn future.
                            if let Some(choice) = shell.section_index_mut(target).take_permission_choice() {
                                let _ = channels.choice_tx.send(choice);
                            }
                            // Second Ctrl+C tap, or a quit request: cancel and
                            // let the turn unwind before the loop tears down.
                            if let Some(cancel) = turn_cancel.as_ref() {
                                let running = turn.as_ref().map_or(target, |live| live.section);
                                if shell.section_index_mut(running).take_cancel()
                                    || (turn.is_none() && shell.section().quit)
                                {
                                    cancel.cancel();
                                }
                            }
                        }
                    }
                    if let Some(text) = shell.section_mut().take_submit() {
                        queue.push_back(text);
                    }
                    // A staged steer goes into the live turn's mailbox
                    // (§11.2); if the turn settled in the same window, demote
                    // its pending queue entry to a follow-up instead.
                    if let Some(text) = shell.section_mut().take_steer() {
                        match turn_steer.as_ref() {
                            Some(mailbox) => mailbox.push(text),
                            None => {
                                shell.section_mut().model.queue_mut().recover_pending_steers();
                            }
                        }
                    }
                }
                // Pump gone: cancel a live turn so it unwinds and settles (the
                // save still runs); with nothing live, nothing drives the loop.
                None => {
                    if let Some(cancel) = turn_cancel.as_ref() {
                        cancel.cancel();
                    } else {
                        break;
                    }
                }
            },
        }
    }
    Ok(())
}

/// Blocking crossterm event pump. Translates terminal events into [`Msg`]s and
/// forwards them; exits on shutdown, send failure, or read error. Runs on a
/// blocking thread because crossterm's `poll`/`read` are synchronous. `budget`
/// is the frame-merge window: idle loops wake on the poll timeout but skip the
/// draw (see the `dirty` flag), and bursts of input coalesce into one repaint,
/// so a quiet session costs nothing.
fn pump_events(tx: mpsc::UnboundedSender<Msg>, shutdown: &AtomicBool, budget: Duration) {
    while !shutdown.load(Ordering::Relaxed) {
        match event::poll(budget) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    // Ignore key-release repeats some terminals emit; only act
                    // on press to avoid double-processing held keys.
                    if key.kind != KeyEventKind::Release && tx.send(Msg::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(..)) => {
                    if tx.send(Msg::Resize).is_err() {
                        break;
                    }
                }
                Ok(Event::Paste(text)) => {
                    if tx.send(Msg::Paste(text)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    }
}

/// Extract the guide snapshot (work log + pending-task state) from a runtime
/// that is free to read — startup and turn boundaries, never mid-turn while a
/// turn owns its runtime. `pending_tasks` is the trait-level plan count: the
/// concrete executor reports exactly its todo ledger's pending count through
/// it, so the snapshot is unchanged by going through the trait.
fn guide_snapshot<C, T>(runtime: &ConversationRuntime<C, T>) -> GuideContext
where
    C: ApiClient,
    T: ToolExecutor,
{
    guide_context(
        &runtime.session().messages,
        runtime.executor().pending_tasks(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        draw_synced, render_items, view, visible_window, wrapped_height, App, Chrome, GuideContext,
        Item, Msg, PermissionChoice, Shell, TuiPermissionPrompter,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use runtime::{
        AgentEvent, PermissionPromptDecision, PermissionPrompter, PermissionRequest, TokenUsage,
    };
    use tokio::sync::mpsc;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn char_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    /// Turn traffic for section 0 — the section tag is loop-level routing the
    /// reducer ignores, so tests talk to section 0 uniformly.
    fn turn_started() -> Msg {
        Msg::TurnStarted { section: 0 }
    }

    fn ev(event: AgentEvent) -> Msg {
        Msg::Event { section: 0, event }
    }

    fn finished(ok: bool, note: &str) -> Msg {
        Msg::TurnFinished {
            section: 0,
            ok,
            note: note.to_string(),
        }
    }

    fn ctrl_g() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)
    }

    fn request(tool: &str) -> Msg {
        Msg::PermissionRequest(PermissionRequest {
            tool_name: tool.to_string(),
            input: r#"{"command":"rm -rf /"}"#.to_string(),
            reason: Some("the command looks destructive".to_string()),
        })
    }

    #[test]
    fn permission_request_raises_the_overlay() {
        let mut app = App::new();
        app.update(turn_started());
        assert!(app.permission.is_none());
        assert!(app.update(request("bash")), "raising the overlay repaints");
        assert!(app.permission.is_some());
    }

    #[test]
    fn permission_overlay_enter_grants_the_highlighted_option() {
        let mut app = App::new();
        app.update(request("bash"));
        // The first option ("Allow once") starts highlighted.
        assert!(app.update(Msg::Key(key(KeyCode::Enter))));
        assert!(matches!(
            app.take_permission_choice(),
            Some(PermissionChoice::AllowOnce)
        ));
        assert!(app.permission.is_none(), "resolving clears the overlay");
    }

    #[test]
    fn permission_overlay_arrow_down_selects_allow_all() {
        let mut app = App::new();
        app.update(request("bash"));
        app.update(Msg::Key(key(KeyCode::Down)));
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(matches!(
            app.take_permission_choice(),
            Some(PermissionChoice::AllowAll)
        ));
    }

    #[test]
    fn permission_overlay_esc_denies_without_quitting() {
        let mut app = App::new();
        app.update(turn_started());
        app.update(request("bash"));
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(matches!(
            app.take_permission_choice(),
            Some(PermissionChoice::Deny)
        ));
        assert!(!app.quit, "Esc in the overlay denies, it never exits");
        assert!(app.permission.is_none());
    }

    #[test]
    fn permission_overlay_swallows_editor_keys() {
        let mut app = App::new();
        app.update(request("bash"));
        // A stray character must not reach the input editor behind the overlay.
        app.update(Msg::Key(char_key('x')));
        assert!(
            app.input.lines().iter().all(String::is_empty),
            "the editor stays empty behind the overlay"
        );
        assert!(app.permission.is_some(), "and the overlay stays open");
    }

    #[test]
    fn permission_overlay_renders_the_policys_reason() {
        let mut app = App::new();
        app.update(turn_started());
        app.update(Msg::PermissionRequest(PermissionRequest {
            tool_name: "write_file".to_string(),
            input: r#"{"path":"../escape.rs"}"#.to_string(),
            reason: Some("outside the workspace root".to_string()),
        }));
        // The render path must surface the policy's `reason`, not just the tool
        // name — the operator is being asked *because* the call leaves the
        // workspace, and that is the fact they need to decide on.
        let rows = render_app_rows(&app, 80, 16);
        assert!(
            rows.iter().any(|row| row.contains("why:")),
            "the popup labels the reason: {rows:#?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("outside the workspace root")),
            "the popup prints the reason text: {rows:#?}"
        );
    }

    #[tokio::test]
    async fn prompter_allow_all_short_circuits_the_overlay() {
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<Msg>();
        let (choice_tx, choice_rx) = mpsc::unbounded_channel::<PermissionChoice>();
        let mut prompter = TuiPermissionPrompter {
            allow_all: true,
            msg_tx,
            choice_rx,
        };
        let req = PermissionRequest {
            tool_name: "bash".to_string(),
            input: "x".to_string(),
            reason: None,
        };
        let decision = prompter.decide(&req).await;
        assert!(matches!(decision, PermissionPromptDecision::Allow));
        assert!(
            msg_rx.try_recv().is_err(),
            "allow_all must not raise an overlay"
        );
        drop(choice_tx);
    }

    #[test]
    fn typing_then_enter_stages_and_echoes_the_submission() {
        let mut app = App::new();
        for c in "hi".chars() {
            assert!(app.update(Msg::Key(char_key(c))));
        }
        assert!(app.transcript.is_empty(), "nothing submitted yet");
        assert!(app.update(Msg::Key(key(KeyCode::Enter))));
        assert_eq!(app.take_submit().as_deref(), Some("hi"));
        assert_eq!(app.transcript.len(), 1, "the line is echoed on submit");
        assert!(
            app.input.lines().iter().all(String::is_empty),
            "editor clears"
        );
    }

    #[test]
    fn pasted_newlines_stay_a_multiline_draft_and_never_submit() {
        let mut app = App::new();
        // A bracketed-paste block arrives whole, so its embedded newline becomes
        // a draft line rather than a bare Enter that fires the turn.
        assert!(app.update(Msg::Paste("line1\nline2".to_string())));
        assert_eq!(app.input.lines().join("\n"), "line1\nline2");
        assert!(app.take_submit().is_none(), "paste must not submit");
        assert!(app.transcript.is_empty(), "nothing ran");
    }

    #[test]
    fn paste_behind_the_permission_overlay_is_swallowed() {
        let mut app = App::new();
        app.update(request("bash"));
        // A modal overlay swallows paste exactly as it swallows keys, so a stray
        // block can never reach the editor behind it.
        app.update(Msg::Paste("rm -rf /".to_string()));
        assert!(
            app.input.lines().iter().all(String::is_empty),
            "the editor stays empty behind the overlay"
        );
    }

    #[test]
    fn empty_submission_is_a_no_op() {
        let mut app = App::new();
        let before = app.transcript.len();
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.transcript.len(), before, "blank line adds nothing");
        assert!(app.take_submit().is_none(), "nothing staged");
    }

    #[test]
    fn submit_while_running_enqueues_instead_of_staging_a_turn() {
        let mut app = App::new();
        // A live turn: submits must queue, never run as their own turn.
        app.model.begin_turn();
        for c in "later".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        assert!(app.update(Msg::Key(key(KeyCode::Enter))));
        assert!(
            app.pending_submit.is_none(),
            "a queued follow-up must not stage a separate turn"
        );
        assert_eq!(app.model.queue().len(), 1);
        assert_eq!(
            app.model
                .queue()
                .items()
                .front()
                .map(|entry| entry.text.as_str()),
            Some("later")
        );
        assert!(
            app.status_note.contains("queued"),
            "the status bar surfaces the queue depth"
        );
    }

    #[test]
    fn steer_submit_while_running_stages_for_the_live_turn() {
        let mut app = App::new();
        app.model.begin_turn();
        app.update(turn_started());
        for c in "pivot now".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        let steer_key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(app.update(Msg::Key(steer_key)));
        assert!(
            app.pending_submit.is_none(),
            "a steer must never queue a separate turn"
        );
        assert_eq!(app.take_steer().as_deref(), Some("pivot now"));
        assert_eq!(
            app.model.queue().items().front().map(|entry| entry.mode),
            Some(crate::core::DeliveryMode::Steer),
            "the steer is visible in the queue with its mode"
        );
        // Consuming the steer terminalizes the entry and echoes it.
        assert!(app.update(ev(AgentEvent::Steered("pivot now".into()))));
        assert!(app.model.queue().is_empty());
    }

    #[test]
    fn steer_submit_while_idle_behaves_like_a_plain_submit() {
        let mut app = App::new();
        for c in "hi".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        let steer_key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(app.update(Msg::Key(steer_key)));
        assert!(app.take_steer().is_none(), "nothing to steer while idle");
        assert_eq!(app.take_submit().as_deref(), Some("hi"));
    }

    #[test]
    fn queued_follow_ups_merge_into_one_injection_at_the_boundary() {
        let mut app = App::new();
        app.model.begin_turn();
        for text in ["first", "second"] {
            for c in text.chars() {
                app.update(Msg::Key(char_key(c)));
            }
            app.update(Msg::Key(key(KeyCode::Enter)));
        }
        assert_eq!(app.model.queue().len(), 2, "both submits queued");
        // The turn-boundary merge the loop performs: one injection, oldest first.
        app.model.end_turn();
        let injection = app.model.queue_mut().drain_injection().expect("two queued");
        let a = injection.find("first").expect("first present");
        let b = injection.find("second").expect("second present");
        assert!(a < b, "oldest follow-up is injected first");
        assert!(app.model.queue().is_empty(), "drain empties the queue");
    }

    #[test]
    fn queued_submit_echoes_a_pending_marker() {
        let mut app = App::new();
        app.model.begin_turn();
        for c in "hold".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        app.update(Msg::Key(key(KeyCode::Enter)));
        let Item::Line(line) = app.transcript.last().expect("echoed") else {
            panic!("a queued submit echoes a transcript line");
        };
        assert!(
            line.spans
                .iter()
                .any(|span| span.content.contains("(queued)")),
            "the echo is marked pending, not sent"
        );
    }

    #[test]
    fn ctrl_g_with_a_task_opens_the_guide_preview() {
        let mut app = App::new();
        app.guide_context = GuideContext {
            work_log: String::from("user: did a thing\n"),
            state: String::from("1 pending tasks in the ledger"),
        };
        for c in "next step".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        assert!(app.update(Msg::Key(ctrl_g())), "Ctrl+G opens the overlay");
        let pending = app.guide.as_ref().expect("guide open");
        assert_eq!(pending.task, "next step");
        let draft = pending.editor.lines().join("\n");
        assert!(
            draft.contains("[guide: next task]\nnext step"),
            "the task lands last in the draft"
        );
        assert!(
            draft.contains("user: did a thing"),
            "the snapshot's work log rides along"
        );
    }

    #[test]
    fn ctrl_g_on_empty_input_hints_without_opening() {
        let mut app = App::new();
        app.update(Msg::Key(ctrl_g()));
        assert!(app.guide.is_none(), "no task, no overlay");
        assert!(
            app.status_note.contains("next task"),
            "the status bar tells the operator to type the task first"
        );
    }

    #[test]
    fn guide_confirm_sends_the_draft_as_a_submission() {
        let mut app = App::new();
        for c in "ship it".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        app.update(Msg::Key(ctrl_g()));
        assert!(app.update(Msg::Key(key(KeyCode::Enter))), "Enter confirms");
        assert!(app.guide.is_none(), "the overlay closes on confirm");
        let body = app.take_submit().expect("draft staged");
        assert!(
            body.contains("[guide: next task]\nship it"),
            "the staged body is the assembled draft, not the bare task"
        );
        assert!(
            app.input.lines().join("\n").is_empty(),
            "confirming clears the input"
        );
    }

    #[test]
    fn guide_confirm_while_running_queues_the_draft() {
        let mut app = App::new();
        app.model.begin_turn();
        for c in "later task".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        app.update(Msg::Key(ctrl_g()));
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(
            app.pending_submit.is_none(),
            "a guided draft never stages its own turn mid-flight"
        );
        assert_eq!(app.model.queue().len(), 1);
        let queued = app.model.queue().items().front().expect("one queued");
        assert!(
            queued.text.contains("[guide: next task]\nlater task"),
            "the queued body is the full draft"
        );
    }

    #[test]
    fn guide_cancel_keeps_the_input_for_editing() {
        let mut app = App::new();
        for c in "draft me".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        app.update(Msg::Key(ctrl_g()));
        assert!(app.guide.is_some(), "overlay open");
        assert!(app.update(Msg::Key(key(KeyCode::Esc))), "Esc cancels");
        assert!(app.guide.is_none(), "overlay closed");
        assert!(app.take_submit().is_none(), "nothing staged");
        assert_eq!(
            app.input.lines().join("\n"),
            "draft me",
            "the task survives a cancel for further editing"
        );
    }

    #[test]
    fn guide_draft_is_editable_before_confirming() {
        // T6: the confirm overlay edits the draft in place; Enter sends the
        // edited body, not the assembled original.
        let mut app = App::new();
        for c in "ship it".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        app.update(Msg::Key(ctrl_g()));
        assert!(app.guide.is_some(), "overlay open");
        // Edit inside the overlay: type a marker at the draft's head.
        for c in "EDIT:".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        app.update(Msg::Key(key(KeyCode::Enter)));
        let body = app.take_submit().expect("edited draft staged");
        assert!(
            body.starts_with("EDIT:"),
            "the edited text leads the sent body: {body:?}"
        );
        assert!(
            body.contains("ship it"),
            "the untouched rest of the draft survives the edit"
        );
    }

    #[test]
    fn queue_panel_edits_withdraws_reorders_and_relanes() {
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let mut app = App::new();
        // Opening on an empty queue hints instead of popping a dead panel.
        app.update(Msg::Key(ctrl_q));
        assert!(app.queue_panel.is_none());
        assert!(app.status_note.contains("queue is empty"));

        app.model.begin_turn();
        app.update(turn_started());
        app.model
            .queue_mut()
            .send("aa", crate::core::QueueSource::User);
        app.model
            .queue_mut()
            .send_in_lane("review", "bb", crate::core::QueueSource::Guide);
        app.model
            .queue_mut()
            .send("cc", crate::core::QueueSource::User);

        // Open, walk down to bb (#2), and re-lane it into main.
        app.update(Msg::Key(ctrl_q));
        assert!(app.queue_panel.is_some());
        app.update(Msg::Key(key(KeyCode::Down)));
        app.update(Msg::Key(char_key('l')));
        assert!(
            app.status_note.contains("#2 → lane main"),
            "the cycle moved bb out of review: {}",
            app.status_note
        );
        // Select cc (#3) and move it one slot earlier; the selection follows
        // the index, so it now points at bb.
        app.update(Msg::Key(key(KeyCode::Down)));
        app.update(Msg::Key(char_key('+')));
        assert_eq!(
            app.model.queue().items()[1].text,
            "cc",
            "reorder moved cc ahead of bb"
        );
        // Withdraw the selected entry (bb): the ledger terminalizes it.
        app.update(Msg::Key(char_key('x')));
        assert_eq!(app.model.queue().len(), 2);
        assert!(app
            .model
            .queue()
            .recent()
            .iter()
            .any(|entry| entry.text == "bb"
                && entry.state == crate::core::QueueEntryState::Withdrawn));
        // Edit pulls the selected entry (clamped onto cc) back into the
        // input for rework; the panel stays open with one row left.
        app.update(Msg::Key(key(KeyCode::Down)));
        app.update(Msg::Key(char_key('e')));
        assert!(app.queue_panel.is_some());
        assert_eq!(app.input.lines().join("\n"), "cc");
        // Close: the remaining entry stays queued.
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(app.queue_panel.is_none());
        assert_eq!(app.model.queue().len(), 1);
    }

    #[test]
    fn queue_panel_closes_when_the_last_entry_leaves() {
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let mut app = App::new();
        app.model.begin_turn();
        app.model
            .queue_mut()
            .send("only", crate::core::QueueSource::User);
        app.update(Msg::Key(ctrl_q));
        assert!(app.queue_panel.is_some());
        app.update(Msg::Key(char_key('x')));
        assert!(app.queue_panel.is_none(), "no rows left, panel closes");
        assert!(app.model.queue().is_empty());
    }

    #[test]
    fn f1_opens_the_key_reference_overlay() {
        let mut app = App::new();
        assert!(!app.help_open);
        assert!(
            app.update(Msg::Key(key(KeyCode::F(1)))),
            "raising the overlay repaints"
        );
        assert!(app.help_open);
        assert!(
            app.status_note.contains("key reference"),
            "the status bar names the overlay and its close key"
        );
    }

    #[test]
    fn help_overlay_swallows_editor_keys_until_closed() {
        let mut app = App::new();
        app.update(Msg::Key(key(KeyCode::F(1))));
        // A stray character must not reach the editor behind the overlay, and
        // does not dismiss it; a swallowed no-op key needs no repaint.
        assert!(
            !app.update(Msg::Key(char_key('x'))),
            "a swallowed key is a no-op"
        );
        assert!(
            app.input.lines().iter().all(String::is_empty),
            "the editor stays empty behind the overlay"
        );
        assert!(app.help_open, "the overlay stays open");
    }

    #[test]
    fn help_overlay_closes_on_esc_or_its_own_key() {
        let mut app = App::new();
        app.update(Msg::Key(key(KeyCode::F(1))));
        assert!(
            app.update(Msg::Key(key(KeyCode::Esc))),
            "Esc repaints on close"
        );
        assert!(!app.help_open, "Esc closes the overlay");
        assert!(!app.quit, "Esc inside the overlay never quits the shell");

        // Its own binding toggles it shut too, so the operator can close with
        // whichever key they opened it.
        app.update(Msg::Key(key(KeyCode::F(1))));
        assert!(app.help_open);
        app.update(Msg::Key(key(KeyCode::F(1))));
        assert!(!app.help_open, "the help key toggles the overlay closed");
    }

    #[test]
    fn help_overlay_is_consultable_mid_turn_without_disturbing_it() {
        let mut app = App::new();
        app.update(turn_started());
        // Read-only: raising it during a live turn neither cancels nor queues.
        assert!(app.update(Msg::Key(key(KeyCode::F(1)))));
        assert!(app.help_open);
        assert!(app.turn_active, "the turn is untouched");
        assert!(!app.pending_cancel, "and not cancelled");
        assert!(app.pending_submit.is_none(), "and nothing was staged");
    }

    #[test]
    fn up_arrow_recalls_the_most_recent_submission() {
        let mut app = App::new();
        // Seed in-memory history; the empty path keeps it off the filesystem.
        app.history.push(&app.history_path, "first");
        app.history.push(&app.history_path, "second");
        assert!(app.update(Msg::Key(key(KeyCode::Up))), "Up recalls");
        assert_eq!(app.input.lines().join("\n"), "second");
        assert!(app.update(Msg::Key(key(KeyCode::Up))));
        assert_eq!(app.input.lines().join("\n"), "first");
        // Clamps at the oldest entry rather than wrapping or blanking.
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.input.lines().join("\n"), "first");
    }

    #[test]
    fn down_arrow_walks_back_toward_newer_entries() {
        let mut app = App::new();
        app.history.push(&app.history_path, "first");
        app.history.push(&app.history_path, "second");
        app.update(Msg::Key(key(KeyCode::Up)));
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.input.lines().join("\n"), "first");
        assert!(app.update(Msg::Key(key(KeyCode::Down))));
        assert_eq!(app.input.lines().join("\n"), "second");
    }

    #[test]
    fn down_past_the_newest_entry_never_blanks_a_live_draft() {
        let mut app = App::new();
        app.history.push(&app.history_path, "only");
        for c in "typing".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        // A fresh draft sits past the end of history: Down must be a no-op, not
        // a wipe (the footgun a naive REPL mirror would have).
        assert!(
            !app.update(Msg::Key(key(KeyCode::Down))),
            "Down past the newest entry consumes nothing"
        );
        assert_eq!(app.input.lines().join("\n"), "typing", "the draft survives");
    }

    #[test]
    fn submit_records_history_for_later_recall() {
        let mut app = App::new();
        for c in "hello".chars() {
            app.update(Msg::Key(char_key(c)));
        }
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(
            app.input.lines().join("\n").is_empty(),
            "submitting clears the input"
        );
        assert!(app.update(Msg::Key(key(KeyCode::Up))), "Up recalls it");
        assert_eq!(app.input.lines().join("\n"), "hello");
    }

    #[test]
    fn arrows_inside_a_multiline_draft_move_the_cursor_not_history() {
        let mut app = App::new();
        app.history.push(&app.history_path, "older");
        app.history.push(&app.history_path, "top\nbottom");
        // Recall the two-line entry; the cursor parks at its end (row 1).
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.input.lines().len(), 2, "a two-line draft recalled");
        // On row 1, Up is an editor motion (recall needs row 0), so it moves the
        // cursor within the draft rather than walking history to "older".
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(
            app.input.lines().join("\n"),
            "top\nbottom",
            "Up inside the draft leaves the text untouched"
        );
    }

    #[test]
    fn escape_and_ctrl_c_quit_when_idle() {
        let mut app = App::new();
        assert!(!app.quit);
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(app.quit, "Esc leaves the carrier shell when idle");

        let mut app = App::new();
        app.update(Msg::Key(ctrl_c()));
        assert!(app.quit, "Ctrl+C leaves the carrier shell when idle");
    }

    #[test]
    fn ctrl_c_double_tap_cancels_a_running_turn_without_quitting() {
        let mut app = App::new();
        app.update(turn_started());
        app.update(Msg::Key(ctrl_c()));
        assert!(!app.quit, "a running turn is not killed by one Ctrl+C");
        assert!(!app.take_cancel(), "first tap only arms");
        app.update(Msg::Key(ctrl_c()));
        assert!(app.take_cancel(), "second tap cancels");
        assert!(!app.quit);
    }

    #[test]
    fn a_stray_key_disarms_the_cancel_double_tap() {
        let mut app = App::new();
        app.update(turn_started());
        app.update(Msg::Key(ctrl_c()));
        app.update(Msg::Key(char_key('x')));
        app.update(Msg::Key(ctrl_c()));
        assert!(!app.take_cancel(), "the taps were not consecutive");
    }

    #[test]
    fn modified_enter_continues_a_line_instead_of_submitting() {
        // Shift+Enter and Alt+Enter must both continue the draft, mirroring
        // the REPL editor — Windows Terminal consumes Alt+Enter as its own
        // fullscreen toggle before the app sees a key, so Shift+Enter is the
        // chord that has to work (KI-001).
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut app = App::new();
            for c in "one".chars() {
                app.update(Msg::Key(char_key(c)));
            }
            assert!(app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, modifiers))));
            assert!(
                app.take_submit().is_none(),
                "a modified Enter must not submit"
            );
            for c in "two".chars() {
                app.update(Msg::Key(char_key(c)));
            }
            // Bare Enter then sends the whole multi-line draft as one turn.
            assert!(app.update(Msg::Key(key(KeyCode::Enter))));
            assert_eq!(app.take_submit().as_deref(), Some("one\ntwo"));
        }
    }

    #[test]
    fn cancel_hint_survives_stream_events_until_the_turn_settles() {
        // A tool start rewrites the transient note ("tool 1: `bash`"); the
        // armed/cancelling hint must outlive it (KI-002).
        let mut app = App::new();
        app.update(turn_started());
        app.update(Msg::Key(ctrl_c()));
        assert!(app.rendered_status_note().contains("again to cancel"));
        app.update(ev(AgentEvent::ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: "{}".into(),
        }));
        assert!(
            app.rendered_status_note().contains("again to cancel"),
            "the armed hint survives a tool start"
        );
        app.update(Msg::Key(ctrl_c()));
        assert_eq!(app.rendered_status_note(), "cancelling turn…");
        app.update(ev(AgentEvent::ToolUse {
            id: "2".into(),
            name: "read_file".into(),
            input: "{}".into(),
        }));
        assert_eq!(
            app.rendered_status_note(),
            "cancelling turn…",
            "the cancelling note survives further stream events"
        );
        app.update(finished(true, "saved x"));
        assert_eq!(
            app.rendered_status_note(),
            "saved x",
            "settle replaces the sticky override"
        );
    }

    #[test]
    fn cancel_hint_disappears_when_the_double_tap_disarms() {
        let mut app = App::new();
        app.update(turn_started());
        app.update(Msg::Key(ctrl_c()));
        assert!(app.rendered_status_note().contains("again to cancel"));
        app.update(Msg::Key(char_key('x')));
        assert_eq!(
            app.rendered_status_note(),
            "turn running · ctrl+c twice to cancel",
            "after disarm the bar falls back to the transient note"
        );
    }

    #[test]
    fn escape_is_inert_while_a_turn_runs() {
        let mut app = App::new();
        app.update(turn_started());
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(!app.quit, "Esc must not abandon a running turn");
    }

    #[test]
    fn text_deltas_accumulate_then_message_stop_flushes_to_the_transcript() {
        let mut app = App::new();
        app.update(turn_started());
        app.update(ev(AgentEvent::TextDelta("he".into())));
        app.update(ev(AgentEvent::TextDelta("llo".into())));
        assert_eq!(app.assistant_buf, "hello", "deltas append");
        assert!(
            app.transcript.is_empty(),
            "not flushed until the segment ends"
        );
        app.update(ev(AgentEvent::MessageStop));
        assert!(app.assistant_buf.is_empty(), "flush clears the buffer");
        assert_eq!(app.transcript.len(), 1, "the segment lands as one line");
    }

    #[test]
    fn tool_use_flushes_text_then_result_clears_the_marker() {
        let mut app = App::new();
        app.update(turn_started());
        app.update(ev(AgentEvent::TextDelta("let me check".into())));
        app.update(ev(AgentEvent::ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: "{}".into(),
        }));
        assert_eq!(app.running_tool.as_deref(), Some("bash"));
        assert_eq!(
            app.transcript.len(),
            1,
            "pending text flushed before the tool"
        );
        app.update(ev(AgentEvent::ToolResult {
            id: "1".into(),
            name: "bash".into(),
            output: "ok".into(),
            is_error: false,
        }));
        assert!(
            app.running_tool.is_none(),
            "the marker clears on the result"
        );
        assert!(app.transcript.len() >= 2, "the result adds its own lines");
    }

    #[test]
    fn turn_finished_clears_the_active_state() {
        let mut app = App::new();
        app.update(turn_started());
        assert!(app.turn_active);
        app.update(finished(true, "saved x"));
        assert!(!app.turn_active);
        assert_eq!(app.status_note, "saved x");
    }

    #[test]
    fn paging_scrolls_and_clamps_at_the_bottom() {
        let mut app = App::new();
        for i in 0..20 {
            app.push_line(ratatui::text::Line::from(format!("line {i}")));
        }
        app.update(Msg::Key(key(KeyCode::PageUp)));
        assert_eq!(app.scroll_up, 3, "PageUp scrolls away from the newest line");
        app.update(Msg::Key(key(KeyCode::PageDown)));
        assert_eq!(app.scroll_up, 0);
        app.update(Msg::Key(key(KeyCode::PageDown)));
        assert_eq!(app.scroll_up, 0, "PageDown clamps at the bottom");
    }

    #[test]
    fn visible_window_pins_to_the_bottom_by_default() {
        // Five single-row lines, a 3-row body, no scroll: show the last three.
        let heights = [1usize, 1, 1, 1, 1];
        let (start, sub) = visible_window(heights.len(), |i| heights[i], 3, 0);
        assert_eq!((start, sub), (2, 0));
    }

    #[test]
    fn visible_window_scrolls_up_in_composed_rows() {
        let heights = [1usize, 1, 1, 1, 1];
        // One row up lifts the window to start at source line 1.
        let (start, sub) = visible_window(heights.len(), |i| heights[i], 3, 1);
        assert_eq!((start, sub), (1, 0));
    }

    #[test]
    fn visible_window_accounts_for_wrapped_rows() {
        // A middle line wraps to 3 composed rows; the window must start inside
        // it and skip the one row that sits above the viewport.
        let heights = [1usize, 3, 1];
        let (start, sub) = visible_window(heights.len(), |i| heights[i], 3, 0);
        assert_eq!((start, sub), (1, 1));
    }

    #[test]
    fn visible_window_clamps_at_the_top() {
        // Over-scroll past the start: clamp to source line 0, no sub-offset.
        let heights = [1usize, 1, 1];
        let (start, sub) = visible_window(heights.len(), |i| heights[i], 3, 100);
        assert_eq!((start, sub), (0, 0));
    }

    #[test]
    fn wrapped_height_counts_composed_rows() {
        // A short line is one row; a long line wraps into several at width 10;
        // zero width composes to nothing.
        let short = ratatui::text::Line::from("hi");
        assert_eq!(wrapped_height(&short, 10), 1);
        assert_eq!(wrapped_height(&short, 0), 0);
        let long = ratatui::text::Line::from("aaa bbb ccc ddd");
        assert!(
            wrapped_height(&long, 10) >= 2,
            "a 15-column line wraps into multiple 10-column rows"
        );
    }

    #[test]
    fn long_tool_output_collapses_and_tab_expands_it() {
        let mut app = App::new();
        app.update(turn_started());
        let body = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.update(ev(AgentEvent::ToolResult {
            id: "1".into(),
            name: "bash".into(),
            output: body,
            is_error: false,
        }));
        // Collapsed by default: only the marker row shows.
        assert_eq!(render_items(&app.transcript).len(), 1);
        // Tab toggles the most recent tool result open (marker + 20 body rows).
        assert!(app.update(Msg::Key(key(KeyCode::Tab))));
        assert_eq!(render_items(&app.transcript).len(), 21);
        // Tab again collapses it back.
        assert!(app.update(Msg::Key(key(KeyCode::Tab))));
        assert_eq!(render_items(&app.transcript).len(), 1);
    }

    #[test]
    fn short_tool_output_starts_expanded() {
        let mut app = App::new();
        app.update(ev(AgentEvent::ToolResult {
            id: "1".into(),
            name: "read_file".into(),
            output: "one\ntwo".into(),
            is_error: false,
        }));
        // 2 lines <= the default inline threshold: marker + both rows, no fold.
        assert_eq!(render_items(&app.transcript).len(), 3);
    }

    #[test]
    fn thinking_persists_folded_and_tab_expands_it() {
        let mut app = App::new();
        app.update(turn_started());
        // Reasoning streams, then the answer, then the segment ends.
        app.update(ev(AgentEvent::ThinkingDelta(
            "step one\nstep two\nstep three".into(),
        )));
        app.update(ev(AgentEvent::TextDelta("the answer".into())));
        app.update(ev(AgentEvent::MessageStop));
        // Reasoning is process: it lands folded (one marker row) ahead of the
        // answer, so the real output is what stands out.
        assert!(
            matches!(
                app.transcript[0],
                Item::Thinking {
                    expanded: false,
                    ..
                }
            ),
            "reasoning folds by default"
        );
        // Folded: thinking marker + one answer row.
        assert_eq!(render_items(&app.transcript).len(), 2);
        // Tab expands the reasoning: marker + 3 body rows + the answer.
        assert!(app.update(Msg::Key(key(KeyCode::Tab))));
        assert_eq!(render_items(&app.transcript).len(), 5);
    }

    #[test]
    fn tab_is_a_no_op_without_tool_results() {
        let mut app = App::new();
        app.push_line(ratatui::text::Line::from("plain"));
        assert!(
            !app.update(Msg::Key(key(KeyCode::Tab))),
            "nothing to expand"
        );
    }

    #[test]
    fn settings_scroll_step_drives_paging() {
        let mut app = App::new();
        app.settings.scroll_step = 5;
        for i in 0..20 {
            app.push_line(ratatui::text::Line::from(format!("line {i}")));
        }
        app.update(Msg::Key(key(KeyCode::PageUp)));
        assert_eq!(app.scroll_up, 5, "the configured stride, not the default 3");
    }

    #[test]
    fn fold_thinking_off_starts_reasoning_expanded() {
        let mut app = App::new();
        app.settings.fold_thinking = false;
        app.update(turn_started());
        app.update(ev(AgentEvent::ThinkingDelta("a\nb".into())));
        app.update(ev(AgentEvent::MessageStop));
        assert!(
            matches!(app.transcript[0], Item::Thinking { expanded: true, .. }),
            "fold_thinking = false opens reasoning inline"
        );
    }

    #[test]
    fn zero_tool_inline_lines_folds_even_short_output() {
        let mut app = App::new();
        app.settings.tool_inline_lines = 0;
        app.update(ev(AgentEvent::ToolResult {
            id: "1".into(),
            name: "read_file".into(),
            output: "one\ntwo".into(),
            is_error: false,
        }));
        // Threshold 0 folds even a 2-line result: marker row only.
        assert_eq!(render_items(&app.transcript).len(), 1);
    }

    #[test]
    fn shell_starts_with_a_single_main_section() {
        let shell = Shell::new();
        assert_eq!(shell.sections.len(), 1);
        assert_eq!(shell.active_index(), 0);
        assert_eq!(shell.section().title, "main");
    }

    #[test]
    fn push_section_appends_and_activates_the_new_tab() {
        let mut shell = Shell::new();
        shell.push_section(String::from("2"));
        assert_eq!(shell.sections.len(), 2);
        assert_eq!(shell.active_index(), 1, "a new section becomes active");
        assert_eq!(shell.section().title, "2");
        // The first section keeps its own name: sections never share identity.
        assert_eq!(shell.sections[0].title, "main");
    }

    #[test]
    fn section_navigation_wraps_both_ways() {
        let mut shell = Shell::new();
        shell.push_section(String::from("2"));
        shell.push_section(String::from("3"));
        assert_eq!(shell.active_index(), 2);
        shell.next_section();
        assert_eq!(shell.active_index(), 0, "next wraps to the first");
        shell.prev_section();
        assert_eq!(shell.active_index(), 2, "prev wraps to the last");
        shell.prev_section();
        assert_eq!(shell.active_index(), 1);
    }

    #[test]
    fn section_navigation_is_a_no_op_with_one_section() {
        let mut shell = Shell::new();
        shell.next_section();
        shell.prev_section();
        assert_eq!(shell.active_index(), 0, "a solo session never flickers");
        assert_eq!(shell.sections.len(), 1);
    }

    #[test]
    fn sections_carry_independent_transcripts() {
        let mut shell = Shell::new();
        shell
            .section_mut()
            .push_line(ratatui::text::Line::from("only in main"));
        shell.push_section(String::from("2"));
        assert!(
            shell.section().transcript.is_empty(),
            "a fresh section starts with its own empty transcript"
        );
        shell.prev_section();
        assert_eq!(
            render_items(&shell.section().transcript).len(),
            1,
            "switching back restores the first section's history"
        );
    }

    #[test]
    fn a_running_turn_guards_section_switching() {
        let mut app = App::new();
        app.update(turn_started());
        // Ctrl+PageDown is the default next-section chord; mid-turn the reducer
        // consumes it with a guard note instead of quitting or folding.
        let next = KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL);
        assert!(app.update(Msg::Key(next)));
        assert!(
            app.status_note.contains("before switching sections"),
            "the guard explains why the switch did not happen"
        );
        assert!(!app.quit, "a guarded switch never quits the shell");
    }

    /// `draw_synced` must bracket the frame with DEC 2026 begin/end so a
    /// supporting terminal composites atomically. A real `CrosstermBackend` over
    /// an in-memory writer is used (not `TestBackend`, which emits no escapes);
    /// a fixed viewport keeps `draw` from querying a live terminal size.
    #[test]
    fn draw_synced_brackets_the_frame_with_dec_2026() {
        use ratatui::backend::CrosstermBackend;
        use ratatui::layout::Rect;
        use ratatui::widgets::Paragraph;
        use ratatui::{Terminal, TerminalOptions, Viewport};
        use std::io;
        use std::sync::{Arc, Mutex};

        // An in-memory writer shared via an `Arc`, so the emitted escape bytes
        // can be inspected without ratatui's unstable `writer()` accessor.
        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().expect("writer lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let shared = SharedBuf::default();
        let backend = CrosstermBackend::new(shared.clone());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 8, 2)),
            },
        )
        .expect("terminal over an in-memory writer");
        draw_synced(&mut terminal, |frame: &mut ratatui::Frame| {
            frame.render_widget(Paragraph::new("hi"), frame.area());
        })
        .expect("draw_synced");
        let bytes = shared.0.lock().expect("writer lock").clone();
        let out = String::from_utf8_lossy(&bytes);
        let begin = out
            .find("\x1b[?2026h")
            .expect("begin synchronized update emitted");
        let end = out
            .find("\x1b[?2026l")
            .expect("end synchronized update emitted");
        assert!(begin < end, "the frame is bracketed begin…end, in order");
    }

    /// Render `shell` into a `w×h` test backend and return the composed rows as
    /// trimmed strings, so layout assertions read the same cells a terminal
    /// would. This is the golden harness for the pure `view` projection: it
    /// verifies chrome (header, tab bar, budget) without a live terminal.
    fn render_rows(shell: &Shell, w: u16, h: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).expect("test backend");
        term.draw(|frame| shell.view(frame)).expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn header_shows_identity_and_context_budget() {
        let mut shell = Shell::new();
        shell.chrome = Chrome {
            model: "mimo-v2.5-pro".to_string(),
            cwd: "~/proj".to_string(),
            context_window: 100_000,
        };
        // Half the window consumed → the 50% accent-pressure boundary.
        shell.section_mut().last_usage = Some(TokenUsage {
            input_tokens: 40_000,
            output_tokens: 5,
            cache_read_input_tokens: 10_000,
            cache_creation_input_tokens: 0,
        });
        let rows = render_rows(&shell, 80, 8);
        assert!(rows[0].contains("heartflow"), "header names the app");
        assert!(rows[0].contains("mimo-v2.5-pro"), "header names the model");
        assert!(rows[0].contains("~/proj"), "header names the cwd");
        assert!(rows[0].contains("[ctx 50%]"), "header shows the budget");
    }

    #[test]
    fn solo_section_hides_the_tab_bar() {
        let shell = Shell::new();
        let rows = render_rows(&shell, 60, 8);
        // Row 0 is the header; row 1 is the transcript's top border, not a tab
        // title — a solo session shows no lonely tab. Default chrome has no
        // window, so no budget readout either.
        assert!(rows[0].contains("heartflow"));
        assert!(!rows[0].contains("ctx"), "no window → no budget");
        assert!(
            rows[1].starts_with('\u{250c}'),
            "row 1 is the transcript border, not a tab bar"
        );
    }

    #[test]
    fn multiple_sections_render_a_tab_bar_under_the_header() {
        let mut shell = Shell::new();
        shell.push_section("2".to_string());
        let rows = render_rows(&shell, 60, 8);
        assert!(rows[0].contains("heartflow"), "header still tops the frame");
        assert!(
            rows[1].contains("main") && rows[1].contains('2'),
            "row 1 is the tab bar listing both sections"
        );
        assert!(
            rows[2].starts_with('\u{250c}'),
            "the transcript drops one row to make room for the bar"
        );
    }

    #[test]
    fn context_budget_reads_overrun_truthfully() {
        let mut shell = Shell::new();
        shell.chrome = Chrome {
            context_window: 1_000,
            ..Chrome::default()
        };
        shell.section_mut().last_usage = Some(TokenUsage {
            input_tokens: 900,
            output_tokens: 0,
            cache_read_input_tokens: 200,
            cache_creation_input_tokens: 0,
        });
        // (900 + 200) / 1000 = 110% — the readout reports the overrun instead of
        // silently clamping to 100, so a blown window is visible.
        let rows = render_rows(&shell, 40, 6);
        assert!(
            rows[0].contains("[ctx 110%]"),
            "over-budget reads above 100%"
        );
    }

    #[test]
    fn status_context_warning_uses_full_input_accounting() {
        let mut shell = Shell::new();
        shell.chrome = Chrome {
            context_window: 1_000,
            ..Chrome::default()
        };
        // Cache reads carry 85% of the window; raw `input_tokens` alone would
        // read 10% and the warning would never fire. Output is excluded from
        // the budget, so a large `output_tokens` must not inflate it either.
        shell.section_mut().last_usage = Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 900,
            cache_read_input_tokens: 750,
            cache_creation_input_tokens: 0,
        });
        // Wide enough that the status line's context readout isn't clipped.
        let rows = render_rows(&shell, 160, 8);
        let status = &rows[rows.len() - 1];
        assert!(
            status.contains("[context: 85%]"),
            "status warns on input + cache, got: {status}"
        );
        assert!(!status.contains("near limit"), "85% is below the 90% cliff");
    }

    /// Render a single section's `view` into a test backend and return its rows.
    /// Used to prove the memoized transcript projection is invalidated (rebuilt)
    /// whenever the transcript mutates, so a stale cache can never linger.
    fn render_app_rows(app: &App, w: u16, h: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).expect("test backend");
        term.draw(|frame| view(app, frame.area(), frame, None))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn render_cache_invalidates_on_transcript_push() {
        let mut app = App::new();
        // Prime the cache with an empty transcript. The backend must be tall
        // enough that the transcript body (height minus borders, input, status)
        // is non-zero, or nothing would render regardless of the cache.
        let primed = render_app_rows(&app, 40, 12);
        assert!(!primed.iter().any(|row| row.contains("hello world")));
        // A flushed line lands in the transcript; the next frame must rebuild.
        app.push_line(ratatui::text::Line::from("hello world"));
        let after = render_app_rows(&app, 40, 12);
        assert!(
            after.iter().any(|row| row.contains("hello world")),
            "a transcript push invalidates the cached projection"
        );
    }

    #[test]
    fn render_cache_invalidates_on_fold_toggle() {
        let mut app = App::new();
        // 50 lines exceeds the default inline threshold, so it starts folded.
        app.push_tool("bash".to_string(), false, &"line\n".repeat(50));
        // A tall backend keeps the marker row visible even once expanded (the
        // marker + all 50 body rows fit), so the assertion reads the marker.
        let folded = render_app_rows(&app, 60, 60);
        assert!(
            folded.iter().any(|row| row.contains("Tab expand")),
            "a long tool result starts folded"
        );
        assert!(app.toggle_last_process(), "the toggle changes the fold");
        let expanded = render_app_rows(&app, 60, 60);
        assert!(
            expanded.iter().any(|row| row.contains("Tab collapse")),
            "a fold toggle invalidates the cached projection"
        );
    }
}

/// Loop-level tests for the three-phase event loop (roadmap §11.0).
///
/// These drive the *real* [`drive_loop`] — not a mock of it — with a scripted
/// API client, a recording draw seam and a recording saver, so no terminal,
/// provider key or home-directory write is involved. The loop is
/// `spawn_local`'d on a `LocalSet` because its future is `!Send` (the runtime
/// carries a `Cell`), exactly like production; an orchestration task feeds it
/// keys over the shared channel and observes rendered state through the draw
/// seam. A gated script entry makes "the turn is live" a deterministic state
/// the orchestration can wait for — which is what makes the mid-turn pumping
/// assertions below race-free.
#[cfg(test)]
mod loop_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    use runtime::{
        ApiRequest, ContentBlock, ConversationMessage, PermissionMode, PermissionPolicy, Session,
        StaticToolExecutor, TurnStream,
    };
    use tokio::sync::{mpsc, oneshot, Notify};

    use crate::new_session_state;

    /// What the draw seam observes about the shell at one frame. The loop
    /// draws whenever it is dirty, so a predicate over these snapshots is the
    /// test's window into mid-turn state.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct FrameSnapshot {
        turn_active: bool,
        /// Which section the live turn belongs to (§11.4): `None` when idle.
        running_section: Option<usize>,
        scroll_up: u16,
        sections: usize,
        transcript_len: usize,
        requests: usize,
        status_note: String,
    }

    /// One scripted provider turn.
    struct ScriptEntry {
        events: Vec<AgentEvent>,
        /// Hold the stream until the test opens the gate (`None` = emit at
        /// once), so "the turn is live" is a state the orchestration can wait
        /// for deterministically.
        gate: Option<Arc<Notify>>,
    }

    impl ScriptEntry {
        fn immediate(events: Vec<AgentEvent>) -> Self {
            Self { events, gate: None }
        }

        fn gated(events: Vec<AgentEvent>, gate: Arc<Notify>) -> Self {
            Self {
                events,
                gate: Some(gate),
            }
        }

        /// A minimal complete reply: one text delta plus the stop event.
        fn text(text: &str) -> Vec<AgentEvent> {
            vec![
                AgentEvent::TextDelta(text.to_string()),
                AgentEvent::MessageStop,
            ]
        }
    }

    /// Scripted API client: each `stream` call pops the next scripted batch
    /// (released by its gate) and records the request it was handed, so a test
    /// observes both what the loop sent and what the turn produced.
    struct ScriptedClient {
        scripts: VecDeque<ScriptEntry>,
        log: Arc<Mutex<Vec<ApiRequest>>>,
    }

    impl ApiClient for ScriptedClient {
        fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
            self.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            let Some(entry) = self.scripts.pop_front() else {
                // Out of scripts: end with nothing, so the runtime reports its
                // normal "no content" failure instead of hanging the test.
                return Ok(TurnStream::from_events(Vec::new()));
            };
            match entry.gate {
                Some(gate) => {
                    let (tx, rx) = mpsc::channel(entry.events.len().max(1));
                    let handle = tokio::spawn(async move {
                        gate.notified().await;
                        for event in entry.events {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                    });
                    Ok(TurnStream::new(rx, handle.abort_handle()))
                }
                None => Ok(TurnStream::from_events(entry.events)),
            }
        }
    }

    /// The test [`TurnSaver`]: counts calls and never touches the disk.
    struct RecordingSaver {
        calls: Arc<AtomicUsize>,
    }

    impl TurnSaver for RecordingSaver {
        async fn save(
            &mut self,
            _state: &SessionShared,
            _session: &Session,
        ) -> io::Result<PathBuf> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(PathBuf::from("recording-saver"))
        }
    }

    /// Everything the orchestration needs to drive and observe one loop.
    struct TestControls {
        msg_tx: mpsc::UnboundedSender<Msg>,
        frames: Arc<Mutex<Vec<FrameSnapshot>>>,
        gates: Vec<Arc<Notify>>,
        /// Frames already scanned by [`Self::wait_for`]. Each wait only
        /// considers frames drawn after the previous one, so a stale frame can
        /// never satisfy a later condition.
        scanned: AtomicUsize,
    }

    impl TestControls {
        fn key(&self, key: KeyEvent) {
            let _ = self.msg_tx.send(Msg::Key(key));
        }

        /// Type `text` into the input and press submit — the same key path an
        /// operator uses, never a back door into the model.
        fn submit_text(&self, text: &str) {
            for c in text.chars() {
                self.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }

        fn interrupt(&self) {
            self.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        }

        fn open_gate(&self, index: usize) {
            if let Some(gate) = self.gates.get(index) {
                gate.notify_one();
            }
        }

        fn frames(&self) -> Vec<FrameSnapshot> {
            self.frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// Spin until a frame drawn *after the frame that satisfied the last
        /// wait* satisfies `predicate`, so tests synchronize on fresh rendered
        /// state instead of on timing — and never on a stale frame.
        async fn wait_for(&self, predicate: impl Fn(&FrameSnapshot) -> bool) {
            let mut watermark = self.scanned.load(Ordering::SeqCst);
            for _ in 0..750 {
                let frames = self.frames();
                if let Some(fresh) = frames.get(watermark..) {
                    if let Some(index) = fresh.iter().position(&predicate) {
                        // Resume after the matched frame, not after everything
                        // drawn so far: later waits must still see frames that
                        // already exist by the time they start.
                        self.scanned.store(watermark + index + 1, Ordering::SeqCst);
                        return;
                    }
                }
                watermark = watermark.min(frames.len());
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            panic!("condition never observed; frames: {:?}", self.frames());
        }
    }

    /// What one loop test asserts on after the loop finished.
    struct LoopReport {
        loop_ok: bool,
        frames: Vec<FrameSnapshot>,
        saves: usize,
        requests: Vec<ApiRequest>,
        /// Terminal ledger of every section's queue after the loop ended
        /// ("#id state text"), for asserting delivery outcomes (§11.4).
        ledger: Vec<String>,
    }

    /// Run `orchestrate` against a live [`drive_loop`] on a `LocalSet`. The
    /// orchestration is expected to end by asking the loop to quit (Ctrl+C
    /// while idle); the runner then gives the loop a bounded moment to unwind
    /// and report.
    async fn run_loop_test<F, Fut>(scripts: Vec<ScriptEntry>, orchestrate: F) -> LoopReport
    where
        F: FnOnce(TestControls) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let saves = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gates: Vec<Arc<Notify>> = scripts
            .iter()
            .filter_map(|script| script.gate.clone())
            .collect();
        let client = ScriptedClient {
            scripts: scripts.into(),
            log: Arc::clone(&requests),
        };
        let runtime = ConversationRuntime::new(
            Session::new(),
            client,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::Allow),
            Vec::new(),
        );
        // The section factory every `Ctrl+T` uses: a fresh state + runtime
        // sharing the request log, with one canned reply scripted.
        let section_log = Arc::clone(&requests);
        let make_section = move || {
            Ok((
                new_session_state(),
                ConversationRuntime::new(
                    Session::new(),
                    ScriptedClient {
                        scripts: vec![ScriptEntry::immediate(ScriptEntry::text("section reply"))]
                            .into(),
                        log: Arc::clone(&section_log),
                    },
                    StaticToolExecutor::new(),
                    PermissionPolicy::new(PermissionMode::Allow),
                    Vec::new(),
                ),
            ))
        };
        let mut shell = Shell::new();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel::<Msg>();
        let (choice_tx, choice_rx) = mpsc::unbounded_channel::<PermissionChoice>();
        let control_tx = msg_tx.clone();
        let observed_frames = Arc::clone(&frames);
        let observed_requests = Arc::clone(&requests);
        let draw = move |shell: &Shell| {
            observed_frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(FrameSnapshot {
                    turn_active: shell.section().turn_active,
                    running_section: shell
                        .sections
                        .iter()
                        .position(|section| section.turn_active),
                    scroll_up: shell.section().scroll_up,
                    sections: shell.sections.len(),
                    transcript_len: shell.section().transcript.len(),
                    requests: observed_requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .len(),
                    status_note: shell.section().rendered_status_note().into_owned(),
                });
            Ok(())
        };
        let recording_saver = RecordingSaver {
            calls: Arc::clone(&saves),
        };
        // Empty temp dirs: the watcher sees no config change, so a turn
        // boundary never reloads a surface mid-test.
        let tag = std::process::id();
        let cwd = std::env::temp_dir().join(format!("hf-loop-cwd-{tag}"));
        let home = std::env::temp_dir().join(format!("hf-loop-home-{tag}"));
        std::fs::create_dir_all(&cwd).ok();
        std::fs::create_dir_all(&home).ok();
        let mut watcher = ConfigWatcher::new(&cwd, &home);
        // The loop's result is recorded beside the channel rather than moved
        // through it: the runner's bounded wait consumes the channel signal,
        // and the outcome must survive that.
        let outcome = Arc::new(Mutex::new(None));
        let ledger_out: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, mut done_rx) = oneshot::channel::<()>();
        let recorded = Arc::clone(&outcome);
        let recorded_ledger = Arc::clone(&ledger_out);
        let local = tokio::task::LocalSet::new();
        local.spawn_local(async move {
            let result = drive_loop(
                &mut shell,
                vec![new_session_state()],
                vec![Some(runtime)],
                make_section,
                &mut watcher,
                &cwd,
                &home,
                LoopChannels {
                    msg_tx,
                    msg_rx,
                    choice_tx,
                    choice_rx,
                },
                draw,
                recording_saver,
            )
            .await;
            // The shell drops with this task; snapshot its queue ledgers on
            // the way out so tests can assert delivery outcomes (§11.4).
            *recorded_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = shell
                .sections
                .iter()
                .flat_map(|section| section.model.queue().recent())
                .map(|entry| format!("#{} {} {}", entry.id, entry.state.label(), entry.text))
                .collect();
            *recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
            let _ = done_tx.send(());
        });
        let controls = TestControls {
            msg_tx: control_tx,
            frames: Arc::clone(&frames),
            gates,
            scanned: AtomicUsize::new(0),
        };
        local
            .run_until(async {
                orchestrate(controls).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), &mut done_rx).await;
            })
            .await;
        // Clone the recorders into locals first: a `MutexGuard` temporary in
        // the struct literal would outlive the bindings it borrows.
        let frames = frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let loop_ok = matches!(
            outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref(),
            Some(Ok(()))
        );
        let ledger = ledger_out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        LoopReport {
            loop_ok,
            frames,
            saves: saves.load(Ordering::SeqCst),
            requests,
            ledger,
        }
    }

    /// All user-visible text in one request, for merge assertions.
    fn request_texts(request: &ApiRequest) -> Vec<String> {
        request
            .messages
            .iter()
            .flat_map(|message: &ConversationMessage| &message.blocks)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_turn_runs_to_completion_and_persists() {
        let report = run_loop_test(
            vec![ScriptEntry::immediate(ScriptEntry::text("hello"))],
            |controls| async move {
                controls.submit_text("hi");
                controls.wait_for(|frame| frame.turn_active).await;
                controls
                    .wait_for(|frame| !frame.turn_active && frame.transcript_len > 0)
                    .await;
                // Idle now: one interrupt quits.
                controls.interrupt();
            },
        )
        .await;
        assert!(report.loop_ok, "the loop returned cleanly");
        assert_eq!(report.saves, 1, "one turn, one save");
        assert_eq!(report.requests.len(), 1, "one turn, one request");
        let last = report.frames.last().expect("frames were drawn");
        assert!(
            last.status_note.contains("saved"),
            "the status bar reports the save, got {:?}",
            last.status_note
        );
    }

    /// The regression roadmap §11.0 exists for: the loop used to park inside
    /// the turn, so a key pressed mid-turn was not reduced until the turn
    /// finished. Here the turn is deterministically live (gated stream), so
    /// the scroll key must be reduced *while it runs*.
    #[tokio::test]
    async fn keys_reach_the_reducer_while_a_turn_is_live() {
        let gate = Arc::new(Notify::new());
        let report = run_loop_test(
            vec![ScriptEntry::gated(ScriptEntry::text("late"), gate)],
            |controls| async move {
                controls.submit_text("hi");
                controls.wait_for(|frame| frame.turn_active).await;
                controls.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
                controls
                    .wait_for(|frame| frame.turn_active && frame.scroll_up > 0)
                    .await;
                controls.open_gate(0);
                controls.wait_for(|frame| !frame.turn_active).await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(
            report.saves, 1,
            "the turn still persisted after the gate opened"
        );
        assert!(
            report
                .frames
                .iter()
                .any(|frame| frame.turn_active && frame.scroll_up > 0),
            "a key was reduced while the turn was live"
        );
    }

    #[tokio::test]
    async fn follow_ups_submitted_during_a_turn_run_one_per_turn() {
        // §11.4/D2.2: the TUI now delivers per entry (rotation granularity),
        // not as one merged injection — each queued follow-up becomes its own
        // turn in arrival order.
        let gate = Arc::new(Notify::new());
        let report = run_loop_test(
            vec![
                ScriptEntry::gated(ScriptEntry::text("first"), gate),
                ScriptEntry::immediate(ScriptEntry::text("second")),
                ScriptEntry::immediate(ScriptEntry::text("third")),
            ],
            |controls| async move {
                controls.submit_text("one");
                controls.wait_for(|frame| frame.turn_active).await;
                controls.submit_text("two");
                controls.submit_text("three");
                controls
                    .wait_for(|frame| frame.turn_active && frame.status_note.contains("queued"))
                    .await;
                controls.open_gate(0);
                // Each follow-up is its own turn; wait for the *last* one to
                // settle (queue then empty, nothing left for the rotation to
                // stage) before quitting.
                for expected in [2usize, 3] {
                    controls
                        .wait_for(move |frame| frame.requests == expected)
                        .await;
                }
                controls
                    .wait_for(|frame| frame.requests == 3 && frame.running_section.is_none())
                    .await;
                // Robust exit: whatever is live gets cancelled, then the idle
                // press quits.
                for _ in 0..3 {
                    controls.interrupt();
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(report.saves, 3, "three turns persisted");
        assert_eq!(report.requests.len(), 3, "one turn per follow-up");
        assert!(
            request_texts(&report.requests[0])
                .iter()
                .any(|text| text.contains("one")),
            "the first turn ran the first submit"
        );
        assert!(
            request_texts(&report.requests[1])
                .iter()
                .any(|text| text.contains("two") && !text.contains("three")),
            "the second turn is the first follow-up alone"
        );
        assert!(
            request_texts(&report.requests[2])
                .iter()
                .any(|text| text.contains("three")),
            "the third turn is the second follow-up"
        );
    }

    #[tokio::test]
    async fn double_interrupt_cancels_the_live_turn() {
        let gate = Arc::new(Notify::new());
        let report = run_loop_test(
            vec![ScriptEntry::gated(ScriptEntry::text("never"), gate)],
            |controls| async move {
                controls.submit_text("hi");
                controls.wait_for(|frame| frame.turn_active).await;
                controls.interrupt();
                controls.interrupt();
                controls
                    .wait_for(|frame| {
                        !frame.turn_active && frame.status_note.contains("turn failed")
                    })
                    .await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(report.saves, 1, "a cancelled turn still persists");
        assert!(
            report.frames.iter().any(|frame| frame.transcript_len > 0),
            "the cancellation surfaces in the transcript"
        );
    }

    #[tokio::test]
    async fn the_armed_cancel_hint_survives_streaming_tool_events() {
        // KI-002 at loop level: while the double-tap is armed, a tool start
        // must not erase the "press again to cancel" hint from the rendered
        // status bar. The tool result landing in the transcript proves the
        // stream events were processed past the overwrite point. The agent
        // loop calls the API a second time after the tool round, so the
        // script carries the closing text reply as its own entry.
        let gate = Arc::new(Notify::new());
        let tool_round = vec![
            AgentEvent::ToolUse {
                id: "1".into(),
                name: "bash".into(),
                input: "{}".into(),
            },
            AgentEvent::MessageStop,
        ];
        let report = run_loop_test(
            vec![
                ScriptEntry::gated(tool_round, gate),
                ScriptEntry::immediate(ScriptEntry::text("after the tool")),
            ],
            |controls| async move {
                controls.submit_text("hi");
                controls.wait_for(|frame| frame.turn_active).await;
                // First tap arms; the hint must be on screen.
                controls.interrupt();
                controls
                    .wait_for(|frame| frame.status_note.contains("again to cancel"))
                    .await;
                // Release the stream: the tool start rewrites the transient
                // note, yet the armed hint must still render after the tool
                // result is in the transcript.
                controls.open_gate(0);
                controls
                    .wait_for(|frame| {
                        frame.status_note.contains("again to cancel") && frame.transcript_len >= 2
                    })
                    .await;
                // The turn runs to completion (never cancelled here): settle
                // replaces the sticky hint.
                controls
                    .wait_for(|frame| !frame.turn_active && frame.status_note.contains("saved"))
                    .await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(report.saves, 1);
    }

    #[tokio::test]
    async fn a_steer_joins_the_running_turns_next_request() {
        // §11.2 end to end: Ctrl+S mid-turn puts the input into the live
        // turn; the runtime absorbs it at the iteration boundary after the
        // tool round, so the *second* request carries it as a user message.
        let gate = Arc::new(Notify::new());
        let tool_round = vec![
            AgentEvent::ToolUse {
                id: "1".into(),
                name: "bash".into(),
                input: "{}".into(),
            },
            AgentEvent::MessageStop,
        ];
        let report = run_loop_test(
            vec![
                ScriptEntry::gated(tool_round, gate),
                ScriptEntry::immediate(ScriptEntry::text("steered ack")),
            ],
            |controls| async move {
                controls.submit_text("go");
                controls.wait_for(|frame| frame.turn_active).await;
                for c in "STEER-NOW".chars() {
                    controls.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
                }
                controls.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
                controls
                    .wait_for(|frame| frame.status_note.contains("steering"))
                    .await;
                controls.open_gate(0);
                controls
                    .wait_for(|frame| !frame.turn_active && frame.status_note.contains("saved"))
                    .await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(report.saves, 1);
        assert_eq!(report.requests.len(), 2, "tool round then steered round");
        assert!(
            request_texts(&report.requests[1])
                .iter()
                .any(|text| text.contains("STEER-NOW")),
            "the steered text rides the second request: {:?}",
            request_texts(&report.requests[1])
        );
        assert!(
            !request_texts(&report.requests[0])
                .iter()
                .any(|text| text.contains("STEER-NOW")),
            "the first request was already in flight when the steer landed"
        );
    }

    #[tokio::test]
    async fn a_late_steer_is_recovered_as_the_next_turn() {
        // The tool-less turn settles before it can consume the steer; the
        // entry must be demoted to a follow-up and run as the next turn —
        // recovered, never lost (pi's "child completed before consuming
        // steering").
        let gate = Arc::new(Notify::new());
        let report = run_loop_test(
            vec![
                ScriptEntry::gated(ScriptEntry::text("plain reply"), gate),
                ScriptEntry::immediate(ScriptEntry::text("after recovery")),
            ],
            |controls| async move {
                controls.submit_text("go");
                controls.wait_for(|frame| frame.turn_active).await;
                for c in "LATE-STEER".chars() {
                    controls.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
                }
                controls.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
                controls
                    .wait_for(|frame| frame.status_note.contains("steering"))
                    .await;
                controls.open_gate(0);
                // Turn one settles without consuming; the recovered steer
                // then runs as turn two.
                controls
                    .wait_for(|frame| !frame.turn_active && frame.status_note.contains("saved"))
                    .await;
                controls.wait_for(|frame| frame.requests == 2).await;
                controls
                    .wait_for(|frame| !frame.turn_active && frame.status_note.contains("saved"))
                    .await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(report.saves, 2, "both the turn and the recovered steer ran");
        assert!(
            request_texts(&report.requests[1])
                .iter()
                .any(|text| text.contains("LATE-STEER")),
            "the unconsumed steer becomes the next turn: {:?}",
            request_texts(&report.requests[1])
        );
    }

    #[tokio::test]
    async fn a_withdrawn_queue_entry_never_runs() {
        // §11.1 P-C at loop level: a follow-up queued mid-turn can be
        // withdrawn through the panel before the settle boundary — it must
        // never reach the provider.
        let gate = Arc::new(Notify::new());
        let report = run_loop_test(
            vec![ScriptEntry::gated(ScriptEntry::text("first reply"), gate)],
            |controls| async move {
                controls.submit_text("go");
                controls.wait_for(|frame| frame.turn_active).await;
                controls.submit_text("extra work");
                controls
                    .wait_for(|frame| frame.turn_active && frame.status_note.contains("queued"))
                    .await;
                // Open the panel (Ctrl+Q), withdraw the selected entry, close.
                controls.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
                controls.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
                controls.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                controls.open_gate(0);
                controls
                    .wait_for(|frame| !frame.turn_active && frame.status_note.contains("saved"))
                    .await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(
            report.requests.len(),
            1,
            "the withdrawn follow-up never ran: {:?}",
            report.requests.len()
        );
        assert_eq!(report.saves, 1);
    }

    #[tokio::test]
    async fn section_navigation_works_while_a_turn_is_live() {
        // §11.4 lifted the idle-only nav block: with section-tagged routing a
        // live turn never depends on which section is on screen, so switching
        // mid-turn must work and the turn must keep running in its section.
        let gate = Arc::new(Notify::new());
        let report = run_loop_test(
            vec![ScriptEntry::gated(ScriptEntry::text("late"), gate)],
            |controls| async move {
                controls.submit_text("hi");
                controls
                    .wait_for(|frame| frame.running_section == Some(0))
                    .await;
                // Ctrl+PageDown is NextSection: mid-turn it must switch.
                controls.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL));
                // Single section: NextSection wraps to itself, so prove the
                // path by opening a second section instead (Ctrl+T).
                controls.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
                controls.wait_for(|frame| frame.sections == 2).await;
                // The turn is still running in section 0 while section 1 is
                // on screen; releasing the gate settles it there.
                controls.open_gate(0);
                controls
                    .wait_for(|frame| frame.running_section.is_none())
                    .await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(report.saves, 1);
        assert!(
            report
                .frames
                .iter()
                .any(|frame| frame.sections == 2 && frame.running_section == Some(0)),
            "the turn kept running in its section after the switch"
        );
    }

    #[tokio::test]
    async fn queued_work_rotates_into_a_background_section() {
        // The §11.4 headline: work queued in section 1 keeps running (one
        // entry per turn, round-robin) while the operator has switched to
        // section 2.
        let first_gate = Arc::new(Notify::new());
        let second_gate = Arc::new(Notify::new());
        let report = run_loop_test(
            vec![
                ScriptEntry::gated(ScriptEntry::text("first"), first_gate),
                ScriptEntry::gated(ScriptEntry::text("queued reply"), second_gate),
            ],
            |controls| async move {
                controls.submit_text("go");
                controls
                    .wait_for(|frame| frame.running_section == Some(0))
                    .await;
                controls.submit_text("follow-up work");
                controls
                    .wait_for(|frame| frame.status_note.contains("queued"))
                    .await;
                // Turn one settles; the rotation stages the queued entry as
                // turn two in section 0 — parked on its own gate.
                controls.open_gate(0);
                controls
                    .wait_for(|frame| frame.running_section == Some(0) && frame.requests == 2)
                    .await;
                // Switch to a fresh section while the background turn runs.
                controls.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
                controls.wait_for(|frame| frame.sections == 2).await;
                // Release the background turn: it settles in section 0 while
                // section 1 is on screen.
                controls.open_gate(1);
                controls
                    .wait_for(|frame| frame.running_section.is_none())
                    .await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(report.requests.len(), 2);
        assert_eq!(report.saves, 2);
        assert!(
            request_texts(&report.requests[1])
                .iter()
                .any(|text| text.contains("follow-up work")),
            "the rotation delivered the queued entry verbatim"
        );
        assert!(
            report
                .ledger
                .iter()
                .any(|row| row.contains("delivered") && row.contains("follow-up work")),
            "the ledger terminalizes the rotation delivery: {:?}",
            report.ledger
        );
    }

    #[tokio::test]
    async fn a_poisoned_queue_entry_fails_after_one_retry() {
        // The rotation is automatic, so a failing entry must not retry
        // forever: one automatic re-queue, then the second consecutive
        // failure files the entry as `Failed` in the ledger.
        let gate = Arc::new(Notify::new());
        let report = run_loop_test(
            vec![ScriptEntry::gated(ScriptEntry::text("ok"), gate)],
            |controls| async move {
                controls.submit_text("go");
                controls
                    .wait_for(|frame| frame.running_section == Some(0))
                    .await;
                // The follow-up has no script left: its turn fails (empty
                // stream), twice in a row.
                controls.submit_text("poison");
                controls
                    .wait_for(|frame| frame.status_note.contains("queued"))
                    .await;
                controls.open_gate(0);
                // Turn one settles, the poison entry runs and fails, is
                // re-queued, fails again, and terminalizes. Requests stop at
                // 2 (the good turn + the first poison attempt... plus the
                // retry = 3 total API calls).
                for expected in [2usize, 3] {
                    controls
                        .wait_for(move |frame| frame.requests == expected)
                        .await;
                }
                // Nothing left to run: idle again.
                controls
                    .wait_for(|frame| frame.running_section.is_none())
                    .await;
                controls.interrupt();
            },
        )
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(
            report.requests.len(),
            3,
            "one good turn, then exactly two attempts at the poisoned entry"
        );
        assert!(
            report
                .ledger
                .iter()
                .any(|row| row.contains("failed") && row.contains("poison")),
            "the second consecutive failure terminalizes as Failed: {:?}",
            report.ledger
        );
        assert!(
            !report
                .ledger
                .iter()
                .any(|row| row.contains("pending") && row.contains("poison")),
            "the poisoned entry never lingers as pending"
        );
    }

    #[tokio::test]
    async fn a_new_section_spawns_while_idle_and_runs_its_own_turn() {
        // No submit ever happens in section 1, so its client never streams;
        // the section-2 turn runs on the factory client, which shares the log.
        let report = run_loop_test(Vec::new(), |controls| async move {
            // Ctrl+T spawns section 2, which activates immediately.
            controls.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
            controls.wait_for(|frame| frame.sections == 2).await;
            controls.submit_text("in section two");
            controls.wait_for(|frame| frame.turn_active).await;
            controls
                .wait_for(|frame| !frame.turn_active && frame.requests == 1)
                .await;
            controls.interrupt();
        })
        .await;
        assert!(
            report.loop_ok,
            "the loop did not exit cleanly; frames: {:?}",
            report.frames
        );
        assert_eq!(report.saves, 1, "the new section's turn persisted");
        assert_eq!(report.requests.len(), 1);
        assert!(
            request_texts(&report.requests[0])
                .iter()
                .any(|text| text.contains("in section two")),
            "the new section ran its own turn"
        );
    }
}
