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
    auto_record_turn, turn_reminder, turn_token_budget, AgentEvent, ApiClient, ConversationRuntime,
    MissionLedger, PermissionPromptDecision, PermissionPrompter, PermissionRequest, RuntimeError,
    SteerMailbox, TokenUsage, ToolExecutor, TurnSummary,
};

use crate::autopilot::{
    budget_stop, continue_nudge, single_active_mission_id, transport_backoff, MAX_TRANSPORT_RETRIES,
};
use crate::config::ConfigWatcher;
use crate::core::{
    guide_context, guide_draft, DeliveryMode, GuideContext, HeartModel, QueueSource,
};
use crate::editor::{display_col, tui_input, History};
use crate::keymap::{Action, Keymap};
use crate::markdown;
use crate::settings::Settings;
use crate::theme::Theme;
use crate::{expand_attachments, AgentRuntime, DiskSaver, SessionShared, TurnSaver};

mod fold;
mod render;
mod stream;

#[allow(clippy::wildcard_imports)]
use fold::*;
#[allow(clippy::wildcard_imports)]
use render::*;
pub(crate) use render::{Chrome, Shell};
#[allow(clippy::wildcard_imports)]
use stream::*;

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
    /// Staged by the CycleMode/ToggleWeb keybinds; drained by the loop, which
    /// owns the runtime (mode policy) and the shared web atomic.
    pending_mode_cycle: bool,
    pending_web_cycle: bool,
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
            pending_mode_cycle: false,
            pending_web_cycle: false,
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
            // Staged for the loop: mode needs the section runtime (idle-gated
            // there); web flips the shared atomic from the loop level.
            Some(Action::CycleMode) => {
                self.pending_mode_cycle = true;
                self.status_note = String::from("cycling mode…");
                true
            }
            Some(Action::ToggleWeb) => {
                self.pending_web_cycle = true;
                self.status_note = String::from("cycling web access…");
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
                self.push_line(muted_line(format!("· turn truncated ({reason})")));
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
            AgentEvent::Error { message, hint, .. } => {
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

    /// Drain the staged mode-cycle / web-cycle keybinds (C-1).
    fn take_mode_cycle(&mut self) -> bool {
        std::mem::take(&mut self.pending_mode_cycle)
    }

    fn take_web_cycle(&mut self) -> bool {
        std::mem::take(&mut self.pending_web_cycle)
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod loop_tests;
