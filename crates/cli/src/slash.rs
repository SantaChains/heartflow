//! Slash-command surface (cli-thinning): the `/` dispatch table and every
//! handler family it owns, extracted verbatim from `main.rs` so the REPL-loop
//! file stays navigable. `main.rs` re-exports this module via
//! `pub(crate) use slash::*;`, so original call sites (`run_repl`'s
//! `dispatch_slash_command`/`LoopControl`/`handle_bang_command` and the
//! `super::` test paths) keep resolving unchanged.
//! Items that stay in `main.rs` (REPL loop, `WorkerRunner`, compaction, the
//! completion context) are reached through the root glob below.

#[allow(clippy::wildcard_imports)]
use crate::*;

/// A slash token that matched no known command: suggest the closest one instead
/// of forwarding the typo to the model as a turn.
fn report_unknown_command(input: &str) {
    match editor::suggest_command(input) {
        Some(good) => println!("unknown command {input}; did you mean {good}?"),
        None => println!("unknown command {input} (type /help for the list)"),
    }
}

/// Reset the live session to a fresh runtime, dropping planning state. Extracted
/// so `/clear` stays a one-token arm and `run_repl` remains under the line cap.
fn handle_clear_command(
    state: &SessionShared,
    selection: &ProviderSelection,
    mode: &str,
    runtime: &mut AgentRuntime,
    plan: &mut ReplPlanState,
) {
    match build_runtime(state, Session::new(), selection.clone(), true, mode) {
        Ok(fresh) => {
            *runtime = fresh;
            plan.clear();
            // A cleared session is a new conversation: give it a fresh file and
            // history row instead of appending onto the one just wiped.
            rotate_session_id(state);
            println!("session cleared.");
        }
        Err(error) => println!("failed to reset session: {error}"),
    }
}

/// What the REPL loop should do after one input line is fully handled.
pub(crate) enum LoopControl {
    Continue,
    Exit,
    /// `/restart` confirmed: unwind the REPL and ask `main` to re-exec.
    Restart,
}

/// `/save` from the REPL: persist the live session and report the path or the
/// failure inline (a failed save is a status message, never an abort).
async fn save_and_report(state: &SessionShared, runtime: &AgentRuntime) {
    match save_session_async(state, runtime.session()).await {
        Ok(path) => println!("session saved -> {}", path.display()),
        Err(error) => println!("failed to save session: {error}"),
    }
}

/// Route one `/`-prefixed line. Slash handling is inherently wide: the
/// subcommands touch every piece of REPL state. The plan/spec contract flags
/// are bundled into [`ReplPlanState`]; the remaining breadth (runtime, model,
/// registries, editor) is genuine — each piece is a distinct owner the arms
/// mutate independently, which is why this is not a single opaque struct.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_slash_command(
    state: &SessionShared,
    trimmed: &str,
    runtime: &mut AgentRuntime,
    selection: &mut ProviderSelection,
    mode: &mut String,
    plan: &mut ReplPlanState,
    model: &mut HeartModel,
    prompter: &mut CliPermissionPrompter,
    cwd: &Path,
    catalog: &ProviderCatalog,
    editor: &mut editor::ReplEditor,
    jobs: &runtime::JobRegistry,
    missions: &runtime::MissionLedger,
    schedules: &runtime::ScheduleStore,
    schedule_links: &mut std::collections::HashMap<u64, (u64, u64, bool)>,
    runtime_handle: &tokio::runtime::Handle,
) -> Result<LoopControl, Box<dyn std::error::Error>> {
    // Every arm yields the loop-level control directly; the match is wrapped in
    // `Ok` once so each arm stays one statement shorter.
    Ok(match trimmed {
        "/exit" | "/quit" => {
            exit_with_resume_hint(state, runtime).await;
            LoopControl::Exit
        }
        "/help" => {
            print_repl_help();
            LoopControl::Continue
        }
        "/status" => {
            print_status(runtime, selection.model(), mode);
            LoopControl::Continue
        }
        "/save" => {
            save_and_report(state, runtime).await;
            LoopControl::Continue
        }
        "/clear" => {
            handle_clear_command(state, selection, mode, runtime, plan);
            LoopControl::Continue
        }
        _ if trimmed == "/sessions" || trimmed.starts_with("/sessions ") => {
            let filter = trimmed.strip_prefix("/sessions").map(|rest| rest.trim());
            // An empty rest means the bare command: no filter, full listing.
            print_sessions_listing(filter.filter(|rest| !rest.is_empty()));
            LoopControl::Continue
        }
        _ if trimmed == "/open" || trimmed.starts_with("/open ") => {
            handle_open_command(state, trimmed, selection, mode, runtime);
            LoopControl::Continue
        }
        _ if trimmed == "/remember" || trimmed.starts_with("/remember ") => {
            handle_remember_command(trimmed);
            LoopControl::Continue
        }
        "/compact" => {
            force_compact(state, runtime);
            LoopControl::Continue
        }
        "/pin" => {
            handle_pin_command(state, runtime);
            LoopControl::Continue
        }
        "/checkpoints" => {
            handle_checkpoints_command();
            LoopControl::Continue
        }
        _ if trimmed == "/restore" || trimmed.starts_with("/restore ") => {
            handle_restore_command(trimmed);
            LoopControl::Continue
        }
        "/focus" => {
            let focused = editor.toggle_focus();
            println!(
                "focus mode: {}",
                if focused { "on (mascot hidden)" } else { "off" }
            );
            LoopControl::Continue
        }
        "/mcp" => {
            print_mcp_status(runtime);
            LoopControl::Continue
        }
        "/restart" => {
            if confirm_restart() {
                LoopControl::Restart
            } else {
                println!("restart cancelled.");
                LoopControl::Continue
            }
        }
        _ if trimmed == "/search" || trimmed.starts_with("/search ") => {
            handle_search_command(trimmed);
            LoopControl::Continue
        }
        _ if trimmed == "/expand" || trimmed.starts_with("/expand ") => {
            handle_expand_command(state, trimmed);
            LoopControl::Continue
        }
        _ if trimmed == "/queue" || trimmed.starts_with("/queue ") => {
            handle_queue_command(trimmed, model);
            LoopControl::Continue
        }
        _ if trimmed == "/jobs" || trimmed.starts_with("/jobs ") => {
            handle_jobs_command(trimmed, jobs, state, selection, runtime_handle);
            LoopControl::Continue
        }
        _ if trimmed == "/mission" || trimmed.starts_with("/mission ") => {
            handle_mission_command(trimmed, missions, state, runtime, prompter, mode).await;
            LoopControl::Continue
        }
        _ if trimmed == "/schedule" || trimmed.starts_with("/schedule ") => {
            handle_schedule_command(
                trimmed,
                schedules,
                jobs,
                state,
                selection,
                runtime_handle,
                schedule_links,
            );
            LoopControl::Continue
        }
        _ if trimmed == "/guide" || trimmed.starts_with("/guide ") => {
            handle_guide_command(state, runtime, model, plan, prompter, editor, mode, trimmed)
                .await;
            LoopControl::Continue
        }
        "/init" => {
            match write_agents_skeleton(cwd, false) {
                Ok(message) => println!("{message}"),
                Err(error) => println!("failed to write AGENTS.md: {error}"),
            }
            LoopControl::Continue
        }
        _ if trimmed == "/mode" || trimmed.starts_with("/mode ") => {
            handle_mode_command(state, trimmed, mode, selection, runtime);
            plan.planning = false;
            plan.speccing = false;
            plan.spec_path = None;
            LoopControl::Continue
        }
        _ if trimmed == "/model" || trimmed.starts_with("/model ") => {
            handle_model_command(state, trimmed, selection, runtime, mode, catalog);
            plan.planning = false;
            plan.speccing = false;
            plan.spec_path = None;
            LoopControl::Continue
        }
        _ if trimmed == "/provider" || trimmed.starts_with("/provider ") => {
            handle_provider_command(state, trimmed, selection, runtime, mode, catalog);
            LoopControl::Continue
        }
        _ if trimmed == "/models" => {
            handle_models_command(selection, catalog).await;
            LoopControl::Continue
        }
        _ if trimmed == "/web" || trimmed.starts_with("/web ") => {
            handle_web_command(state, trimmed);
            plan.planning = false;
            plan.speccing = false;
            plan.spec_path = None;
            LoopControl::Continue
        }
        _ if trimmed == "/effort" || trimmed.starts_with("/effort ") => {
            handle_effort_command(state, trimmed, selection, runtime, mode);
            plan.planning = false;
            plan.speccing = false;
            plan.spec_path = None;
            LoopControl::Continue
        }
        _ if trimmed == "/bootstrap" || trimmed.starts_with("/bootstrap ") => {
            handle_bootstrap_command(state, trimmed, selection, runtime, mode, catalog).await;
            plan.planning = false;
            plan.speccing = false;
            plan.spec_path = None;
            LoopControl::Continue
        }
        _ if trimmed == "/plan" || trimmed.starts_with("/plan ") => {
            handle_plan_command(state, trimmed, plan, cwd, mode, runtime, prompter).await?;
            LoopControl::Continue
        }
        _ if trimmed == "/spec" || trimmed.starts_with("/spec ") => {
            handle_spec_command(state, trimmed, plan, cwd, mode, runtime, prompter).await?;
            LoopControl::Continue
        }
        _ => {
            report_unknown_command(trimmed);
            LoopControl::Continue
        }
    })
}

/// `!<cmd>` — run a shell command directly through the same bash tool path the
/// model uses (pwsh on Windows, UTF-8 wrapped), with no model round-trip.
/// Dangerous commands are confirmed first; output is folded like tool output
/// and stays reachable via `/expand`.
///
/// `!` is the operator's own escape hatch so it is never *blocked* by the
/// permission mode — but `read-only`/`plan` promise that nothing in the session
/// changes, and a silent exception makes that promise false. So the mode is
/// folded into the same confirmation, with the reason stated, instead of being
/// quietly ignored or bluntly refused.
pub(crate) async fn handle_bang_command(state: &SessionShared, mode: &str, input: &str) {
    let command = input.trim().strip_prefix('!').unwrap_or("").trim();
    if command.is_empty() {
        println!("usage: !<shell command>   e.g. !git status");
        return;
    }
    let mut reasons: Vec<String> = Vec::new();
    if matches!(mode, "read-only" | "plan") {
        reasons.push(format!(
            "this session is in `{mode}` mode, where tools cannot write"
        ));
    }
    if is_dangerous_command(command) {
        reasons.push("the command looks destructive".to_string());
    }
    if !reasons.is_empty()
        && !Confirm::new(&format!(
            "Run `{command}`?\n  because: {}",
            reasons.join("; ")
        ))
        .with_default(false)
        .prompt()
        .unwrap_or(false)
    {
        println!("cancelled.");
        return;
    }
    let bash_input = BashCommandInput {
        command: command.to_string(),
        timeout: None,
        description: None,
        run_in_background: Some(false),
        dangerously_disable_sandbox: None,
    };
    // `execute_bash` drives its own current-thread runtime; run it off the
    // async reactor so the nested `block_on` never panics.
    let joined = tokio::task::spawn_blocking(move || execute_bash(bash_input)).await;
    let output = match joined {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            println!(
                "{}",
                format!("shell error: {error}").with(theme::Theme::current().error().crossterm())
            );
            return;
        }
        Err(task) => {
            println!(
                "{}",
                format!("shell task failed: {task}")
                    .with(theme::Theme::current().error().crossterm())
            );
            return;
        }
    };
    let mut body = output.stdout;
    if !output.stderr.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&output.stderr);
    }
    if body.trim().is_empty() {
        println!(
            "{}",
            "(no output)".with(theme::Theme::current().muted().crossterm())
        );
    } else {
        let markdown = fold_tool_output(state, "shell", &body, FOLD_TOOL_OUTPUT_LINES);
        println!("{}", TerminalRenderer::new().render_markdown(&markdown));
    }
    if output.interrupted {
        println!(
            "{}",
            "! interrupted (timeout)".with(theme::Theme::current().strong().crossterm())
        );
    } else if let Some(interpretation) = &output.return_code_interpretation {
        println!(
            "{}",
            format!("! {interpretation}").with(theme::Theme::current().muted().crossterm())
        );
    }
}

/// Manual `/compact`: force a compaction immediately, bypassing the auto
/// gates while honoring every knob of the active policy (preserve window,
/// replay tail, tail budget, summary mode). Reports when the session is
/// still too short to have anything to compact.
fn force_compact(state: &SessionShared, runtime: &mut AgentRuntime) {
    let result = runtime.compact_now(runtime.compaction());
    if result.removed_message_count == 0 {
        println!("nothing to compact: session is within the preserve window.");
    } else {
        note_mirror_rewrite(state);
        println!(
            "Compacted {} messages into a resumable summary.",
            result.removed_message_count
        );
    }
}

/// List the shadow-git snapshots recorded for this project (§6 checkpoints).
/// The listing is what `/restore` indexes into; newest snapshot is 1.
fn handle_checkpoints_command() {
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            println!("cannot resolve the project directory: {error}");
            return;
        }
    };
    match runtime::list_snapshots(&cwd) {
        Ok(snapshots) if snapshots.is_empty() => {
            println!("no checkpoints yet - they are recorded before each write_file/edit_file/apply_patch.");
        }
        Ok(snapshots) => {
            println!("checkpoints (newest first; restore with /restore <number>):");
            for (index, snapshot) in snapshots.iter().enumerate() {
                println!(
                    "  {:>2}. {}  {}  {}",
                    index + 1,
                    &snapshot.sha[..8.min(snapshot.sha.len())],
                    snapshot.time,
                    snapshot.subject
                );
            }
        }
        Err(error) => println!("cannot list checkpoints: {error}"),
    }
}

/// `/restore <number|sha>`: bring the project's tracked files back to a
/// snapshot recorded before a write-class tool ran. Files created *after*
/// the snapshot are deliberately kept (the MVP refuses to run a git clean
/// over the user's project); the output says so explicitly.
fn handle_restore_command(input: &str) {
    let arg = input.trim().strip_prefix("/restore").unwrap_or("").trim();
    if arg.is_empty() {
        println!("usage: /restore <number|sha> [--purge] - list candidates with /checkpoints");
        return;
    }
    // `--purge` is the explicit opt-in to also remove files created after the
    // target snapshot; without it, restore keeps them in place.
    let mut tokens = arg.split_whitespace();
    let revision_arg = tokens.next().unwrap_or("");
    let purge = tokens.next() == Some("--purge");
    if tokens.next().is_some() {
        println!("usage: /restore <number|sha> [--purge]");
        return;
    }
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            println!("cannot resolve the project directory: {error}");
            return;
        }
    };
    // Resolve a numeric index against the listing so the two commands share
    // one ordering; anything non-numeric is treated as a revision directly.
    let revision = match revision_arg.parse::<usize>() {
        Ok(index) => match runtime::list_snapshots(&cwd) {
            Ok(snapshots) if index >= 1 && index <= snapshots.len() => {
                snapshots[index - 1].sha.clone()
            }
            Ok(snapshots) => {
                println!("no checkpoint {index} (1..={} exist)", snapshots.len());
                return;
            }
            Err(error) => {
                println!("cannot list checkpoints: {error}");
                return;
            }
        },
        Err(_) => revision_arg.to_string(),
    };
    match runtime::restore_snapshot(&cwd, &revision) {
        Ok(file_count) => {
            println!("restored {file_count} tracked file(s) to snapshot {revision}.");
        }
        Err(error) => {
            println!("restore failed: {error}");
            return;
        }
    }
    if !purge {
        println!("note: files created after that snapshot are kept in place - rerun with `--purge` to remove them (a backup snapshot is taken first).");
        return;
    }
    match runtime::purge_new_files_since(&cwd, &revision) {
        Ok(report) => {
            println!(
                "purge backup snapshot: {} (rerun /restore {} to undo the purge)",
                &report.backup_sha[..report.backup_sha.len().min(12)],
                &report.backup_sha[..report.backup_sha.len().min(12)]
            );
            println!(
                "removed {} file(s) created after the snapshot.",
                report.removed.len()
            );
            for path in &report.removed {
                println!("  - {path}");
            }
            for failure in &report.failed {
                println!("  ! {failure}");
            }
        }
        Err(error) => println!("purge failed (restore already applied): {error}"),
    }
}

/// Manual `/pin`: toggle the never-compacted flag on the most recent
/// assistant/user turn. A pinned message survives every compaction verbatim
/// instead of being folded into the summary, so key constraints or decisions
/// stay in context. Reports the new state and how many messages are pinned.
fn handle_pin_command(state: &SessionShared, runtime: &mut AgentRuntime) {
    match runtime.toggle_pin_latest() {
        Some(pinned) => {
            // Pinning flips a flag on an already-mirrored row without changing
            // the transcript length, so the mirror must be fully rewritten.
            note_mirror_rewrite(state);
            if pinned {
                println!(
                    "pinned the last message - it will survive compaction ({} pinned total).",
                    runtime.pinned_count()
                );
            } else {
                println!(
                    "unpinned the last message ({} pinned total).",
                    runtime.pinned_count()
                );
            }
        }
        None => println!("nothing to pin yet - send a message first."),
    }
}

/// Inspect or switch the permission mode mid-session while keeping the current
/// conversation. `read-only` reads and plans without writing (plan mode),
/// `workspace-write` (default) asks before `bash`, `full` auto-approves all.
fn handle_mode_command(
    state: &SessionShared,
    input: &str,
    mode: &mut String,
    selection: &ProviderSelection,
    runtime: &mut AgentRuntime,
) {
    let requested = input
        .strip_prefix("/mode")
        .map(str::trim)
        .unwrap_or_default();
    if requested.is_empty() {
        println!("mode: {mode}");
        println!("known modes: {}", KNOWN_PERMISSION_MODES.join(", "));
        println!("switch with: /mode NAME");
        return;
    }
    let normalized = if requested == "auto" {
        "full"
    } else {
        requested
    };
    if !KNOWN_PERMISSION_MODES.contains(&normalized) {
        println!(
            "unknown mode '{requested}'; choose one of: {}",
            KNOWN_PERMISSION_MODES.join(", ")
        );
        return;
    }
    match build_runtime(
        state,
        runtime.session().clone(),
        selection.clone(),
        true,
        normalized,
    ) {
        Ok(rebuilt) => {
            *runtime = rebuilt;
            *mode = normalized.to_string();
            lock_state(state).mode = normalized.to_string();
            println!("mode -> {normalized}");
        }
        Err(error) => println!("failed to switch mode: {error}"),
    }
}

/// Print the saved-session list for the `/sessions` command. Each row leads
/// with the session's topic (its first user message — a bare `<id>.json` path
/// is unreadable as a list entry) with the path after it. With a filter, only
/// sessions whose topic contains it (case-insensitive) are shown, up to a
/// larger cap since the point is narrowing a long list down.
fn print_sessions_listing(filter: Option<&str>) {
    let sessions = list_sessions();
    if sessions.is_empty() {
        println!("no saved sessions found");
        return;
    }
    let needle = filter.map(str::to_lowercase);
    println!("saved sessions (newest first):");
    let cap = if needle.is_some() { 20 } else { 9 };
    let mut shown = 0;
    for (index, path) in sessions.iter().enumerate() {
        let topic = session_topic(path);
        if let Some(needle) = &needle {
            let haystack = topic.as_deref().unwrap_or("");
            if !haystack.to_lowercase().contains(needle) {
                continue;
            }
        }
        shown += 1;
        if shown > cap {
            break;
        }
        let topic = topic.as_deref().unwrap_or("(untitled)");
        println!("  {}. {}  ({})", index + 1, topic, path.display());
    }
    if shown == 0 {
        println!(
            "no session matches `{}` (searched each session's opening message)",
            filter.unwrap_or_default()
        );
        return;
    }
    println!("resume with: heartflow --resume; open in-place: /open <N> (or /open <TOPIC>)");
}

/// REPL command help table: (left column, description, optional continuation
/// line). Printed by `print_repl_help` and kept in sync with
/// `editor::COMMANDS` by the `repl_help_and_completion_commands_stay_in_sync`
/// test below; a command only enters one table when it enters both. The
/// `!<CMD>` row is deliberately outside the sync (it is not a slash command).
pub(crate) const REPL_HELP: &[(&str, &str, Option<&str>)] = &[
    ("/help", "Show help", None),
    ("/status", "Show session status", None),
    ("/model [NAME]", "Show or switch the model", None),
    (
        "/provider [NAME]",
        "Show or switch the provider (+ optional model), persisted",
        None,
    ),
    (
        "/models",
        "Refresh the model list from the live endpoint",
        None,
    ),
    (
        "/bootstrap <P>",
        "Validated provider swap: pre-flight, switch, memory carries",
        Some("over (--reorganize lets the new model rewrite MEMORY.md)"),
    ),
    ("/mode [NAME]", "Show or switch the permission mode", None),
    (
        "/web <MODE>",
        "Web tool access: on / off / local (knowledge-base retrieval only)",
        None,
    ),
    (
        "/effort <LEVEL>",
        "Reasoning depth: none / low / medium / high / max",
        None,
    ),
    (
        "/plan <GOAL>",
        "Plan first (writes gated to .heartflow/plans), then /plan approve to execute",
        None,
    ),
    (
        "/spec <GOAL>",
        "Design first (writes gated to .heartflow/specs), then review execute/cancel/revise",
        None,
    ),
    ("/compact", "Compact session history", None),
    (
        "/pin",
        "Toggle never-compacted on the last message (survives /compact)",
        None,
    ),
    (
        "/checkpoints",
        "List shadow-git snapshots taken before write-class tools",
        None,
    ),
    (
        "/restore <N>",
        "Restore tracked files to checkpoint N (see /checkpoints)",
        None,
    ),
    (
        "/focus",
        "Toggle focus mode (hide mascot, minimal chrome)",
        None,
    ),
    ("/save", "Persist the session now", None),
    ("/clear", "Start a fresh session", None),
    (
        "/sessions [F]",
        "List saved sessions (F filters by opening message)",
        None,
    ),
    (
        "/open <N|TOP>",
        "Jump to saved session N, or the one whose opening message matches TOP",
        None,
    ),
    (
        "/remember <T>",
        "Persist a durable pitfall/preference to MEMORY.md",
        None,
    ),
    (
        "/search <Q>",
        "Full-text search saved conversation history",
        None,
    ),
    ("/mcp", "List connected MCP servers and tools", None),
    (
        "/expand [ID]",
        "Re-show a folded tool output (default: latest)",
        None,
    ),
    (
        "/queue [SUB]",
        "Queue control: send/lane/edit/remove/move/relane/merge/pop/clear",
        None,
    ),
    (
        "/guide <TASK>",
        "Assemble a prior-work/current-state/task draft and run it now",
        Some("(/guide draft / /guide queue keep it editable or queued)"),
    ),
    (
        "/jobs [SUB]",
        "Subagent workers: spawn [--write]/stop/log - results arrive as",
        Some("the next supervisor turn (workers run read-only without --write)"),
    ),
    (
        "/schedule [SUB]",
        "Timed prompts: add/run-due/show/pause/resume/delete by id",
        None,
    ),
    (
        "/mission [SUB]",
        "Mission ledger: create/criterion/approve/complete/pause/resume/blocked/turn",
        None,
    ),
    (
        "!<CMD>",
        "Run a shell command directly (no model); output folds like a tool result",
        None,
    ),
    (
        "/init",
        "Scaffold a starting AGENTS.md in the current directory",
        None,
    ),
    (
        "/restart",
        "Re-launch the program with a fresh config/MCP load",
        None,
    ),
    ("/exit", "Quit the REPL", None),
];

fn print_repl_help() {
    println!("Available commands:");
    for (left, desc, continuation) in REPL_HELP {
        println!("  {left:<15}{desc}");
        if let Some(extra) = continuation {
            println!("  {:<15}{extra}", "");
        }
    }
    println!();
    println!("Input: Enter sends, Alt/Shift+Enter inserts a newline, Up/Down walks history,");
    println!("  Tab completes a slash command, Ctrl+C clears the idle line, Ctrl+D quits.");
    println!("Ctrl+C while a turn is running interrupts that turn; on an empty prompt");
    println!("it just clears the line. Quit with /exit or Ctrl+D.");
}

/// Handle `/expand [ID]`: reprint a folded tool output (default: the latest).
fn handle_expand_command(state: &SessionShared, input: &str) {
    let Ok(log) = state.lock() else {
        println!("nothing to expand yet.");
        return;
    };
    if log.expandable.is_empty() {
        println!("nothing to expand yet.");
        return;
    }
    let requested = input
        .strip_prefix("/expand")
        .map(str::trim)
        .filter(|arg| !arg.is_empty());
    let index = match requested {
        Some(arg) => match arg.parse::<usize>() {
            Ok(n) if (1..=log.expandable.len()).contains(&n) => n - 1,
            _ => {
                println!(
                    "no folded output #{arg} (valid: 1..{}).",
                    log.expandable.len()
                );
                return;
            }
        },
        None => log.expandable.len() - 1,
    };
    println!("{}", log.expandable[index]);
}

/// Search saved conversation history (the `SQLite` store) for a text query.
fn handle_search_command(input: &str) {
    let query = input
        .strip_prefix("/search")
        .map(str::trim)
        .unwrap_or_default();
    if query.is_empty() {
        println!("usage: /search <query>   (search saved conversation history)");
        return;
    }
    let path = store_path();
    if !path.exists() {
        println!("history search unavailable: no sessions saved yet (run /save or finish a turn).");
        return;
    }
    match Store::open_read_only(&path).and_then(|store| store.search(query, None, 20)) {
        Ok(hits) => {
            if hits.is_empty() {
                println!("no matches for {query:?}.");
                return;
            }
            for line in format_search_hits(&hits) {
                println!("{line}");
            }
        }
        Err(error) => println!("history search failed: {error}"),
    }
}

/// Render search hits as one tidy, terminal-safe line each (snippets are
/// trimmed by characters, so Chinese stays intact and never splits mid-codepoint).
pub(crate) fn format_search_hits(hits: &[SearchHit]) -> Vec<String> {
    hits.iter()
        .map(|hit| {
            let snippet: String = hit.snippet.chars().take(120).collect();
            format!(
                "[{} #{}] {}: {}",
                hit.session_id,
                hit.seq,
                role_str(hit.role),
                snippet
            )
        })
        .collect()
}

fn print_status(runtime: &AgentRuntime, model: &str, mode: &str) {
    let usage = runtime.usage().cumulative_usage();
    println!(
        "status: model={} mode={} messages={} turns={} in={} out={} cache_read={} cache_write={}",
        model,
        mode,
        runtime.session().messages.len(),
        runtime.usage().turns(),
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_input_tokens,
        usage.cache_creation_input_tokens,
    );
}

/// Summarize MCP servers and their tools from the live tool advertisements.
fn print_mcp_status(runtime: &AgentRuntime) {
    let mcp_specs: Vec<ToolSpec> = runtime
        .tool_specs()
        .into_iter()
        .filter(|spec| spec.name.starts_with("mcp__"))
        .collect();
    if mcp_specs.is_empty() {
        println!("mcp servers: none (configure [mcp.servers.NAME] in ~/.heartflow/config.toml)");
        return;
    }
    let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for spec in &mcp_specs {
        let route = spec.name.strip_prefix("mcp__").unwrap_or_default();
        if let Some((server, tool)) = route.split_once("__") {
            grouped.entry(server).or_default().push(tool);
        }
    }
    println!("mcp servers: {}", grouped.len());
    for (server, tools) in grouped {
        println!("  {server}: {} tools ({})", tools.len(), tools.join(", "));
    }
}

/// `/queue` — inspect and edit the pending follow-up messages.
fn handle_queue_command(input: &str, model: &mut HeartModel) {
    let arg = input.trim().strip_prefix("/queue").unwrap_or("").trim();
    let mut tokens = arg.split_whitespace();
    let sub = tokens.next().unwrap_or("");
    let rest = arg.strip_prefix(sub).unwrap_or("").trim();
    match sub {
        "" => {
            if model.queue().is_empty() {
                println!("queue is empty (`/queue send <text>` queues a message for the next turn)");
                return;
            }
            let lanes = model.queue().lane_names();
            println!("{} queued (oldest first):", model.queue().len());
            for entry in model.queue().items() {
                let lane = if entry.lane == core::MAIN_LANE {
                    String::new()
                } else {
                    format!("@{} ", entry.lane)
                };
                let mode = if entry.mode == core::DeliveryMode::Steer {
                    format!("·{} ", entry.mode.tag())
                } else {
                    String::new()
                };
                println!(
                    "  #{} [{} {lane}{mode}] {}",
                    entry.id,
                    entry.source.label(),
                    truncate_chars(&entry.text, 120)
                );
            }
            if lanes.len() > 1 {
                println!("lanes: {} (see `/queue lane <name>`)", lanes.join(", "));
            }
            let recent = model.queue().recent();
            if !recent.is_empty() {
                println!("recent ({} shown of {}):", recent.len().min(5), recent.len());
                for entry in recent.iter().take(5) {
                    println!(
                        "  #{} [{}] {}",
                        entry.id,
                        entry.state.label(),
                        truncate_chars(&entry.text, 80)
                    );
                }
            }
        }
        "send" => {
            // `/queue send [--lane <name>] <text>` — a named lane keeps the
            // entry visible/editable as a group (D2.1); delivery order is
            // still global arrival order.
            let (lane, text) = if let Some(rest) = rest.strip_prefix("--lane") {
                let rest = rest.trim();
                match rest.split_once(char::is_whitespace) {
                    Some((lane, text)) => (lane, text.trim()),
                    None => {
                        println!("usage: /queue send [--lane <name>] <text>");
                        return;
                    }
                }
            } else {
                (core::MAIN_LANE, rest)
            };
            if text.is_empty() {
                println!("usage: /queue send [--lane <name>] <text>");
                return;
            }
            match model.queue_mut().send_in_lane(lane, text, core::QueueSource::User) {
                Some(id) => println!("queued #{id} (lane {lane}); it runs as the next turn."),
                None => println!("queue is full or text is blank; nothing queued."),
            }
        }
        "lane" => {
            if rest.is_empty() {
                println!("usage: /queue lane <name>   (lanes: {})", model.queue().lane_names().join(", "));
                return;
            }
            let entries = model.queue().items_in_lane(rest);
            if entries.is_empty() {
                println!("lane {rest} has no pending entries.");
                return;
            }
            println!("lane {rest} ({} pending):", entries.len());
            for entry in entries {
                println!(
                    "  #{} [{}] {}",
                    entry.id,
                    entry.source.label(),
                    truncate_chars(&entry.text, 120)
                );
            }
        }
        "edit" => {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let (id_part, text) = (parts.next().unwrap_or(""), parts.next().unwrap_or("").trim());
            let Ok(id) = id_part.parse::<u64>() else {
                println!("usage: /queue edit <id> <new text>");
                return;
            };
            if model.queue_mut().edit(id, text) {
                println!("entry #{id} updated.");
            } else {
                println!("no pending entry #{id} (see /queue) or new text is blank.");
            }
        }
        "remove" => {
            let Ok(id) = rest.parse::<u64>() else {
                println!("usage: /queue remove <id>");
                return;
            };
            match model.queue_mut().withdraw(id) {
                Some(text) => println!("withdrawd #{id}:\n{text}"),
                None => println!("no pending entry #{id}."),
            }
        }
        "move" => {
            let mut parts = rest.split_whitespace();
            let (id_part, dir) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let Ok(id) = id_part.parse::<u64>() else {
                println!("usage: /queue move <id> up|down");
                return;
            };
            let moved = match dir {
                "up" => model.queue_mut().move_entry(id, true),
                "down" => model.queue_mut().move_entry(id, false),
                _ => {
                    println!("usage: /queue move <id> up|down");
                    return;
                }
            };
            println!(
                "{}",
                if moved { format!("entry #{id} moved.") } else { format!("cannot move #{id} in that direction.") }
            );
        }
        "relane" => {
            // `/queue relane <id> <name>` — reassign a pending entry to
            // another lane (D2.4 改派); delivery order is untouched.
            let mut parts = rest.split_whitespace();
            let (id_part, lane) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let Ok(id) = id_part.parse::<u64>() else {
                println!("usage: /queue relane <id> <lane>");
                return;
            };
            if lane.is_empty() {
                println!("usage: /queue relane <id> <lane>");
                return;
            }
            println!(
                "{}",
                if model.queue_mut().reassign_lane(id, lane) {
                    format!("entry #{id} reassigned to lane {lane}.")
                } else {
                    format!("no pending entry #{id}, or already in lane {lane}.")
                }
            );
        }
        "merge" => {
            let merged = !model.queue().is_merged();
            model.queue_mut().set_merged(merged);
            println!(
                "merged delivery {} - pending entries are injected as {}.",
                if merged { "on" } else { "off" },
                if merged { "one combined message" } else { "separate turns" }
            );
        }
        "pop" => match model.queue_mut().cancel_last() {
            Some(text) => println!("withdrawd last queued message:\n{text}"),
            None => println!("queue is empty"),
        },
        "clear" => {
            let mut withdrawn = 0;
            while let Some(entry) = model.queue_mut().take_next() {
                model.queue_mut().finish_entry(entry, core::QueueEntryState::Withdrawn);
                withdrawn += 1;
            }
            println!(
                "{}",
                if withdrawn > 0 { format!("{withdrawn} pending entry(ies) withdrawn (visible under /queue recent).") } else { "queue is empty".to_string() }
            );
        }
        other => println!(
            "usage: /queue [send [--lane <name>]|lane <name>|edit <id> <text>|remove <id>|move <id> up|down|relane <id> <lane>|merge|pop|clear] (`{other}` is not a subcommand)"
        ),
    }
}

/// `/jobs` — subagent job control: list, spawn, stop, log.
fn handle_jobs_command(
    input: &str,
    jobs: &runtime::JobRegistry,
    state: &SessionShared,
    selection: &ProviderSelection,
    runtime_handle: &tokio::runtime::Handle,
) {
    let arg = input.trim().strip_prefix("/jobs").unwrap_or("").trim();
    let mut tokens = arg.split_whitespace();
    let sub = tokens.next().unwrap_or("");
    let rest = arg.strip_prefix(sub).unwrap_or("").trim();
    match sub {
        "" => {
            let records = jobs.list();
            if records.is_empty() {
                println!("no jobs yet (`/jobs spawn <task>` starts a context-isolated worker)");
                return;
            }
            for record in &records {
                println!(
                    "  #{} [{}] {} - {}",
                    record.id,
                    record.status.label(),
                    record.label,
                    truncate_chars(&record.prompt_head, 90)
                );
                if let Some(head) = &record.result_head {
                    println!("        result: {}", truncate_chars(head, 120));
                }
                if let Some(note) = &record.note {
                    println!("        note: {note}");
                }
            }
        }
        "spawn" => {
            let (write, task) = if let Some(rest) = rest.strip_prefix("--write") {
                (true, rest.trim())
            } else {
                (false, rest)
            };
            if task.is_empty() {
                println!("usage: /jobs spawn [--write] <task>   (workers run read-only unless --write)");
                return;
            }
            let runner = WorkerRunner {
                state: state.clone(),
                selection: selection.clone(),
                runtime_handle: runtime_handle.clone(),
                write,
            };
            let label: String = task.trim().chars().take(24).collect();
            match jobs.spawn(&label, task, Box::new(runner)) {
                Ok(id) => println!(
                    "spawned worker job #{id} ({}): it runs on its own isolated context; results arrive as the next supervisor turn.",
                    if write { "workspace-write" } else { "read-only" }
                ),
                Err(runtime::SpawnError::Saturated) => println!(
                    "job backlog is full ({} queued+running cap); stop or wait for one to finish",
                    env::var("HEARTFLOW_JOB_MAX_PENDING").as_deref().unwrap_or("20")
                ),
            }
        }
        "stop" => {
            let Ok(id) = rest.parse::<u64>() else {
                println!("usage: /jobs stop <id>");
                return;
            };
            println!(
                "{}",
                if jobs.stop(id) {
                    format!("stop requested for job #{id}; the in-flight call finishes, its result is discarded.")
                } else {
                    format!("no active job #{id}.")
                }
            );
        }
        "log" => {
            let mut parts = rest.split_whitespace();
            let (id_part, n_part) = (parts.next().unwrap_or(""), parts.next().unwrap_or("10"));
            let Ok(id) = id_part.parse::<u64>() else {
                println!("usage: /jobs log <id> [lines=10]");
                return;
            };
            let lines = n_part.parse::<usize>().unwrap_or(10).min(100);
            let tail = jobs.log_tail(id, lines);
            if tail.is_empty() {
                println!("no events for job #{id}.");
                return;
            }
            for line in tail {
                println!("  {line}");
            }
        }
        other => println!("usage: /jobs [spawn [--write] <task>|stop <id>|log <id> [lines]] (`{other}` is not a subcommand)"),
    }
}

/// `/mission` — the durable mission ledger (roadmap §1/§4 P-A): the
/// supervisor's long-lived task units. The disciplines (anti-early-complete,
/// anti-early-give-up, budget-not-kill) are enforced in `runtime::mission`,
/// not by prompt — this surface only translates them into messages.
async fn handle_mission_command(
    input: &str,
    missions: &runtime::MissionLedger,
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
    mode: &str,
) {
    let arg = input.trim().strip_prefix("/mission").unwrap_or("").trim();
    let mut tokens = arg.split_whitespace();
    let sub = tokens.next().unwrap_or("");
    let rest = arg.strip_prefix(sub).unwrap_or("").trim();
    match sub {
        "" => {
            let all = missions.list();
            if all.is_empty() {
                println!("no missions yet (`/mission create <objective>` opens one)");
                return;
            }
            for m in &all {
                let turns = m.budget.turns.map_or(String::from("-"), |v| v.to_string());
                let toks = m.budget.tokens.map_or(String::from("-"), |v| v.to_string());
                println!(
                    "  m{} [{}] {} - {}/{} turns, {}/{} tokens",
                    m.id,
                    m.status.label(),
                    truncate_chars(&m.objective, 90),
                    m.used.turns,
                    turns,
                    m.used.tokens,
                    toks
                );
                if m.budget_exhausted {
                    println!("        budget exhausted (reminders paused, mission stays open)");
                }
            }
        }
        "show" => {
            let Ok(id) = rest.parse::<u64>() else {
                println!("usage: /mission show <id>");
                return;
            };
            let Some(m) = missions.get(id) else {
                println!("no mission m{id}.");
                return;
            };
            println!("m{} [{}] {}", m.id, m.status.label(), m.objective);
            for (index, (criterion, state)) in m.criteria.iter().zip(&m.states).enumerate() {
                let mark = match state {
                    runtime::CriterionState::Pending => "[ ]",
                    runtime::CriterionState::Satisfied { .. } => "[x]",
                };
                println!("  {mark} #{index} {}", criterion.describe());
                if let runtime::CriterionState::Satisfied { evidence } = state {
                    println!("        evidence: {evidence}");
                }
            }
            if let Some(reason) = &m.blocked_reason {
                println!(
                    "  blocked claim {}/{}: {}",
                    m.blocked_streak, runtime::BLOCKED_STREAK_NEEDED, reason
                );
            }
            println!(
                "  usage: {} turns / {} tokens / {} ms wall",
                m.used.turns, m.used.tokens, m.wall_ms()
            );
            let receipts = m.receipts.iter().rev().take(5).collect::<Vec<_>>();
            if !receipts.is_empty() {
                println!("  recent receipts:");
                for receipt in receipts.into_iter().rev() {
                    println!("    [{}] {}: {}", receipt.kind, receipt.ts_ms, receipt.text);
                }
            }
        }
        "create" => {
            if rest.is_empty() {
                println!("usage: /mission create <objective>   (add criteria next: /mission criterion)");
                return;
            }
            let made = missions.create(rest, Vec::new(), runtime::MissionBudget::default());
            println!(
                "opened m{}; completion needs at least one criterion (`/mission criterion {} cmd|file|human|nodiag|free ...`)",
                made.id, made.id
            );
        }
        "criterion" => {
            let mut parts = rest.splitn(3, char::is_whitespace);
            let (id_part, kind, value) = (
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or("").trim(),
            );
            let Ok(id) = id_part.parse::<u64>() else {
                println!("usage: /mission criterion <id> cmd|file|human|nodiag|free ...");
                return;
            };
            let criterion = match kind {
                "cmd" if !value.is_empty() => runtime::CompletionCriterion::CommandSucceeds {
                    cmd: value.to_string(),
                },
                "file" => {
                    let (path, pattern) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
                    if path.is_empty() || pattern.is_empty() {
                        println!("usage: /mission criterion <id> file <path> <regex>");
                        return;
                    }
                    runtime::CompletionCriterion::FileMatches {
                        path: path.to_string(),
                        pattern: pattern.to_string(),
                    }
                }
                "human" => runtime::CompletionCriterion::HumanApproved,
                "nodiag" if !value.is_empty() => runtime::CompletionCriterion::NoDiagnostics {
                    scope: value.to_string(),
                },
                "free" if !value.is_empty() => runtime::CompletionCriterion::FreeText {
                    check: value.to_string(),
                },
                _ => {
                    println!(
                        "usage: /mission criterion <id> cmd <command> | file <path> <regex> | human | nodiag <scope> | free <claim>\n        (free/human/nodiag need explicit approval; free alone never completes)"
                    );
                    return;
                }
            };
            match missions.add_criterion(id, criterion) {
                Ok((_, index)) => println!("criterion #{index} added to m{id}"),
                Err(error) => println!("{error}"),
            }
        }
        "approve" => {
            let mut parts = rest.splitn(3, char::is_whitespace);
            let (id_part, index_part, evidence) = (
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or("").trim(),
            );
            let (Ok(id), Ok(index)) = (id_part.parse::<u64>(), index_part.parse::<usize>())
            else {
                println!("usage: /mission approve <id> <criterion-index> [evidence]");
                return;
            };
            match missions.approve_criterion(id, index, evidence) {
                Ok(_) => println!("criterion #{index} of m{id} approved"),
                Err(error) => println!("{error}"),
            }
        }
        "complete" => {
            let Ok(id) = rest.parse::<u64>() else {
                println!("usage: /mission complete <id>");
                return;
            };
            match missions.complete(id) {
                Ok(m) => println!("m{} complete; all criteria verified.", m.id),
                Err(error) => println!("refused: {error}"),
            }
        }
        "pause" | "resume" => {
            let Ok(id) = rest.parse::<u64>() else {
                println!("usage: /mission {sub} <id>");
                return;
            };
            let result = if sub == "pause" {
                missions.pause(id)
            } else {
                missions.resume(id)
            };
            match result {
                Ok(m) => {
                    println!("m{} is now {}", m.id, m.status.label());
                    // ST-3: a resume only flips the ledger; the autopilot
                    // loop exited when the mission blocked, so a loop-mode
                    // session restarts the drive here instead of stranding
                    // the mission Active with nobody pushing it. A paused
                    // mission stays parked — the operator resumes work, not
                    // necessarily the automation.
                    if sub == "resume" && mode == "loop" {
                        println!("loop: restarting the auto-continue drive for m{}...", m.id);
                        let _ = maybe_autopilot(state, runtime, prompter, mode).await;
                    } else if sub == "resume" {
                        println!("hint: /mode loop restarts the auto-continue drive for this mission.");
                    }
                }
                Err(error) => println!("{error}"),
            }
        }
        "blocked" => {
            if rest.is_empty() {
                println!("usage: /mission blocked <id> <reason>   (3 consecutive claims flip the status)");
                return;
            }
            let (id_part, reason) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let Ok(id) = id_part.parse::<u64>() else {
                println!("usage: /mission blocked <id> <reason>");
                return;
            };
            match missions.mark_blocked(id, reason) {
                Ok((m, flipped)) => {
                    if flipped {
                        println!("m{} is now blocked: {}", m.id, reason);
                    } else {
                        println!(
                            "blocked claim {}/{} recorded; a productive turn resets the streak",
                            m.blocked_streak, runtime::BLOCKED_STREAK_NEEDED
                        );
                    }
                }
                Err(error) => println!("{error}"),
            }
        }
        "turn" => {
            let mut parts = rest.split_whitespace();
            let (id_part, tokens_part) = (parts.next().unwrap_or(""), parts.next().unwrap_or("0"));
            let (Ok(id), Ok(tokens_used)) = (id_part.parse::<u64>(), tokens_part.parse::<u64>())
            else {
                println!("usage: /mission turn <id> [tokens=0]   (accounts one productive turn)");
                return;
            };
            match missions.record_turn(id, tokens_used) {
                Ok((m, attention)) => {
                    println!("m{} advanced to {} turns", m.id, m.used.turns);
                    if let Some(reminder) = attention {
                        println!("  ! {reminder}");
                    }
                }
                Err(error) => println!("{error}"),
            }
        }
        other => println!(
            "usage: /mission [show <id>|create <objective>|criterion|approve|complete|pause|resume|blocked|turn] (`{other}` is not a subcommand)"
        ),
    }
}

/// `/schedule` — scheduled runs (roadmap §1 pi-job item 6): due schedules are
/// handed out as normal subagent jobs by the loop-top drain or by an explicit
/// `/schedule run-due`; there is no resident daemon by design.
fn handle_schedule_command(
    input: &str,
    schedules: &runtime::ScheduleStore,
    jobs: &runtime::JobRegistry,
    state: &SessionShared,
    selection: &mut ProviderSelection,
    runtime_handle: &tokio::runtime::Handle,
    links: &mut std::collections::HashMap<u64, (u64, u64, bool)>,
) {
    let arg = input.trim().strip_prefix("/schedule").unwrap_or("").trim();
    let mut tokens = arg.split_whitespace();
    let sub = tokens.next().unwrap_or("");
    let rest = arg.strip_prefix(sub).unwrap_or("").trim();
    match sub {
        "" => {
            let all = schedules.list();
            if all.is_empty() {
                println!(
                    "no schedules yet (`/schedule add <name> interval 30m <prompt>` or `/schedule add <name> once +1h <prompt>`)"
                );
                return;
            }
            let now = runtime::epoch_ms();
            for s in &all {
                let state_mark = if s.enabled { " " } else { "!" };
                let due_mark = if s.is_due(now) { " (due)" } else { "" };
                println!(
                    "  {state_mark}#{} {} [{}] {}{} - {}",
                    s.id,
                    s.name,
                    describe_schedule_trigger(&s.trigger),
                    if s.quiet { "quiet " } else { "" },
                    due_mark,
                    truncate_chars(&s.prompt, 80)
                );
            }
        }
        "add" => {
            let mut parts = rest.splitn(4, char::is_whitespace);
            let (name, kind, spec, tail) = (
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or("").trim(),
            );
            if name.is_empty() || tail.is_empty() {
                println!(
                    "usage: /schedule add <name> interval <spec> [--quiet] <prompt> | /schedule add <name> once <+spec|epoch-ms> [--quiet] <prompt>"
                );
                return;
            }
            let (quiet, prompt) = match tail.strip_prefix("--quiet ") {
                Some(p) => (true, p.trim()),
                None if tail == "--quiet" => (true, ""),
                None => (false, tail),
            };
            if prompt.is_empty() {
                println!("a schedule needs a prompt to run.");
                return;
            }
            let result = match kind {
                "interval" => runtime::parse_duration_spec(spec).map(|every_secs| {
                    (
                        runtime::ScheduleTrigger::Interval { every_secs },
                        runtime::CatchUp::None,
                    )
                }),
                "once" => {
                    if let Some(relative) = spec.strip_prefix('+') {
                        runtime::parse_duration_spec(relative).map(|secs| {
                            (
                                runtime::ScheduleTrigger::Once {
                                    at_ms: runtime::epoch_ms() + secs.saturating_mul(1000),
                                },
                                runtime::CatchUp::None,
                            )
                        })
                    } else if spec.chars().all(|c| c.is_ascii_digit()) && !spec.is_empty() {
                        spec.parse::<u64>().map(|at_ms| {
                            (
                                runtime::ScheduleTrigger::Once { at_ms },
                                runtime::CatchUp::None,
                            )
                        }).map_err(|_| runtime::ScheduleError::BadSpec(spec.to_string()))
                    } else {
                        Err(runtime::ScheduleError::BadSpec(spec.to_string()))
                    }
                }
                _ => Err(runtime::ScheduleError::BadSpec(kind.to_string())),
            };
            match result.and_then(|(trigger, catch_up)| {
                schedules.add(name, prompt, trigger, catch_up, quiet)
            }) {
                Ok(s) => println!(
                    "schedule #{} `{}` added ({}){}",
                    s.id,
                    s.name,
                    describe_schedule_trigger(&s.trigger),
                    if quiet { "; success will not wake the supervisor" } else { "" }
                ),
                Err(error) => println!("{error}"),
            }
        }
        "run-due" => {
            let launched = drain_due_schedules(
                schedules, jobs, links, state, selection, runtime_handle, runtime::DueReason::RunDue,
            );
            if launched == 0 {
                println!("nothing due right now (`/schedule` marks due items).");
            }
        }
        "show" => {
            let Ok(id) = rest.parse::<u64>() else {
                println!("usage: /schedule show <id>");
                return;
            };
            let Some(s) = schedules.get(id) else {
                println!("no schedule #{id}.");
                return;
            };
            println!(
                "#{} `{}` [{}] {} prompt: {}",
                s.id,
                s.name,
                describe_schedule_trigger(&s.trigger),
                if s.enabled { "enabled" } else { "paused" },
                s.prompt
            );
            if s.history.is_empty() {
                println!("  no runs yet.");
                return;
            }
            println!("  recent runs:");
            for run in s.history.iter().rev().take(10).collect::<Vec<_>>().into_iter().rev() {
                let outcome = match &run.outcome {
                    runtime::RunOutcome::Started => String::from("started"),
                    runtime::RunOutcome::SkippedOverlap => String::from("skipped (overlap)"),
                    runtime::RunOutcome::SkippedCatchUp => String::from("skipped (missed, catch-up none)"),
                    runtime::RunOutcome::Complete => String::from("complete"),
                    runtime::RunOutcome::Failed { error } => format!("failed: {error}"),
                };
                println!("    @{} {} ({})", run.started_ms, outcome, run.reason.label());
            }
        }
        "pause" | "resume" | "delete" => {
            let Ok(id) = rest.parse::<u64>() else {
                println!("usage: /schedule {sub} <id>");
                return;
            };
            let result = match sub {
                "pause" => schedules.pause(id).map(|_| ()),
                "resume" => schedules.resume(id).map(|_| ()),
                _ => schedules.delete(id).map(|_| ()),
            };
            println!(
                "{}",
                match result {
                    Ok(()) => format!("schedule #{id} {sub}d."),
                    Err(error) => error.to_string(),
                }
            );
        }
        other => println!(
            "usage: /schedule [add|run-due|show <id>|pause <id>|resume <id>|delete <id>] (`{other}` is not a subcommand)"
        ),
    }
}

fn describe_schedule_trigger(trigger: &runtime::ScheduleTrigger) -> String {
    match trigger {
        runtime::ScheduleTrigger::Once { at_ms } => format!("once @ epoch {at_ms}"),
        runtime::ScheduleTrigger::Interval { every_secs } => format!("every {every_secs}s"),
    }
}

/// `/guide <TASK>` — assemble the three-part guide draft (prior work, current
/// state, next task) locally with zero token spend, mirroring Qoder's
/// guidance flow. D2.5 引导直接发送: the default is to *run* the assembled
/// draft as the next turn immediately; `/guide draft` keeps the old
/// print-and-edit flow; `/guide queue` files it as the next queued turn.
#[allow(clippy::too_many_arguments)]
async fn handle_guide_command(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    model: &mut HeartModel,
    plan: &ReplPlanState,
    prompter: &mut CliPermissionPrompter,
    editor: &mut editor::ReplEditor,
    mode: &str,
    input: &str,
) {
    let arg = input.trim().strip_prefix("/guide").unwrap_or("").trim();
    let (route, task) = if let Some(rest) = arg.strip_prefix("draft ") {
        ("draft", rest.trim())
    } else if let Some(rest) = arg.strip_prefix("queue ") {
        ("queue", rest.trim())
    } else if arg == "draft" || arg == "queue" {
        ("draft", "")
    } else {
        ("send", arg)
    };
    if task.is_empty() {
        println!("usage: /guide <task> (run now) | /guide draft <task> (print for editing) | /guide queue <task> (queue for next turn)");
        return;
    }
    // Assembly lives in `core` so the ratatui Ctrl+G overlay builds the exact
    // same draft; this command only assembles and routes it.
    let context = guide_context(
        &runtime.session().messages,
        runtime.executor().todo_ledger().pending_tasks(),
    );
    let draft = guide_draft(&context, task);
    match route {
        "draft" => {
            println!("guide draft (edit and resend, or paste into the next message):\n");
            println!("{draft}\n");
        }
        "queue" => match model.queue_mut().send(&draft, core::QueueSource::Guide) {
            Some(id) => println!("guide queued as #{id}; it runs as the next turn."),
            None => println!("queue is full; nothing queued."),
        },
        _ => {
            // Direct send: the guide IS the next user message, assembled from
            // live transcript context so the model re-enters with anchored
            // state instead of a bare task line.
            model.begin_turn();
            let delivery = if plan.planning {
                run_turn_interactive(state, runtime, &draft, Some(&mut BlockPrompter)).await
            } else {
                run_turn_interactive(state, runtime, &draft, Some(prompter)).await
            };
            model.end_turn();
            match delivery {
                Ok(outcome) => {
                    maybe_auto_compact(state, runtime);
                    editor.note_turn(outcome.ok);
                    // A loop-mode session auto-continues from a guided turn
                    // too (semantic completeness, B项).
                    if plan.active_policy().is_none() {
                        let _ = maybe_autopilot(state, runtime, prompter, mode).await;
                    }
                }
                Err(error) => {
                    println!("guide delivery failed: {error}");
                    editor.note_turn(false);
                }
            }
        }
    }
}

/// Jump the live REPL to a previously saved section: reload session N (using the
/// same newest-first numbering as `/sessions`) into a freshly built runtime,
/// swapping the in-place conversation without restarting the process. A
/// non-numeric argument is a topic: the newest session whose opening message
/// contains it opens when unique, otherwise the matches list with their
/// numbers. The connected MCP servers and provider are reconstructed by
/// `build_runtime`.
fn handle_open_command(
    state: &SessionShared,
    input: &str,
    selection: &ProviderSelection,
    mode: &str,
    runtime: &mut AgentRuntime,
) {
    let arg = input.trim().strip_prefix("/open").unwrap_or("").trim();
    if arg.is_empty() {
        println!("usage: /open <N|TOPIC>   (N is a 1-based index from /sessions; TOPIC matches the opening message)");
        return;
    }
    let Ok(index) = arg.parse::<usize>() else {
        open_session_by_topic(state, arg, selection, mode, runtime);
        return;
    };
    let sessions = list_sessions();
    if sessions.is_empty() {
        println!("no saved sessions to open");
        return;
    }
    if index == 0 || index > sessions.len() {
        println!("session {index} is out of range (1..{})", sessions.len());
        return;
    }
    open_session_path(state, index, &sessions[index - 1], selection, mode, runtime);
}

/// `/open TOPIC`: newest-first scan over saved sessions, matching the argument
/// case-insensitively against each session's opening message. A unique match
/// opens at once; several matches print their `/sessions` numbers so the user
/// can disambiguate with `/open N`.
fn open_session_by_topic(
    state: &SessionShared,
    topic: &str,
    selection: &ProviderSelection,
    mode: &str,
    runtime: &mut AgentRuntime,
) {
    let needle = topic.to_lowercase();
    let matches: Vec<(usize, std::path::PathBuf)> = list_sessions()
        .into_iter()
        .enumerate()
        .filter(|(_, path)| {
            session_topic(path).is_some_and(|text| text.to_lowercase().contains(&needle))
        })
        .collect();
    match matches.as_slice() {
        [] => println!(
            "no session matches `{topic}` (searched each session's opening message); try /sessions [FILTER]"
        ),
        [(index, path)] => open_session_path(state, *index, path, selection, mode, runtime),
        many => {
            println!("several sessions match `{topic}`; open one by number:");
            for (index, path) in many {
                let text = session_topic(path).unwrap_or_else(|| "(untitled)".to_string());
                println!("  {index}. {text}  ({})", path.display());
            }
        }
    }
}

/// Load the session file at `index` (1-based, `/sessions` numbering) into a
/// fresh runtime, swapping it in as the live conversation.
fn open_session_path(
    state: &SessionShared,
    index: usize,
    path: &Path,
    selection: &ProviderSelection,
    mode: &str,
    runtime: &mut AgentRuntime,
) {
    match load_saved_session(path) {
        Ok(session) => match build_runtime(state, session, selection.clone(), true, mode) {
            Ok(fresh) => {
                *runtime = fresh;
                // Own this transcript's identity so subsequent turns keep
                // writing the same file and history row.
                adopt_session_path(state, path);
                println!("opened session {index}: {}", path.display());
            }
            Err(error) => println!("failed to start runtime for session {index}: {error}"),
        },
        Err(error) => println!("failed to load session {index}: {error}"),
    }
}

/// Append a durable note to `~/.heartflow/MEMORY.md` as a single `- ` bullet,
/// skipping exact duplicates so the file stays token-cheap. Returns `true` when
/// a new line was written, `false` when it was already present.
pub(crate) fn remember_note(home: &Path, text: &str) -> io::Result<bool> {
    let text = text.trim();
    if text.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty note"));
    }
    let dir = home.join(".heartflow");
    fs::create_dir_all(&dir)?;
    let path = dir.join("MEMORY.md");
    let mut existing = fs::read_to_string(&path).unwrap_or_default();
    let bullet = format!("- {text}");
    if existing.lines().any(|line| line.trim() == bullet) {
        return Ok(false);
    }
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(&bullet);
    existing.push('\n');
    fs::write(&path, existing)?;
    Ok(true)
}

/// `/remember <note>`: persist a pitfall or preference the user asked to keep.
fn handle_remember_command(input: &str) {
    let note = input.trim().strip_prefix("/remember").unwrap_or("").trim();
    if note.is_empty() {
        println!("usage: /remember <note>   (persist a durable pitfall or preference)");
        return;
    }
    match remember_note(&home_dir(), note) {
        Ok(true) => println!("saved to memory: {note}"),
        Ok(false) => println!("already in memory (not duplicated): {note}"),
        Err(error) => println!("failed to write memory: {error}"),
    }
}

/// Auto-record hard pitfalls into durable memory so later sessions dodge the
/// same trap: a note counts as a pitfall only when its task failed every attempt
/// and was skipped (completed notes are not pitfalls). Writes are best-effort
/// and deduped by `remember_note`.
fn record_pitfalls(home: &Path, memory: &[String]) {
    for note in memory.iter().filter(|note| note.contains("SKIPPED")) {
        let _ = remember_note(home, note);
    }
}

/// Ask the user to confirm an in-place restart; returns `true` to proceed.
fn confirm_restart() -> bool {
    Confirm::new("Restart heartflow now? The current session is not saved automatically.")
        .with_default(false)
        .prompt()
        .unwrap_or(false)
}

/// `/bootstrap <provider> [model] [--reorganize]` — 模型自举: swap the session's
/// provider/token/model in one validated step. Pre-flight runs BEFORE any
/// switch (resolve → reachability probe → runtime build), so a failed
/// bootstrap never strands the session on a broken transport. Memory carries
/// over automatically: MEMORY.md is model-agnostic and re-read every turn.
/// `--reorganize` additionally runs one in-session turn asking the NEW model
/// to reorganize MEMORY.md, written back atomically behind a `.bak` backup.
async fn handle_bootstrap_command(
    state: &SessionShared,
    input: &str,
    selection: &mut ProviderSelection,
    runtime: &mut AgentRuntime,
    mode: &str,
    _catalog: &ProviderCatalog,
) {
    let requested = input
        .strip_prefix("/bootstrap")
        .map(str::trim)
        .unwrap_or_default();
    if requested.is_empty() {
        println!("usage: /bootstrap <provider> [model] [--token <key>] [--reorganize]");
        println!(
            "       validates the new provider (credentials + endpoint reachability) BEFORE switching, then swaps the live runtime; memory carries over automatically."
        );
        println!(
            "       an explicitly named model is never changed; when none is named the flash tier is auto-selected; the endpoint's own model list and context windows are cached into the catalog."
        );
        return;
    }
    let reorganize = requested.contains("--reorganize");
    let requested = requested.replace("--reorganize", "").trim().to_string();
    // `--token <key>` supplies an inline credential (config's `api_key` shape)
    // instead of relying on the provider.toml/env resolution.
    let mut inline_token: Option<String> = None;
    let mut tokens: Vec<&str> = requested.split_whitespace().collect();
    if let Some(position) = tokens.iter().position(|part| *part == "--token") {
        if position + 1 < tokens.len() {
            inline_token = Some(tokens[position + 1].to_string());
            tokens.drain(position..=position + 1);
        } else {
            println!("usage: --token needs a key argument");
            return;
        }
    }
    let mut parts = tokens.into_iter();
    let Some(provider) = parts.next() else {
        println!("usage: /bootstrap <provider> [model] [--token <key>] [--reorganize]");
        return;
    };
    let model = parts.next();

    // Pre-flight 1: resolve. An unknown provider never touches the live runtime.
    let mut next = match resolve_selection(Some(provider), model) {
        Ok(next) => next,
        Err(error) => {
            println!("bootstrap aborted (nothing switched): {error}");
            return;
        }
    };
    // Pre-flight 2: reachability. The models list doubles as a credential +
    // endpoint proof for every protocol; a probe failure aborts before the
    // live runtime is touched. This is the gate that makes "switched" mean
    // "verified": without it a bad token or wrong base_url still reports
    // bootstrap complete and fails on first use.
    let mut server_context: Option<u64> = None;
    if let ProviderSelection::Profile(profile) = &mut next {
        if let Some(token) = &inline_token {
            profile.api_key = token.clone();
        }
        // Snapshot the profile so the probe/discovery/balance reads below do
        // not hold the `&mut next` borrow alive across `next.set_model`.
        let snapshot = profile.clone();
        match probe_models(&snapshot).await {
            Ok((models, supports_balance)) => {
                println!(
                    "pre-flight ok: endpoint reachable, {} models listed",
                    models.len()
                );
                // An explicitly named model is NEVER changed: it is verified
                // against the endpoint's own list and kept either way (some
                // endpoints under-report). Autonomous choice (flash tier / first
                // listed) applies only when the operator named no model.
                let picked = if let Some(requested_model) = model {
                    if models.iter().any(|info| info.id == requested_model) {
                        println!("model `{requested_model}` verified against the endpoint list");
                    } else {
                        println!(
                            "note: `{requested_model}` is not in the endpoint's list; your explicit choice is kept unchanged"
                        );
                    }
                    Some(requested_model.to_string())
                } else if let Some(flash) = models
                    .iter()
                    .map(|info| info.id.as_str())
                    .find(|id| id.contains("flash"))
                {
                    println!("no model named; auto-selected the flash tier: {flash}");
                    Some(flash.to_string())
                } else {
                    let fallback = models.first().map(|info| info.id.clone());
                    match &fallback {
                        Some(id) => println!("no model named and no flash tier; selected: {id}"),
                        None => println!("endpoint listed no models; keeping the catalog default"),
                    }
                    fallback
                };
                if let Some(picked) = picked {
                    server_context = models
                        .iter()
                        .find(|info| info.id == picked)
                        .and_then(|info| info.context_length.map(u64::from));
                    next.set_model(picked);
                    // Adopt the endpoint-reported window on the ACTIVE profile
                    // too, not just the catalog: the ctx bar and auto-compact
                    // budget read the selection, and the previous provider's
                    // window must not linger after a bootstrap.
                    next.set_context_window(server_context);
                }
                // Balance is a DeepSeek-only extra (`/user/balance`); other
                // providers without that endpoint just skip the line.
                if supports_balance && snapshot.base_url.contains("deepseek") {
                    let client = OpenAiClient::new(snapshot.api_key.clone())
                        .with_base_url(snapshot.base_url.clone());
                    if let Ok(balance) = client.get_balance().await {
                        for info in &balance.balance_infos {
                            println!("balance: {} {}", info.total_balance, info.currency);
                        }
                    }
                }
                // Discovery cache: file the endpoint's own list into the catalog
                // so `/model` completion stays fresh even when the compiled-in
                // seed is stale (models retire and rename over time).
                let discovered: Vec<(String, Option<u64>)> = models
                    .iter()
                    .map(|info| (info.id.clone(), info.context_length.map(u64::from)))
                    .collect();
                if !discovered.is_empty() {
                    if let Err(error) = persist_discovered(
                        &home_dir(),
                        &snapshot.base_url,
                        snapshot.protocol,
                        &discovered,
                    ) {
                        println!("(could not cache discovered models: {error})");
                    }
                }
            }
            Err(error) => {
                println!("bootstrap aborted (nothing switched): endpoint check failed: {error}");
                println!(
                    "hint: verify the token and base_url; if the model id is new, name it explicitly (`/bootstrap <provider> <model>`) — an explicit id is passed through verbatim and can be checked against the vendor's docs via web search"
                );
                return;
            }
        }
    }
    // Pre-flight 3: build the replacement runtime. On any failure the live
    // runtime stays untouched.
    let rebuilt = match build_runtime(state, runtime.session().clone(), next.clone(), true, mode) {
        Ok(rebuilt) => rebuilt,
        Err(error) => {
            println!("bootstrap aborted (nothing switched): {error}");
            return;
        }
    };
    *runtime = rebuilt;
    *selection = next;
    // Price mirror follows the swap (same discipline as /model and /provider).
    set_display_price(state, active_model_price(selection, _catalog));
    println!("bootstrap complete -> model {}", selection.model());
    println!("memory carried over: MEMORY.md is model-agnostic and re-read every turn.");
    if let ProviderSelection::Profile(profile) = selection {
        if let Err(error) = persist_model(
            &home_dir(),
            &profile.base_url,
            profile.protocol,
            &profile.model,
            server_context.or(profile.context_window.map(u64::from)),
            ModelSource::User,
        ) {
            println!("(could not record model in catalog: {error})");
        }
        println!(
            "note: the switch is session-live; to make it permanent, set the same [provider] in config.toml"
        );
    }
    if reorganize {
        reorganize_memory(state, runtime).await;
    }
}

/// The `--reorganize` step: one in-session turn on the NEW model rewrites
/// MEMORY.md for its own future use. The transcript keeps the change
/// auditable; the file is backed up and only replaced on a non-empty result.
async fn reorganize_memory(state: &SessionShared, runtime: &mut AgentRuntime) {
    let path = home_dir().join(".heartflow").join("MEMORY.md");
    let Ok(current) = std::fs::read_to_string(&path) else {
        println!("no MEMORY.md to reorganize (skipped)");
        return;
    };
    let prompt = format!(
        "Reorganize the following long-term memory file for your own future use: deduplicate, group by theme, keep every durable preference, pitfall and fact, drop nothing that is still useful, and output ONLY the reorganized markdown with no commentary.

--- MEMORY.md ---
{current}"
    );
    let outcome = run_turn_interactive(state, runtime, &prompt, None).await;
    let Ok(outcome) = outcome else {
        println!("reorganize turn failed; memory untouched");
        return;
    };
    if !outcome.ok {
        println!("reorganize turn failed; memory untouched");
        return;
    }
    let text: String = runtime
        .session()
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(
                    "
",
                )
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        println!("model produced no reorganized memory; file untouched");
        return;
    }
    let backup = path.with_extension("md.bak");
    if std::fs::copy(&path, &backup).is_err() {
        println!("could not back up MEMORY.md; file untouched");
        return;
    }
    let temp = path.with_extension("md.tmp");
    if std::fs::write(&temp, &text)
        .and_then(|_| std::fs::rename(&temp, &path))
        .is_ok()
    {
        println!("memory reorganized (backup: {})", backup.display());
    } else {
        println!(
            "could not write the reorganized memory; backup at {}",
            backup.display()
        );
    }
}

/// `/web on|off|local` — 联网三态 (roadmap C-1). Rebuilds the runtime so the
/// model sees the tool set change; memory and transcript are unaffected.
fn handle_web_command(state: &SessionShared, input: &str) {
    let requested = input
        .strip_prefix("/web")
        .map(str::trim)
        .unwrap_or_default();
    let Some(access) = crate::tool_exec::WebAccess::parse(requested) else {
        let current = crate::tool_exec::WebAccess::from_u8(
            lock_state(state)
                .web_access
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        println!(
            "usage: /web on|off|local   (current: {}; local keeps document/knowledge-base retrieval)",
            current.label()
        );
        return;
    };
    // Shared atomic with the executor: the flip lands on the very next request.
    lock_state(state)
        .web_access
        .store(access.as_u8(), std::sync::atomic::Ordering::Relaxed);
    println!("web access -> {}", access.label());
}

/// `/effort [none|low|medium|high|max]` — reasoning depth switch. Rebuilds the
/// runtime; `none` turns thinking off (reasoning_effort cleared).
fn handle_effort_command(
    state: &SessionShared,
    input: &str,
    selection: &mut ProviderSelection,
    runtime: &mut AgentRuntime,
    mode: &str,
) {
    let requested = input
        .strip_prefix("/effort")
        .map(str::trim)
        .unwrap_or_default();
    let current = match selection {
        ProviderSelection::Profile(profile) => profile.reasoning_effort.clone(),
        ProviderSelection::Env { .. } => None,
    };
    if requested.is_empty() {
        println!(
            "reasoning effort: {}",
            current.as_deref().unwrap_or("none (off)")
        );
        println!("switch with: /effort none|low|medium|high|max");
        return;
    }
    let normalized = requested.to_ascii_lowercase();
    if !matches!(
        normalized.as_str(),
        "none" | "low" | "medium" | "high" | "max"
    ) {
        println!("unknown effort '{requested}'; choose none, low, medium, high or max.");
        return;
    }
    let mut next = selection.clone();
    match &mut next {
        ProviderSelection::Profile(profile) => {
            profile.reasoning_effort = if normalized == "none" {
                None
            } else {
                Some(normalized.clone())
            };
        }
        ProviderSelection::Env { .. } => {
            println!("env-driven selection has no reasoning-effort knob; nothing switched.");
            return;
        }
    }
    match build_runtime(state, runtime.session().clone(), next.clone(), true, mode) {
        Ok(rebuilt) => {
            *runtime = rebuilt;
            *selection = next;
            println!("reasoning effort -> {normalized}");
            // Persist when the (profile) selection actually carries the knob:
            // session-only effort used to be silently rolled back by the next
            // config hot-reload (the disk value wins there). Env-driven
            // selections have no knob — persisting `None` would erase a
            // hand-written `reasoning_effort`, so they stay untouched.
            if let Some(effort) = selection.reasoning_effort() {
                if let Err(error) = provider::persist_reasoning_effort(&home_dir(), Some(effort)) {
                    println!("(could not persist effort: {error})");
                }
            }
        }
        Err(error) => println!("failed to switch effort: {error}"),
    }
}

/// `/models` — refresh `/model` completion from the wire: probe the active
/// endpoint's own model list (which also proves the credential), cache it
/// into the catalog, and print it. No argument switches anything; the
/// current model is verified against the server list when it reports one.
async fn handle_models_command(selection: &ProviderSelection, catalog: &ProviderCatalog) {
    let ProviderSelection::Profile(profile) = selection else {
        println!("model self-discovery needs an explicit provider; the env-driven Anthropic path exposes no model endpoint (use `/provider <name>` first)");
        return;
    };
    println!("probing {} ...", profile.base_url);
    match probe_models(profile).await {
        Ok((models, supports_balance)) => {
            println!("models ({}) from the endpoint:", models.len());
            for info in &models {
                let context = info
                    .context_length
                    .map_or_else(|| "ctx n/a".to_string(), |t| format!("ctx {t}"));
                println!("  {:<32} {context}", info.id);
            }
            let current_known = models.iter().any(|info| info.id == profile.model);
            if !current_known {
                println!(
                    "note: current model `{}` is not in the endpoint list; it is kept unchanged (some endpoints under-report)",
                    profile.model
                );
            }
            let discovered: Vec<(String, Option<u64>)> = models
                .iter()
                .map(|info| (info.id.clone(), info.context_length.map(u64::from)))
                .collect();
            if discovered.is_empty() {
                println!("endpoint listed no models; catalog untouched");
                return;
            }
            match persist_discovered(
                &home_dir(),
                &profile.base_url,
                profile.protocol,
                &discovered,
            ) {
                Ok(()) => println!(
                    "catalog updated: `/model` completion now offers the live list ({} models filed under {})",
                    discovered.len(),
                    catalog
                        .provider_key_by_host(&profile.base_url)
                        .unwrap_or_else(|| profile.base_url.clone())
                ),
                Err(error) => println!("(could not cache discovered models: {error})"),
            }
            // DeepSeek-shaped surfaces also expose `/user/balance`; a free
            // extra line when the endpoint supports it.
            if supports_balance {
                let client = OpenAiClient::new(profile.api_key.clone())
                    .with_base_url(profile.base_url.clone());
                if let Ok(balance) = client.get_balance().await {
                    for info in &balance.balance_infos {
                        println!("balance: {} {}", info.total_balance, info.currency);
                    }
                }
            }
        }
        Err(error) => println!("probe failed (catalog untouched): {error}"),
    }
}

/// `/provider [NAME] [MODEL]` — show the active provider and the switchable
/// list (no argument), or swap the live transport (name + optional model).
/// Lighter than `/bootstrap`: no pre-flight probe, same swap-or-stay contract
/// as `/model` — the runtime is rebuilt first and the selection only moves on
/// success, so a failed switch never strands the session.
fn handle_provider_command(
    state: &SessionShared,
    input: &str,
    selection: &mut ProviderSelection,
    runtime: &mut AgentRuntime,
    mode: &str,
    catalog: &ProviderCatalog,
) {
    let requested = input
        .strip_prefix("/provider")
        .map(str::trim)
        .unwrap_or_default();
    if requested.is_empty() {
        print_provider_status(selection, catalog);
        return;
    }
    let mut parts = requested.split_whitespace();
    let Some(name) = parts.next() else {
        return;
    };
    let model = parts.next();
    if parts.next().is_some() {
        println!("usage: /provider [NAME] [MODEL]");
        return;
    }
    let next = match resolve_selection(Some(name), model) {
        Ok(next) => next,
        Err(error) => {
            println!("provider switch aborted (nothing changed): {error}");
            return;
        }
    };
    match build_runtime(state, runtime.session().clone(), next.clone(), true, mode) {
        Ok(rebuilt) => {
            *runtime = rebuilt;
            *selection = next;
            println!("provider -> {name} (model {})", selection.model());
            // Persist so the switch survives restart (user config.toml
            // `[provider]` name/model, comment-preserving), plus the catalog
            // record `/model` keeps for listings and completion.
            if let ProviderSelection::Profile(profile) = selection {
                if let Err(error) = persist_model(
                    &home_dir(),
                    &profile.base_url,
                    profile.protocol,
                    &profile.model,
                    profile.context_window.map(u64::from),
                    ModelSource::User,
                ) {
                    println!("(could not record model in catalog: {error})");
                }
            }
            persist_active_selection(selection, Some(name));
            // Re-lookup the display price so the usage line tracks the new model.
            set_display_price(state, active_model_price(selection, catalog));
        }
        Err(error) => println!("failed to switch provider: {error}"),
    }
}

/// `/provider` with no argument: the active provider (same report `/model`
/// gives) plus every switchable name — the builtins and any `[provider]`
/// transport configured in config.toml — so the command is discoverable
/// without reading docs.
fn print_provider_status(selection: &ProviderSelection, catalog: &ProviderCatalog) {
    print_model_status(selection, catalog);
    let settings = load_merged_settings(&cwd_dir(), &home_dir());
    let mut names: Vec<String> = provider::builtin_provider_names()
        .iter()
        .map(ToString::to_string)
        .collect();
    if settings.base_url.is_some() || settings.api_key_env.is_some() || settings.api_key.is_some() {
        let custom = settings
            .name
            .clone()
            .unwrap_or_else(|| "custom (config.toml [provider])".to_string());
        if !names.contains(&custom) {
            names.push(custom);
        }
    }
    println!(
        "switch with: /provider NAME [MODEL]  (available: {})",
        names.join(", ")
    );
}

/// `std::env::current_dir`, collapsing errors to a fallible-but-rare empty
/// path. Only the provider-settings merge needs it here; the merge tolerates
/// absent files, so an empty path just skips the project layer.
pub(crate) fn cwd_dir() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_default()
}

fn handle_model_command(
    state: &SessionShared,
    input: &str,
    selection: &mut ProviderSelection,
    runtime: &mut AgentRuntime,
    mode: &str,
    catalog: &ProviderCatalog,
) {
    let requested = input
        .strip_prefix("/model")
        .map(str::trim)
        .unwrap_or_default();
    if requested.is_empty() {
        print_model_status(selection, catalog);
        return;
    }

    let mut requested = requested.to_string();
    // Catalog-aware disambiguation, ordered exact → unique prefix → unique
    // substring → ambiguous (abort with candidates) → unknown (trust + note).
    // Typing `/model flash` lands on deepseek-flash instead of 404ing later.
    if let ProviderSelection::Profile(profile) = selection {
        let known: Vec<String> = catalog
            .provider_by_host(&profile.base_url)
            .map(|provider| provider.models.iter().map(|m| m.id.clone()).collect())
            .unwrap_or_default();
        let ci_exact = known
            .iter()
            .find(|id| id.eq_ignore_ascii_case(&requested))
            .cloned();
        if let Some(exact) = ci_exact {
            // Same id, different casing: adopt the catalog's spelling so the
            // recorded/persisted id is canonical.
            if exact != requested {
                requested = exact;
            }
        } else if !known.is_empty() {
            let needle = requested.to_ascii_lowercase();
            let prefix: Vec<&String> = known
                .iter()
                .filter(|id| id.to_ascii_lowercase().starts_with(&needle))
                .collect();
            let contains: Vec<&String> = known
                .iter()
                .filter(|id| id.to_ascii_lowercase().contains(&needle))
                .collect();
            let unique: Option<&String> = match (prefix.as_slice(), contains.as_slice()) {
                ([one], _) => Some(one),
                ([], [one]) => Some(one),
                ([], []) => None,
                (prefix, _) if prefix.len() > 1 => {
                    println!(
                        "ambiguous model {requested:?}; prefix candidates: {} — type more characters",
                        prefix
                            .iter()
                            .map(|id| id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    return;
                }
                (_, contains) => {
                    println!(
                        "ambiguous model {requested:?}; candidates: {} — type more characters",
                        contains
                            .iter()
                            .map(|id| id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    return;
                }
            };
            match unique {
                Some(matched) => {
                    println!("matched catalog model {matched} for {requested:?}");
                    requested = matched.clone();
                }
                None => println!(
                    "note: {requested:?} is not in the catalog for this provider; sending as-is"
                ),
            }
        }
    }

    let mut next = selection.clone();
    next.set_model(requested.clone());
    match build_runtime(state, runtime.session().clone(), next.clone(), true, mode) {
        Ok(rebuilt) => {
            *runtime = rebuilt;
            *selection = next;
            println!("model -> {}", selection.model());
            // Record the switch into the catalog's persisted layer so the model
            // shows up in later `/model` listings and completion. Only a profile
            // selection carries the base_url/protocol to file it under; the
            // legacy env-Anthropic path has no endpoint to key on, so it skips.
            if let ProviderSelection::Profile(profile) = selection {
                if let Err(error) = persist_model(
                    &home_dir(),
                    &profile.base_url,
                    profile.protocol,
                    &requested,
                    profile.context_window.map(u64::from),
                    ModelSource::User,
                ) {
                    println!("(could not record model in catalog: {error})");
                }
            }
            // Also write the model into the user config.toml so the switch
            // survives restart, same as `/provider` does for provider+model.
            persist_active_selection(selection, None);
            // Re-lookup the display price so the usage line tracks the new model.
            set_display_price(state, active_model_price(selection, catalog));
        }
        Err(error) => println!("failed to switch model: {error}"),
    }
}

/// Write the just-applied switch back into the user-layer config.toml
/// (`provider::persist_selection`, toml_edit comment-preserving). `/provider`
/// passes the provider name so a builtin switch pins itself; `/model` passes
/// `None` so only the model line moves. Best-effort: a failure prints but
/// never aborts the already-applied switch.
fn persist_active_selection(selection: &ProviderSelection, provider: Option<&str>) {
    let model = selection.model().to_string();
    if let Err(error) = persist_selection(&home_dir(), provider, &model) {
        println!("(could not persist switch to config.toml: {error})");
        return;
    }
    // A project-layer config wins per field over the user layer, so a
    // `[provider]` table there would silently override what was just written —
    // say so instead of letting the next launch surprise the user.
    let project = cwd_dir().join(".heartflow").join("config.toml");
    let overrides = fs::read_to_string(project)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .is_some_and(|table| table.contains_key("provider"));
    if overrides {
        println!("(note: .heartflow/config.toml [provider] in this directory overrides the persisted switch)");
    }
}

/// `/model` with no argument: report the `provider/protocol/base_url` that is
/// *actually* active (so a transport failure is never misread as the wrong
/// vendor), the current model, and the catalog's known models for that
/// provider. Replaces the old hardcoded `DeepSeek` shortlist: the list now tracks
/// the resolved transport and grows from `hf models` discovery and hand-edits.
fn print_model_status(selection: &ProviderSelection, catalog: &ProviderCatalog) {
    match selection {
        ProviderSelection::Env { model } => {
            println!("provider: anthropic (env)");
            println!("protocol: {}", ProviderProtocol::Anthropic.as_str());
            println!("model: {model}");
            println!("switch with: /model NAME");
        }
        ProviderSelection::Profile(profile) => {
            let key = catalog
                .provider_key_by_host(&profile.base_url)
                .unwrap_or_else(|| "custom".to_string());
            println!("provider: {key}");
            println!("protocol: {}", profile.protocol.as_str());
            println!("base_url: {}", profile.base_url);
            println!("model: {}", profile.model);
            // §12 P-A: the credential source is part of the identity — show
            // it masked so a config-layer endpoint paired with a builtin-layer
            // key env (the observed 401 cross-wiring) is visible at a glance.
            println!("key from: {}", profile.credential_source.describe());
            match catalog.provider_by_host(&profile.base_url) {
                Some(entry) if !entry.models.is_empty() => {
                    let names: Vec<&str> = entry
                        .models
                        .iter()
                        .map(|model| model.id.as_str())
                        .collect();
                    println!("known models: {}", names.join(", "));
                }
                _ => println!(
                    "known models: none cached yet (run `hf models` to discover, or add them to provider.toml)"
                ),
            }
            println!("switch with: /model NAME");
        }
    }
}

/// Handle `/plan [goal|approve|end|status]`. Planning swaps the live runtime
/// to a hard-gated policy (only the plan document is writable) instead of
/// rebuilding it, so the todo ledger and conversation survive the transition
/// into execution.
async fn handle_plan_command(
    state: &SessionShared,
    input: &str,
    plan: &mut ReplPlanState,
    cwd: &Path,
    mode: &str,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
) -> io::Result<()> {
    let arg = input
        .strip_prefix("/plan")
        .map(str::trim)
        .unwrap_or_default();
    match arg {
        "" => {
            println!("planning is {}", if plan.planning { "ON" } else { "off" });
            println!("  /plan <goal>    research and draft a plan (writes gated to the plan file)");
            println!("  /plan approve   load the approved plan into the task list and execute it");
            println!("  /plan end       leave planning without executing");
        }
        "status" => {
            println!("planning is {}", if plan.planning { "ON" } else { "off" });
            if let Some(path) = &plan.plan_path {
                println!("plan file: {}", path.display());
                match fs::read_to_string(path) {
                    Ok(markdown) => {
                        let tasks = parse_plan_tasks(&markdown);
                        let pending = tasks
                            .iter()
                            .filter(|(_, status)| *status == "pending")
                            .count();
                        println!("tasks: {} total, {pending} pending", tasks.len());
                    }
                    Err(_) => println!("plan file not written yet."),
                }
            }
        }
        "end" => end_planning(plan, mode, runtime),
        "approve" => approve_plan(state, plan, mode, runtime, prompter).await?,
        goal => start_planning(state, goal, plan, cwd, runtime).await?,
    }
    Ok(())
}

/// Begin a planning session: reserve the plan document, engage the gated
/// policy, and run the first research-and-plan turn under `BlockPrompter`.
async fn start_planning(
    state: &SessionShared,
    goal: &str,
    plan: &mut ReplPlanState,
    cwd: &Path,
    runtime: &mut AgentRuntime,
) -> io::Result<()> {
    let path = match plan_file_path(cwd, goal) {
        Ok(path) => path,
        Err(error) => {
            println!("cannot start planning: {error}");
            return Ok(());
        }
    };
    plan.planning = true;
    plan.plan_path = Some(path.clone());
    set_runtime_mode_policy(runtime, "plan");
    println!("planning mode ON - writes are blocked except the plan document.");
    println!("plan file: {}", path.display());
    run_turn_interactive(
        state,
        runtime,
        &plan_brief(goal, &path),
        Some(&mut BlockPrompter),
    )
    .await?;
    println!();
    println!("Refine by typing notes (still planning), then `/plan approve` to execute or `/plan end` to stop.");
    Ok(())
}

/// Leave planning and restore the session's normal permission policy in place.
fn end_planning(plan: &mut ReplPlanState, mode: &str, runtime: &mut AgentRuntime) {
    if !plan.planning {
        println!("not planning.");
        return;
    }
    set_runtime_mode_policy(runtime, mode);
    plan.planning = false;
    println!("planning mode off - back to {mode}.");
}

/// Approve the plan: parse its checkboxes, seed the todo ledger deterministically,
/// restore the execution policy, and run the first execution turn.
async fn approve_plan(
    state: &SessionShared,
    plan: &mut ReplPlanState,
    mode: &str,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
) -> io::Result<()> {
    if !plan.planning {
        println!("not planning; start with `/plan <goal>`.");
        return Ok(());
    }
    let Some(path) = plan.plan_path.clone() else {
        println!("no plan file to approve.");
        plan.planning = false;
        return Ok(());
    };
    let markdown = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            println!("cannot read plan {}: {error}", path.display());
            return Ok(());
        }
    };
    let tasks = parse_plan_tasks(&markdown);
    if tasks.is_empty() {
        println!(
            "no `- [ ]` tasks found in {} - add checkbox tasks before approving.",
            path.display()
        );
        return Ok(());
    }
    let pending = tasks
        .iter()
        .filter(|(_, status)| *status == "pending")
        .count();
    let approved = Confirm::new(&format!(
        "Execute the plan: {} task(s), {pending} pending?",
        tasks.len()
    ))
    .with_default(true)
    .prompt()
    .unwrap_or(false);
    if !approved {
        println!("approval cancelled - still planning.");
        return Ok(());
    }
    if let Err(error) = runtime.seed_plan(&plan_seed_json(&tasks)) {
        println!("failed to load tasks into the ledger: {error}");
        return Ok(());
    }
    set_runtime_mode_policy(runtime, mode);
    plan.planning = false;
    println!(
        "plan approved - {} tasks, execution mode {mode}. Each task runs in a fresh context.",
        tasks.len()
    );
    let mut memory: Vec<String> = Vec::new();
    let mut escalation = InteractiveEscalation;
    let status = run_task_loop(
        state,
        runtime,
        &path,
        &tasks,
        prompter,
        &mut escalation,
        &mut memory,
    )
    .await?;
    if status == TaskLoopStatus::Aborted {
        println!("task loop stopped early.");
    }

    // Reflect: persist a durable recap of the run, then optionally distill a
    // reusable skill. The plan file holds the authoritative final checkboxes.
    let cwd = env::current_dir()?;
    let final_markdown = fs::read_to_string(&path).unwrap_or_else(|_| markdown.clone());
    let fallback = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("plan")
        .to_string();
    let goal = extract_plan_goal(&final_markdown, &fallback);
    let final_tasks = parse_plan_tasks(&final_markdown);
    let usage = runtime.usage().cumulative_usage();
    match write_reflection(&cwd, &path, &final_tasks, &memory, &usage, &goal) {
        Ok(reflection_path) => {
            println!("reflection saved to {}", reflection_path.display());
            // Durable, cross-session memory: auto-record hard pitfalls (tasks
            // that failed every attempt) so later sessions dodge the same trap.
            record_pitfalls(&home_dir(), &memory);
            let has_experience = memory.iter().any(|note| !note.contains("SKIPPED"));
            let mut confirm = || {
                Confirm::new("Distill a reusable skill from this run into .agent/skills?")
                    .with_default(false)
                    .prompt()
                    .unwrap_or(false)
            };
            match maybe_sink_skill(&cwd, &goal, &reflection_path, has_experience, &mut confirm) {
                Ok(Some(skill_path)) => println!("skill written to {}", skill_path.display()),
                Ok(None) => {}
                Err(error) => println!("(note) could not write a skill: {error}"),
            }
        }
        Err(error) => println!("(note) could not write the reflection: {error}"),
    }
    Ok(())
}

/// Handle `/spec [goal|approve|end|status|list]`. The spec-mode mirror of
/// `/plan`: reserve a spec path, run the research-first turn, then present the
/// three-way confirmation overlay (execute / cancel / revise). Execution reuses
/// the plan task loop verbatim.
async fn handle_spec_command(
    state: &SessionShared,
    input: &str,
    plan: &mut ReplPlanState,
    cwd: &Path,
    mode: &str,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
) -> io::Result<()> {
    let arg = input
        .strip_prefix("/spec")
        .map(str::trim)
        .unwrap_or_default();
    match arg {
        "" => {
            println!(
                "spec-writing is {}",
                if plan.speccing { "ON" } else { "off" }
            );
            println!(
                "  /spec <goal>       research and draft a spec (writes gated to the spec file)"
            );
            println!("  /spec approve      review the spec and choose execute/cancel/revise");
            println!("  /spec list         list saved specs under .heartflow/specs/");
            println!("  /spec end          leave spec-writing without executing");
        }
        "status" => {
            println!(
                "spec-writing is {}",
                if plan.speccing { "ON" } else { "off" }
            );
            if let Some(path) = &plan.spec_path {
                println!("spec file: {}", path.display());
                match fs::read_to_string(path) {
                    Ok(markdown) => {
                        let tasks = parse_plan_tasks(&markdown);
                        let pending = tasks
                            .iter()
                            .filter(|(_, status)| *status == "pending")
                            .count();
                        println!("tasks: {} total, {pending} pending", tasks.len());
                    }
                    Err(_) => println!("spec file not written yet."),
                }
            }
        }
        "list" => match list_specs(cwd) {
            Ok(specs) if specs.is_empty() => println!("no specs under .heartflow/specs/ yet."),
            Ok(specs) => {
                println!("specs under .heartflow/specs/ (newest first):");
                for (index, path) in specs.iter().enumerate() {
                    println!("  [{index}] {}", path.display());
                }
                println!("review one with `/spec approve <path>`.");
            }
            Err(error) => println!("cannot list specs: {error}"),
        },
        "end" => end_spec(plan, mode, runtime),
        "approve" => {
            let Some(path) = plan.spec_path.clone() else {
                println!("no spec to approve; `/spec <goal>` or `/spec approve <path>`.");
                return Ok(());
            };
            spec_review_loop(state, runtime, prompter, &path, plan, mode).await?;
        }
        _ if arg.starts_with("approve ") => {
            let target = arg.strip_prefix("approve ").unwrap_or_default().trim();
            let path = match resolve_spec_path(cwd, target) {
                Ok(path) => path,
                Err(error) => {
                    println!("{error}");
                    return Ok(());
                }
            };
            spec_review_loop(state, runtime, prompter, &path, plan, mode).await?;
        }
        goal => start_spec(state, goal, plan, cwd, mode, runtime, prompter).await?,
    }
    Ok(())
}

/// Begin a spec-writing session: reserve the spec document, engage the gated
/// policy, run the first research-and-design turn under `SpecPrompter`, then
/// present the confirmation overlay.
async fn start_spec(
    state: &SessionShared,
    goal: &str,
    plan: &mut ReplPlanState,
    cwd: &Path,
    mode: &str,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
) -> io::Result<()> {
    let path = match spec_file_path(cwd, goal) {
        Ok(path) => path,
        Err(error) => {
            println!("cannot start spec: {error}");
            return Ok(());
        }
    };
    plan.speccing = true;
    plan.spec_path = Some(path.clone());
    set_runtime_mode_policy(runtime, "spec");
    println!("spec mode ON - writes are blocked except the spec document.");
    println!("spec file: {}", path.display());
    run_turn_interactive(
        state,
        runtime,
        &spec_brief(goal, &path),
        Some(&mut SpecPrompter),
    )
    .await?;
    println!();
    spec_review_loop(state, runtime, prompter, &path, plan, mode).await?;
    Ok(())
}

/// Leave spec-writing and restore the session's normal permission policy.
fn end_spec(plan: &mut ReplPlanState, mode: &str, runtime: &mut AgentRuntime) {
    if !plan.speccing {
        println!("not spec-writing.");
        return;
    }
    set_runtime_mode_policy(runtime, mode);
    plan.speccing = false;
    plan.spec_path = None;
    println!("spec mode off - back to {mode}.");
}

/// The confirmation overlay loop: show the full spec, then act on the
/// three-way decision. `Revise` runs a revision turn and re-reviews; `Execute`
/// and `Cancel` exit the loop.
async fn spec_review_loop(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
    path: &Path,
    plan: &mut ReplPlanState,
    mode: &str,
) -> io::Result<()> {
    loop {
        let markdown = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                println!("cannot read spec {}: {error}", path.display());
                return Ok(());
            }
        };
        if markdown.trim().is_empty() {
            println!(
                "spec {} is empty - refine it by typing notes (still spec-writing), then `/spec approve` to review.",
                path.display()
            );
            return Ok(());
        }
        match spec_confirmation(path, &markdown) {
            SpecDecision::Execute => {
                execute_spec(state, runtime, prompter, path, &markdown, plan, mode).await?;
                return Ok(());
            }
            SpecDecision::Cancel => {
                // Restore the session policy only if this was an active
                // spec-writing session; a later `/spec approve <path>` review
                // never changed the policy in the first place.
                if plan.speccing {
                    end_spec(plan, mode, runtime);
                }
                println!("spec kept at {}; execution deferred.", path.display());
                return Ok(());
            }
            SpecDecision::Revise(notes) => {
                run_turn_interactive(
                    state,
                    runtime,
                    &spec_revision_brief(&notes, path),
                    Some(&mut SpecPrompter),
                )
                .await?;
                println!();
            }
        }
    }
}

/// Execute a spec: parse its checkbox tasks, seed the todo ledger, restore the
/// execution policy, and run the plan task loop. Mirrors `approve_plan`.
async fn execute_spec(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
    path: &Path,
    markdown: &str,
    plan: &mut ReplPlanState,
    mode: &str,
) -> io::Result<()> {
    let tasks = parse_plan_tasks(markdown);
    if tasks.is_empty() {
        println!(
            "no `- [ ]` tasks found in {} - add checkbox tasks before executing.",
            path.display()
        );
        return Ok(());
    }
    let pending = tasks
        .iter()
        .filter(|(_, status)| *status == "pending")
        .count();
    if let Err(error) = runtime.seed_plan(&plan_seed_json(&tasks)) {
        println!("failed to load tasks into the ledger: {error}");
        return Ok(());
    }
    set_runtime_mode_policy(runtime, mode);
    plan.speccing = false;
    plan.spec_path = None;
    println!(
        "spec approved - {} tasks ({pending} pending), execution mode {mode}. Each task runs in a fresh context.",
        tasks.len()
    );
    let mut memory: Vec<String> = Vec::new();
    let mut escalation = InteractiveEscalation;
    let status = run_task_loop(
        state,
        runtime,
        path,
        &tasks,
        prompter,
        &mut escalation,
        &mut memory,
    )
    .await?;
    if status == TaskLoopStatus::Aborted {
        println!("task loop stopped early.");
    }

    // Reflect: same tail as `approve_plan` — durable recap + optional skill.
    let cwd = env::current_dir()?;
    let final_markdown = fs::read_to_string(path).unwrap_or_else(|_| markdown.to_string());
    let fallback = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("spec")
        .to_string();
    let goal = extract_plan_goal(&final_markdown, &fallback);
    let final_tasks = parse_plan_tasks(&final_markdown);
    let usage = runtime.usage().cumulative_usage();
    match write_reflection(&cwd, path, &final_tasks, &memory, &usage, &goal) {
        Ok(reflection_path) => {
            println!("reflection saved to {}", reflection_path.display());
            record_pitfalls(&home_dir(), &memory);
            let has_experience = memory.iter().any(|note| !note.contains("SKIPPED"));
            let mut confirm = || {
                Confirm::new("Distill a reusable skill from this run into .agent/skills?")
                    .with_default(false)
                    .prompt()
                    .unwrap_or(false)
            };
            match maybe_sink_skill(&cwd, &goal, &reflection_path, has_experience, &mut confirm) {
                Ok(Some(skill_path)) => println!("skill written to {}", skill_path.display()),
                Ok(None) => {}
                Err(error) => println!("(note) could not write a skill: {error}"),
            }
        }
        Err(error) => println!("(note) could not write the reflection: {error}"),
    }
    Ok(())
}

/// Resolve a `/spec approve <target>` argument to an existing spec file: an
/// existing path as written, else a filename under `.heartflow/specs/`.
fn resolve_spec_path(cwd: &Path, target: &str) -> Result<PathBuf, String> {
    let direct = Path::new(target);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let in_specs = cwd.join(".heartflow").join("specs").join(target);
    if in_specs.is_file() {
        return Ok(in_specs);
    }
    Err(format!(
        "no spec found at `{target}` (checked the path and .heartflow/specs/)"
    ))
}

#[cfg(test)]
mod repl_help_sync_tests {
    use std::collections::BTreeSet;

    use super::REPL_HELP;
    use crate::editor::COMMANDS;

    /// The command head of a help left-column entry: its first whitespace
    /// token (`/model [NAME]` -> `/model`). `!<CMD>` is shell passthrough, not
    /// a slash command, and stays out of the comparison.
    fn help_head(left: &str) -> Option<&str> {
        let head = left.split_whitespace().next()?;
        (head != "!<CMD>").then_some(head)
    }

    /// The command head of a completion entry, mirroring `suggest_command`
    /// (`/provider [NAME] [MODEL]` -> `/provider`).
    fn completion_head(entry: &str) -> &str {
        entry.split([' ', '<']).next().unwrap_or(entry)
    }

    #[test]
    fn repl_help_and_completion_commands_stay_in_sync() {
        let help: BTreeSet<&str> = REPL_HELP
            .iter()
            .filter_map(|(left, _, _)| help_head(left))
            .collect();
        let completions: BTreeSet<&str> = COMMANDS
            .iter()
            .map(|(name, _)| completion_head(name))
            .collect();
        assert_eq!(
            help.symmetric_difference(&completions).collect::<Vec<_>>(),
            Vec::<&&str>::new(),
            "REPL_HELP and editor::COMMANDS disagree; a command must enter both or neither"
        );
        // Duplicates would render twice in the dropdown: guard the heads too.
        let mut heads: Vec<&str> = COMMANDS
            .iter()
            .map(|(name, _)| completion_head(name))
            .collect();
        heads.sort_unstable();
        let unique = heads.len();
        heads.dedup();
        assert_eq!(heads.len(), unique, "editor::COMMANDS has duplicate heads");
    }
}
