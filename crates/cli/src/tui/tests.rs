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
        .any(|entry| entry.text == "bb" && entry.state == crate::core::QueueEntryState::Withdrawn));
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

/// Reducer fuzz (roadmap §4 抗冲击战役②): a fixed-seed xorshift throws
/// 2000 random messages — keys from every binding family, stream events
/// from every variant, oversized pastes with control bytes, overlay and
/// panel chords, mid-turn traffic — at a single [`App`]. Invariants: the
/// reducer never panics, the transcript only grows, and the queue never
/// exceeds its cap. The seed is fixed so any panic reproduces exactly.
#[test]
fn reducer_fuzz_survives_random_message_streams() {
    let mut rng: u64 = 0xDEAD_BEEF_CAFE_F00D;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut app = App::new();
    let mut transcript_floor = 0_usize;
    let keys = [
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
    ];
    let events = [
        AgentEvent::TextDelta("delta text ".into()),
        AgentEvent::ThinkingDelta("thinking ".into()),
        AgentEvent::ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: "{}".into(),
        },
        AgentEvent::ToolResult {
            id: "1".into(),
            name: "bash".into(),
            output: "ok".into(),
            is_error: false,
        },
        AgentEvent::ToolResult {
            id: "9".into(),
            name: "ghost".into(),
            output: String::new(),
            is_error: true,
        },
        AgentEvent::Usage(TokenUsage::default()),
        AgentEvent::MessageStop,
        AgentEvent::Truncated("max tokens".into()),
        AgentEvent::Steered("steered text".into()),
        AgentEvent::Error {
            message: "boom".into(),
            hint: Some("try again".into()),
        },
    ];
    for _step in 0..2000 {
        let msg = match next() % 6 {
            0 => Msg::Key(keys[(next() as usize) % keys.len()]),
            1 => Msg::Event {
                section: (next() % 2) as usize,
                event: events[(next() as usize) % events.len()].clone(),
            },
            2 => Msg::Paste("paste\nwith\nnewlines \u{7}\u{0}".repeat(1 + (next() % 40) as usize)),
            3 => {
                if next() % 2 == 0 {
                    Msg::TurnStarted { section: 0 }
                } else {
                    Msg::Resize
                }
            }
            4 => Msg::TurnFinished {
                section: 0,
                ok: next() % 2 == 0,
                note: "note".into(),
            },
            _ => Msg::PermissionRequest(PermissionRequest {
                tool_name: "bash".into(),
                input: "{}".into(),
                reason: None,
            }),
        };
        app.update(msg);
        assert!(
            app.transcript.len() >= transcript_floor,
            "transcript shrank past floor {transcript_floor}"
        );
        transcript_floor = transcript_floor.max(app.transcript.len());
        // Drain the loop-facing channels so staging does not accumulate;
        // draining is the loop's job, the reducer only parks state.
        let _ = app.take_submit();
        let _ = app.take_steer();
        let _ = app.take_cancel();
        let _ = app.take_permission_choice();
        assert!(
            app.model.queue().len() <= crate::core::DEFAULT_QUEUE_LIMIT,
            "queue exceeded its cap"
        );
    }
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
