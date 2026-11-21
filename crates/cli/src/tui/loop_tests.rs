//! Loop-level tests for the three-phase event loop (roadmap §11.0).
//!
//! These drive the *real* [`drive_loop`] — not a mock of it — with a scripted
//! API client, a recording draw seam and a recording saver, so no terminal,
//! provider key or home-directory write is involved. The loop is
//! `spawn_local`'d on a `LocalSet` because its future is `!Send` (the runtime
//! carries a `Cell`), exactly like production; an orchestration task feeds it
//! keys over the shared channel and observes rendered state through the draw
//! seam. A gated script entry makes "the turn is live" a deterministic state
//! the orchestration can wait for — which is what makes the mid-turn pumping
//! assertions below race-free.

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
    async fn save(&mut self, _state: &SessionShared, _session: &Session) -> io::Result<PathBuf> {
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
                .wait_for(|frame| !frame.turn_active && frame.status_note.contains("turn failed"))
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
