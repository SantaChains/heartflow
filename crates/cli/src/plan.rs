//! Plan mode + Hermes task loop (cli-thinning): the plan-document helpers, the
//! fresh-context task orchestrator with its attempt ladder and operator
//! escalation, and the reflection / skill-distillation tail. Extracted verbatim
//! from `main.rs` and re-exported crate-wide so the `/plan` dispatch sites and
//! `super::` test paths keep resolving.
//!
//! The turn driver itself (`run_turn_interactive`) stays in `main.rs` because it
//! is welded to the blocking REPL's `TurnRenderer`; this module only calls it
//! through the crate root, so no rendering code moves.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use inquire::{Select, Text};
use runtime::{ConversationMessage, TokenUsage};
use tools::task_id;

use crate::{
    note_mirror_rewrite, run_turn_interactive, unix_millis, unix_secs, AgentRuntime,
    CliPermissionPrompter, SessionShared, TurnOutcome,
};

/// Create `.heartflow/plans/` under `cwd` and reserve a unique plan path for
/// `goal`. The CLI owns the exact path so the model writes where we can find it.
pub(crate) fn plan_file_path(cwd: &Path, goal: &str) -> Result<PathBuf, String> {
    let dir = cwd.join(".heartflow").join("plans");
    fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let stamp = unix_secs();
    let slug: String = goal
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(24)
        .collect();
    let slug = if slug.is_empty() { "plan" } else { &slug };
    Ok(dir.join(format!("{stamp}-{slug}.md")))
}

/// The kickoff message for a planning turn: states the goal and the single file
/// the model is allowed to write, plus the checkbox format the seeder parses.
pub(crate) fn plan_brief(goal: &str, plan_path: &Path) -> String {
    format!(
        "PLAN MODE: investigate and plan, make no real changes yet.\n\
         Objective: {goal}\n\
         Explore the repository as needed (read/search/fetch), then write ONE implementation plan to exactly this file: {path}\n\
         The plan must contain a `## Tasks` section whose items are markdown checkboxes, one per task, using the literal form `- [ ] description` (mark done items `- [x]`). These checkboxes become the tracked task list verbatim, so keep each one a concrete, verifiable step.\n\
         Also include `## Goal` and `## Verification` (how success is proven).",
        path = plan_path.display()
    )
}

/// Parse the checkbox tasks from a plan document. `- [ ]` becomes pending and
/// `- [x]`/`- [X]` completed; every other line is ignored. This is the
/// deterministic bridge from the approved plan to the todo ledger.
pub(crate) fn parse_plan_tasks(markdown: &str) -> Vec<(String, &'static str)> {
    let mut tasks = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        let after_bullet = match trimmed
            .strip_prefix('-')
            .or_else(|| trimmed.strip_prefix('*'))
        {
            Some(rest) => rest.trim_start(),
            None => continue,
        };
        let (status, content) = if let Some(content) = after_bullet.strip_prefix("[ ]") {
            ("pending", content)
        } else if let Some(content) = after_bullet
            .strip_prefix("[x]")
            .or_else(|| after_bullet.strip_prefix("[X]"))
        {
            ("completed", content)
        } else {
            continue;
        };
        let content = content.trim().to_string();
        if !content.is_empty() {
            tasks.push((content, status));
        }
    }
    tasks
}

/// Build the `todo_write`-shaped JSON used to seed the ledger from a plan.
/// Each task carries a stable positional id so the task-loop orchestrator can
/// track it across the plan document, the ledger, and its reflection record.
pub(crate) fn plan_seed_json(tasks: &[(String, &'static str)]) -> String {
    let items: Vec<serde_json::Value> = tasks
        .iter()
        .enumerate()
        .map(|(index, (content, status))| {
            serde_json::json!({
                "id": task_id(index),
                "content": content,
                "status": status,
            })
        })
        .collect();
    serde_json::json!({ "todos": items }).to_string()
}

/// The result of running the whole plan task loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskLoopStatus {
    Completed,
    Aborted,
}

/// Max automated attempts per task before the operator is asked.
const MAX_TASK_ATTEMPTS: usize = 3;

/// The escalation the operator chooses after a task exhausts its attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Escalation {
    Retry,
    Skip,
    Abort,
    Reword(String),
}

/// Decides what to do when a task fails every attempt. Kept behind a trait so
/// the loop can be driven by a stub in tests and by the terminal otherwise.
pub(crate) trait EscalationHandler {
    fn handle(&mut self, task_id: &str, task_content: &str) -> Escalation;
}

pub(crate) struct InteractiveEscalation;

impl EscalationHandler for InteractiveEscalation {
    fn handle(&mut self, id: &str, content: &str) -> Escalation {
        let choices = vec![
            "Try again (fresh attempts)".to_string(),
            "Skip this task".to_string(),
            "Reword the task".to_string(),
            "Abort the plan".to_string(),
        ];
        let prompt = format!("[{id}] {content:?} failed after {MAX_TASK_ATTEMPTS} attempts. Next?");
        match Select::new(&prompt, choices).prompt() {
            Ok(choice) if choice.starts_with("Try again") => Escalation::Retry,
            Ok(choice) if choice.starts_with("Skip") => Escalation::Skip,
            Ok(choice) if choice.starts_with("Reword") => {
                match Text::new("New task description").prompt() {
                    Ok(text) if !text.trim().is_empty() => {
                        Escalation::Reword(text.trim().to_string())
                    }
                    _ => Escalation::Skip,
                }
            }
            // Any terminal/cancel condition or unknown pick stops the loop safely.
            _ => Escalation::Abort,
        }
    }
}

/// Pre-compaction kickoff note per attempt: first try is clean, the second
/// retries verbatim, the third demands a fundamentally different approach.
#[must_use]
pub(crate) fn attempt_note(attempt: usize) -> &'static str {
    match attempt {
        0 | 1 => "",
        2 => "PREVIOUS ATTEMPT FAILED. Retry the same approach once; the failure may be transient.\n",
        _ => "ALL PRIOR ATTEMPTS FAILED. Change strategy: pursue a fundamentally different approach.\n",
    }
}

/// Seed JSON for a single focused task, keeping its stable plan id so the
/// ledger, plan checkbox, and reflection line all refer to the same task.
#[must_use]
pub(crate) fn task_seed_json(id: &str, content: &str) -> String {
    serde_json::json!({
        "todos": [{ "id": id, "content": content, "status": "in_progress" }]
    })
    .to_string()
}

/// Kickoff user message for one task on a given attempt.
#[must_use]
pub(crate) fn task_kickoff(id: &str, content: &str, attempt: usize) -> String {
    format!(
        "TASK [{id}]: {content}\n{note}Do only this task now. Your todo list holds just this one item: finish it, verify it against the plan's ## Verification, then mark it `completed` with `todo_write` - the loop cannot advance until you do. Report the concrete result.",
        note = attempt_note(attempt)
    )
}

/// Fresh-context seeds for a task: prior tasks' high-density conclusions, so
/// knowledge carries forward while the raw transcript does not.
#[must_use]
pub(crate) fn build_task_seeds(memory: &[String]) -> Vec<ConversationMessage> {
    if memory.is_empty() {
        return Vec::new();
    }
    let joined = memory.join("\n");
    vec![ConversationMessage::user_text(format!(
        "CONCLUSIONS FROM EARLIER TASKS IN THIS PLAN (already done, do not redo them):\n{joined}"
    ))]
}

/// A one-line memory entry recording a finished task's outcome.
#[must_use]
pub(crate) fn task_memory_note(id: &str, content: &str, outcome: &TurnOutcome) -> String {
    let detail = outcome.conclusion.as_deref().unwrap_or("(no text summary)");
    format!("[{id}] {content} -> {detail}")
}

/// Flip the `target_index`-th plan checkbox to checked, mirroring the
/// `parse_plan_tasks` order. Idempotent when already `[x]`.
pub(crate) fn mark_plan_task_done(path: &Path, target_index: usize) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    let mut seen = 0usize;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_checkbox = trimmed
            .strip_prefix('-')
            .or_else(|| trimmed.strip_prefix('*'))
            .is_some_and(|rest| {
                let inner = rest.trim_start();
                inner.starts_with("[ ]") || inner.starts_with("[x]") || inner.starts_with("[X]")
            });
        if is_checkbox {
            if seen == target_index {
                lines.push(line.replacen("[ ]", "[x]", 1));
            } else {
                lines.push(line.to_string());
            }
            seen += 1;
            continue;
        }
        lines.push(line.to_string());
    }
    let mut result = lines.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    fs::write(path, result)
}

/// Coordinates for one pending plan task inside the task loop.
struct TaskContext<'a> {
    plan_path: &'a Path,
    index: usize,
    id: String,
    content: String,
}

/// Hermes-style task loop: run every pending plan task in order, each from a
/// fresh context, verify it deterministically, and escalate on failure.
pub(crate) async fn run_task_loop(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    plan_path: &Path,
    tasks: &[(String, &'static str)],
    prompter: &mut CliPermissionPrompter,
    escalation: &mut dyn EscalationHandler,
    memory: &mut Vec<String>,
) -> io::Result<TaskLoopStatus> {
    for (index, (content, initial_status)) in tasks.iter().enumerate() {
        if *initial_status == "completed" {
            continue;
        }
        let task = TaskContext {
            plan_path,
            index,
            id: task_id(index),
            content: content.clone(),
        };
        if run_one_task(state, runtime, task, prompter, escalation, memory).await?
            == TaskLoopStatus::Aborted
        {
            return Ok(TaskLoopStatus::Aborted);
        }
    }
    Ok(TaskLoopStatus::Completed)
}

/// Drive one task through its attempt ladder and, on repeated failure, the
/// operator escalation. `Completed` covers both done and skipped.
async fn run_one_task(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    mut task: TaskContext<'_>,
    prompter: &mut CliPermissionPrompter,
    escalation: &mut dyn EscalationHandler,
    memory: &mut Vec<String>,
) -> io::Result<TaskLoopStatus> {
    let ledger = runtime.executor().todo_ledger();
    loop {
        for attempt in 1..=MAX_TASK_ATTEMPTS {
            println!(
                "\n=== [{}] {} (attempt {}/{}) ===",
                task.id, task.content, attempt, MAX_TASK_ATTEMPTS
            );
            if let Err(error) = runtime.seed_plan(&task_seed_json(&task.id, &task.content)) {
                println!("failed to seed task into the ledger: {error}");
                return Ok(TaskLoopStatus::Completed);
            }
            runtime.reset_for_task(build_task_seeds(memory));
            // A fresh task context replaces the transcript wholesale, so the
            // prior mirror rows are no longer a valid append base.
            note_mirror_rewrite(state);
            let outcome = run_turn_interactive(
                state,
                runtime,
                &task_kickoff(&task.id, &task.content, attempt),
                Some(prompter),
            )
            .await?;
            // Deterministic verify (no judge): turn succeeded, no tool errors,
            // and the model marked the single focused task completed.
            if outcome.ok && outcome.tool_errors == 0 && ledger.pending_tasks() == 0 {
                if let Err(error) = mark_plan_task_done(task.plan_path, task.index) {
                    println!("(note) could not update the plan checkbox: {error}");
                }
                memory.push(task_memory_note(&task.id, &task.content, &outcome));
                println!("[{}] done.", task.id);
                return Ok(TaskLoopStatus::Completed);
            }
            println!(
                "[{}] not verified (turn ok: {}, tool errors: {}, pending: {}).",
                task.id,
                outcome.ok,
                outcome.tool_errors,
                ledger.pending_tasks()
            );
        }
        // The match is the loop's tail: Retry/Reword fall through to a fresh
        // attempt ladder, Skip/Abort exit.
        match escalation.handle(&task.id, &task.content) {
            Escalation::Retry => {}
            Escalation::Skip => {
                memory.push(format!(
                    "[{}] {} -> SKIPPED after {} failed attempts",
                    task.id, task.content, MAX_TASK_ATTEMPTS
                ));
                println!("[{}] skipped.", task.id);
                return Ok(TaskLoopStatus::Completed);
            }
            Escalation::Reword(new_content) => task.content = new_content,
            Escalation::Abort => {
                println!("[{}] aborting the remaining tasks.", task.id);
                return Ok(TaskLoopStatus::Aborted);
            }
        }
    }
}

/// Directory holding plan reflection documents (sibling of the plans dir).
fn reflections_dir(cwd: &Path) -> PathBuf {
    cwd.join(".heartflow").join("reflections")
}

/// Extract the plan's `## Goal` text for the reflection header; falls back to
/// the supplied name when the plan has no goal section.
#[must_use]
pub(crate) fn extract_plan_goal(markdown: &str, fallback: &str) -> String {
    let mut in_goal = false;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("## goal") {
            in_goal = true;
            continue;
        }
        if in_goal {
            if trimmed.starts_with('#') {
                break;
            }
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    fallback.to_string()
}

/// Build the reflection document body. Pure so it can be tested without a
/// model: lists every task with its final status plus the conclusion the loop
/// captured, and the aggregated token usage.
#[must_use]
pub(crate) fn build_reflection_doc(
    stamp: &str,
    plan_path: &Path,
    goal: &str,
    tasks: &[(String, &'static str)],
    memory: &[String],
    usage: &TokenUsage,
) -> String {
    let mut lines = vec![
        String::from("# heartflow reflection"),
        String::new(),
        format!("- generated: {stamp}"),
        format!("- plan: {}", plan_path.display()),
        format!("- goal: {goal}"),
        format!(
            "- usage: in {} / out {} / cache read {} / total {}",
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_input_tokens,
            usage.total_tokens()
        ),
        String::new(),
        String::from("## Tasks"),
    ];
    for (index, (content, status)) in tasks.iter().enumerate() {
        let id = task_id(index);
        let verdict = if *status == "completed" {
            "completed"
        } else {
            "not completed"
        };
        lines.push(format!("- [{id}] {content} - {verdict}"));
        if let Some(note) = memory
            .iter()
            .find(|note| note.starts_with(&format!("[{id}] ")))
        {
            lines.push(format!("  - {note}"));
        }
    }
    let mut doc = lines.join("\n");
    doc.push('\n');
    doc
}

/// Persist a plan reflection document, creating the reflections directory.
pub(crate) fn write_reflection(
    cwd: &Path,
    plan_path: &Path,
    tasks: &[(String, &'static str)],
    memory: &[String],
    usage: &TokenUsage,
    goal: &str,
) -> io::Result<PathBuf> {
    let dir = reflections_dir(cwd);
    fs::create_dir_all(&dir)?;
    let stamp = unix_millis();
    let path = dir.join(format!("{stamp}.md"));
    let doc = build_reflection_doc(
        &format!("{stamp}ms since epoch (UTC)"),
        plan_path,
        goal,
        tasks,
        memory,
        usage,
    );
    fs::write(&path, doc)?;
    Ok(path)
}

/// Turn a plan goal into a filesystem-safe skill slug.
#[must_use]
pub(crate) fn skill_slug(goal: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in goal.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed: String = out.trim_matches('-').chars().take(40).collect();
    if trimmed.is_empty() {
        String::from("plan-reflection")
    } else {
        trimmed
    }
}

/// Optionally distill a reusable skill from a reflection. Never auto-writes:
/// it only creates `.agent/skills/<slug>/SKILL.md` when there is non-trivial
/// experience AND `confirm` returns true. `confirm` is injected so the write
/// path can be exercised without a terminal.
pub(crate) fn maybe_sink_skill(
    cwd: &Path,
    goal: &str,
    reflection: &Path,
    has_experience: bool,
    confirm: &mut dyn FnMut() -> bool,
) -> io::Result<Option<PathBuf>> {
    if !has_experience || !confirm() {
        return Ok(None);
    }
    let slug = skill_slug(goal);
    let dir = cwd.join(".agent").join("skills").join(&slug);
    fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    let safe_goal = goal.replace('"', "'");
    let body = format!(
        "---\nname: {slug}\ndescription: \"Workflow learned from the '{safe_goal}' plan\"\n---\n\n# {safe_goal}\n\nReusable workflow distilled by heartflow after completing this plan. See the full reflection at {} for the per-task record.\n",
        reflection.display()
    );
    fs::write(&path, body)?;
    Ok(Some(path))
}
