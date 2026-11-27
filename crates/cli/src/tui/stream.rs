use super::*;

/// The channel plumbing one loop instance owns: the shared message bus (keys
/// from the pump, events from the turn) plus the permission decision pair the
/// prompter parks on. Grouped so [`drive_loop`]'s parameter list stays
/// readable.
pub(super) struct LoopChannels {
    pub(super) msg_tx: mpsc::UnboundedSender<Msg>,
    pub(super) msg_rx: mpsc::UnboundedReceiver<Msg>,
    pub(super) choice_tx: mpsc::UnboundedSender<PermissionChoice>,
    pub(super) choice_rx: mpsc::UnboundedReceiver<PermissionChoice>,
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
pub(super) async fn event_loop<F>(
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
/// [`crate::permissions::set_runtime_mode_policy`] generic over the runtime's
/// executor: the drive loop holds `ConversationRuntime<C, T>` where T is only
/// known as `ToolExecutor`, so the MCP read-only names come through the trait.
fn set_section_mode_policy<C: ApiClient, T: ToolExecutor>(
    runtime: &mut ConversationRuntime<C, T>,
    mode: &str,
) {
    let mcp_read_only = runtime.executor().mcp_read_only_names();
    runtime.set_permission_policy(crate::permissions::permission_policy_for_mode(
        mode,
        &mcp_read_only,
    ));
}

/// C项 TUI autopilot cap: auto-continue nudges per mission, matching the
/// blocking REPL's `MAX_AUTOPILOT_TURNS`.
const MAX_TUI_AUTOPILOT_NUDGES: usize = 64;

#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_loop<C, T, F, D, S>(
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
    // C项 TUI autopilot state: per-mission auto-continue counts (cap below),
    // so an unsatisfiable mission cannot spin the queue forever.
    let mut loop_nudges: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    // S2 (roadmap §16) for the TUI autopilot: a transport-class failure in a
    // loop-mode section schedules the same nudge again after a backoff
    // instead of silently stopping the loop. One pending backoff at a time;
    // per-mission attempt counts reset on any non-transport settle, exactly
    // like the REPL's `run_loop_autopilot`.
    let mut loop_backoff: Option<(usize, u64, tokio::time::Instant)> = None;
    let mut loop_transport_retries: std::collections::HashMap<u64, usize> =
        std::collections::HashMap::new();

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
                // section, rebuilt fresh at every turn boundary. The ledger
                // also arms the D1.2 mid-turn token guard with the single
                // active mission's remaining budget.
                let missions_root = crate::storage::sessions_dir().join("missions");
                let missions = MissionLedger::load(&missions_root).list();
                let mission_section = turn_reminder(&missions);
                runtime.set_turn_token_budget(turn_token_budget(&missions));
                let mode_section = crate::mode_prompt_segment(
                    &crate::storage::lock_state(&states[shell.active_index()]).mode,
                );
                let merged = match (mission_section, mode_section) {
                    (Some(mission), Some(segment)) => Some(format!(
                        "{mission}

{segment}"
                    )),
                    (Some(mission), None) => Some(mission),
                    (None, Some(segment)) => Some(segment.to_string()),
                    (None, None) => None,
                };
                runtime.set_turn_context(merged);
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
        // S2 timer: when a loop-mode transport backoff is pending, the select
        // wakes at its deadline to re-queue the nudge (see the settle arm).
        let backoff_due = loop_backoff.as_ref().map(|(_, _, due)| *due);
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
                runtime.set_turn_token_budget(None);
                prompter = Some(finished_prompter);
                let mut mission_attention: Option<String> = None;
                let (ok, err_note, turn_error_kind) = match turn_outcome {
                    // The summary's contents (iteration count, tool results)
                    // are already folded into the session and the transcript;
                    // the TUI settle only needs success vs. failure.
                    Ok(summary) => {
                        // Auto-account against the single active mission
                        // (§11.3): success only — a failed turn is not
                        // productive work and must not reset a blocked
                        // streak. `turn_usage` is this turn's own spend; the
                        // ledger debits per turn, never the session
                        // cumulative.
                        let tokens = u64::from(summary.turn_usage.input_tokens)
                            .saturating_add(u64::from(summary.turn_usage.output_tokens));
                        mission_attention =
                            auto_record_turn(&crate::storage::sessions_dir().join("missions"), tokens);
                        (true, None, None)
                    }
                    Err(error) => (false, Some(error.to_string()), Some(error.kind())),
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
                    // run_turn_interactive. The structured class rides along
                    // so downstream consumers classify without re-parsing.
                    let text = err_note
                        .clone()
                        .unwrap_or_else(|| String::from("turn failed"));
                    app.apply_event(&AgentEvent::Error {
                        message: text,
                        hint: None,
                        kind: turn_error_kind,
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
                // C项 TUI autopilot, with REPL parity on failures (§16 S2): a
                // loop-mode section keeps driving its single active mission.
                // A transport-class failure backs off (30s → 2m → 8m) and
                // re-queues the same nudge via the timer arm below; a real
                // failure records a blocked claim like the REPL's autopilot,
                // and three consecutive claims flip the mission to Blocked
                // and stop. The nudge goes into the *section's* queue, so
                // the §11.4 rotation runs it where the mission lives, not
                // wherever the operator happens to be looking.
                if crate::storage::lock_state(&states[section]).mode == "loop" {
                    let ledger = MissionLedger::load(
                        &crate::storage::sessions_dir().join("missions"),
                    );
                    if let Some(mission_id) = single_active_mission_id(&ledger) {
                        if ok {
                            loop_transport_retries.remove(&mission_id);
                            loop_backoff = None;
                            match ledger.complete(mission_id) {
                                Ok(_) => {
                                    app.push_line(muted_line(format!(
                                        "· loop: mission m{mission_id} complete"
                                    )));
                                }
                                Err(runtime::MissionError::Unsatisfied { .. }) => {
                                    // D1.2 driving half: a spent budget keeps
                                    // the mission open but stops the driver.
                                    if budget_stop(&ledger, mission_id) {
                                        app.push_line(muted_line(format!(
                                            "· loop: mission m{mission_id} budget exhausted - stopping (raise it with /mission or finish the criteria manually)"
                                        )));
                                    } else {
                                        let nudges =
                                            loop_nudges.entry(mission_id).or_insert(0);
                                        if *nudges < MAX_TUI_AUTOPILOT_NUDGES {
                                            *nudges += 1;
                                            let queued = app
                                                .model
                                                .queue_mut()
                                                .send(
                                                    &continue_nudge(mission_id),
                                                    crate::core::QueueSource::User,
                                                )
                                                .is_some();
                                            if !queued {
                                                app.push_line(muted_line(
                                                    "· loop: queue full - auto-continue paused"
                                                        .to_string(),
                                                ));
                                            }
                                        } else {
                                            app.push_line(muted_line(format!(
                                                "· loop: mission m{mission_id} hit the auto-continue cap ({MAX_TUI_AUTOPILOT_NUDGES})"
                                            )));
                                        }
                                    }
                                }
                                // WeakOnly / NotActive / NotFound: stop honestly.
                                Err(_) => {}
                            }
                        } else if turn_error_kind == Some(runtime::ErrorKind::Transport)
                            && loop_transport_retries
                                .get(&mission_id)
                                .copied()
                                .unwrap_or(0)
                                < MAX_TRANSPORT_RETRIES
                        {
                            let attempt = loop_transport_retries.entry(mission_id).or_insert(0);
                            let delay = transport_backoff(*attempt);
                            *attempt += 1;
                            loop_backoff =
                                Some((section, mission_id, tokio::time::Instant::now() + delay));
                            app.push_line(muted_line(format!(
                                "· loop: transport failure ({}); retrying in {}s (transport retry {}/{})",
                                runtime::ErrorKind::Transport.label(),
                                delay.as_secs(),
                                *attempt,
                                MAX_TRANSPORT_RETRIES
                            )));
                        } else {
                            // A real failure, or transport retries exhausted:
                            // one blocked claim, REPL parity. Claims below the
                            // streak keep driving; the third flips the mission
                            // to Blocked and stops.
                            loop_transport_retries.remove(&mission_id);
                            loop_backoff = None;
                            if let Ok((mission, flipped)) =
                                ledger.mark_blocked(mission_id, "auto-continue turn failed")
                            {
                                if flipped {
                                    app.push_line(muted_line(format!(
                                        "· loop: mission m{mission_id} blocked after {} consecutive failed turns - stopping.",
                                        mission.blocked_streak
                                    )));
                                } else {
                                    app.push_line(muted_line(format!(
                                        "· loop: blocked claim {}/{} recorded; the rotation keeps driving.",
                                        mission.blocked_streak,
                                        runtime::BLOCKED_STREAK_NEEDED
                                    )));
                                    // Keep driving: re-nudge once so the next
                                    // turn happens without operator input.
                                    let nudges = loop_nudges.entry(mission_id).or_insert(0);
                                    if *nudges < MAX_TUI_AUTOPILOT_NUDGES {
                                        *nudges += 1;
                                        let _ = app.model.queue_mut().send(
                                            &continue_nudge(mission_id),
                                            crate::core::QueueSource::User,
                                        );
                                    }
                                }
                            }
                        }
                    }
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
                // forever. S2 exception: when the autopilot scheduled a
                // transport backoff for this failure, the rotation must not
                // also hot-requeue the entry (the timer re-nudges after the
                // delay) — the entry files as `Failed`; the backoff's retry
                // arrives as a fresh entry.
                if let Some(entry) = turn_entry.take() {
                    let s2_backoff_scheduled = loop_backoff.is_some();
                    let retried = last_failed_entry == Some(entry.id);
                    let state = if ok {
                        last_failed_entry = None;
                        crate::core::QueueEntryState::Delivered
                    } else if s2_backoff_scheduled || retried {
                        // Both file as `Failed` without a retry: an S2
                        // backoff re-nudges as a fresh entry after the delay,
                        // and a twice-failed entry is a poisoned one.
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
                    // ===== C-1 keybind drains =====
                    // Web 三态: the shared atomic flips live — the model sees
                    // the new advertisement on its very next request.
                    if shell.section_mut().take_web_cycle() {
                        let state = &states[shell.active_index()];
                        let next =
                            match crate::tool_exec::WebAccess::from_u8(crate::storage::lock_state(state).web_access.load(std::sync::atomic::Ordering::Relaxed)) {
                                crate::tool_exec::WebAccess::On => crate::tool_exec::WebAccess::Off,
                                crate::tool_exec::WebAccess::Off => crate::tool_exec::WebAccess::Local,
                                crate::tool_exec::WebAccess::Local => crate::tool_exec::WebAccess::On,
                            };
                        if let Some(runtime) = runtimes[shell.active_index()].as_ref() {
                            runtime.executor().set_web_access(next.as_u8());
                        }
                        crate::storage::lock_state(state).web_access.store(next.as_u8(), std::sync::atomic::Ordering::Relaxed);
                        shell.section_mut().status_note =
                            format!("web access -> {}", next.label());
                    }
                    // Mode cycle: idle-only per section — a live turn owns the
                    // runtime the new policy must land on.
                    if shell.section_mut().take_mode_cycle() {
                        let slot = shell.active_index();
                        if let Some(runtime) = runtimes[slot].as_mut() {
                            let state = &states[slot];
                            let current = crate::storage::lock_state(state).mode.clone();
                            let next = match current.as_str() {
                                "plan" => "spec",
                                "spec" => "loop",
                                "loop" | "read-only" | "full" => "workspace-write",
                                _ => "plan",
                            };
                            set_section_mode_policy(runtime, next);
                            crate::storage::lock_state(state).mode = next.to_string();
                            shell.section_mut().status_note = format!("mode -> {next}");
                        } else {
                            shell.section_mut().status_note = String::from(
                                "finish or cancel the turn before switching modes",
                            );
                        }
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
            _ = async {
                match backoff_due {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                // The backoff elapsed — re-validate and re-queue the nudge
                // into the section's own queue. Mode may have changed during
                // the wait, and the mission may have advanced (completed,
                // blocked, paused) from another path: stop honestly on both.
                if let Some((bsection, mission_id, _)) = loop_backoff.take() {
                    if bsection >= shell.sections.len()
                        || crate::storage::lock_state(&states[bsection]).mode != "loop"
                    {
                        if bsection < shell.sections.len() {
                            shell.section_index_mut(bsection).push_line(muted_line(
                                "· loop: backoff dropped (section left loop mode)".to_string(),
                            ));
                        }
                    } else {
                        let ledger = MissionLedger::load(
                            &crate::storage::sessions_dir().join("missions"),
                        );
                        match ledger.complete(mission_id) {
                            Ok(_) => {
                                shell.section_index_mut(bsection).push_line(muted_line(
                                    format!("· loop: mission m{mission_id} complete"),
                                ));
                            }
                            Err(runtime::MissionError::Unsatisfied { .. }) => {
                                // D1.2 driving half: a spent budget stops the
                                // driver here too (the settle arm checks it
                                // as well; this covers a budget that flipped
                                // while the backoff ticked).
                                if budget_stop(&ledger, mission_id) {
                                    shell.section_index_mut(bsection).push_line(muted_line(
                                        format!(
                                            "· loop: mission m{mission_id} budget exhausted - stopping (raise it with /mission or finish the criteria manually)"
                                        ),
                                    ));
                                } else {
                                    let queued = {
                                        let app = shell.section_index_mut(bsection);
                                        app.model
                                            .queue_mut()
                                            .send(
                                                &continue_nudge(mission_id),
                                                crate::core::QueueSource::User,
                                            )
                                            .is_some()
                                    };
                                    if !queued {
                                        shell.section_index_mut(bsection).push_line(muted_line(
                                            "· loop: queue full - auto-continue paused".to_string(),
                                        ));
                                    }
                                }
                            }
                            // WeakOnly / NotActive / NotFound: stop honestly.
                            Err(_) => {
                                shell.section_index_mut(bsection).push_line(muted_line(
                                    format!(
                                        "· loop: mission m{mission_id} is no longer driveable - stopping."
                                    ),
                                ));
                            }
                        }
                    }
                }
                dirty = true;
            }
        }
    }
    Ok(())
}

/// KI-003 paste-burst window (see `pump_events`): key events within this gap
/// of the previous key belong to one input burst; real pastes land
/// sub-millisecond apart, human typing never under ~30ms.
const PASTE_BURST_WINDOW: std::time::Duration = std::time::Duration::from_millis(10);
/// An Enter is rewritten to a newline only once the burst has this many
/// events — two fast taps are plausible typing, three-plus inside 10ms is a
/// paste stream.
const PASTE_BURST_MIN: usize = 3;

/// Blocking crossterm event pump. Translates terminal events into [`Msg`]s and
/// forwards them; exits on shutdown, send failure, or read error. Runs on a
/// blocking thread because crossterm's `poll`/`read` are synchronous. `budget`
/// is the frame-merge window: idle loops wake on the poll timeout but skip the
/// draw (see the `dirty` flag), and bursts of input coalesce into one repaint,
/// so a quiet session costs nothing.
fn pump_events(tx: mpsc::UnboundedSender<Msg>, shutdown: &AtomicBool, budget: Duration) {
    // KI-003 paste-burst heuristic (same shape as the REPL editor): without
    // bracketed-paste support a multi-line paste arrives as a sub-10ms burst
    // of key events whose newlines are bare Enters — the first one would
    // submit the half-pasted draft. Rewrite a mid-burst Enter as
    // Shift+Enter, which the reducer treats as a newline.
    let mut last_key_at: Option<std::time::Instant> = None;
    let mut burst_len = 0usize;
    while !shutdown.load(Ordering::Relaxed) {
        match event::poll(budget) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    // Ignore key-release repeats some terminals emit; only act
                    // on press to avoid double-processing held keys.
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    let now = std::time::Instant::now();
                    let in_burst =
                        last_key_at.is_some_and(|at| now.duration_since(at) < PASTE_BURST_WINDOW);
                    burst_len = if in_burst {
                        burst_len.saturating_add(1)
                    } else {
                        1
                    };
                    last_key_at = Some(now);
                    let mut key = key;
                    if key.code == KeyCode::Enter
                        && burst_len >= PASTE_BURST_MIN
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        key.modifiers |= KeyModifiers::SHIFT;
                    }
                    if tx.send(Msg::Key(key)).is_err() {
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
