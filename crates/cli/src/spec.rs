//! Spec mode confirmation flow (cli-thinning): the spec-mode mirror of the
//! plan flow's seed mechanism. The CLI reserves the exact spec path (so the
//! model writes where we can find it), kicks off the research-first turn, then
//! presents the three-way confirmation overlay — execute / cancel / revise.
//!
//! Task-list parsing, ledger seeding and the execution loop are all reused
//! verbatim from `plan.rs` (`parse_plan_tasks`, `plan_seed_json`,
//! `run_task_loop`), which is why this module holds only the spec-specific
//! surface: the `.heartflow/specs/` path reservation, the research-first brief,
//! and the overlay decision.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use inquire::{Select, Text};

use crate::unix_secs;

/// Create `.heartflow/specs/` under `cwd` and reserve a unique spec path for
/// `goal`. Mirrors `plan_file_path` — the CLI owns the exact path so the model
/// writes where the confirmation flow can find it.
pub(crate) fn spec_file_path(cwd: &Path, goal: &str) -> Result<PathBuf, String> {
    let dir = cwd.join(".heartflow").join("specs");
    fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let stamp = unix_secs();
    let slug: String = goal
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(24)
        .collect();
    let slug = if slug.is_empty() { "spec" } else { &slug };
    Ok(dir.join(format!("{stamp}-{slug}.md")))
}

/// The kickoff message for a spec-writing turn: research-first, then one
/// self-consistent minimal design spec plus a checkbox task list, and no
/// implementation. Mirrors `plan_brief`.
pub(crate) fn spec_brief(goal: &str, spec_path: &Path) -> String {
    format!(
        "SPEC MODE: research and design, make no implementation changes yet.\n\
         Objective: {goal}\n\
         Research the topic first (web search / fetch / reading the repository), then write ONE self-consistent, minimal design spec to exactly this file: {path}\n\
         The spec must contain a `## Tasks` section whose items are markdown checkboxes, one per task, using the literal form `- [ ] description` (mark done items `- [x]`). These checkboxes become the tracked task list verbatim, so keep each one a concrete, verifiable step.\n\
         Also include `## Goal` and `## Verification` (how success is proven). Do NOT implement anything - the spec is approved by the operator before any code changes.",
        path = spec_path.display()
    )
}

/// The revision nudge for a `Revise` decision: the operator's notes are handed
/// back verbatim so the model folds them into the existing spec.
pub(crate) fn spec_revision_brief(notes: &str, spec_path: &Path) -> String {
    format!(
        "The operator reviewed your spec at {path} and added these notes. Revise the spec to address them, then re-present the updated spec. Stay in spec mode: still do NOT implement anything.\n\nOperator notes:\n{notes}",
        path = spec_path.display()
    )
}

/// The operator's decision after reviewing the spec overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpecDecision {
    /// Load the task list and start implementing.
    Execute,
    /// Keep the spec on disk, do not execute.
    Cancel,
    /// Fold operator notes into the spec and re-review.
    Revise(String),
}

/// Present the spec overlay — the full document preview plus the three-way
/// choice. Returns the decision; a terminal cancel (Esc) is treated as
/// `Cancel` (safe: the spec stays on disk).
pub(crate) fn spec_confirmation(spec_path: &Path, markdown: &str) -> SpecDecision {
    let rule = "\u{2500}".repeat(72);
    println!("\n{rule}");
    println!(" SPEC REVIEW: {}", spec_path.display());
    println!("{rule}");
    // The overlay is the operator's window into the document the model wrote;
    // print it whole so the decision is informed by the actual content, not a
    // summary.
    println!("{markdown}");
    println!("{rule}");
    let choices = vec![
        "Execute now (load the task list and implement)".to_string(),
        "Cancel (keep the spec, do not execute)".to_string(),
        "Add revisions (type notes, then re-review)".to_string(),
    ];
    match Select::new("Spec review", choices).prompt() {
        Ok(choice) if choice.starts_with("Execute") => SpecDecision::Execute,
        Ok(choice) if choice.starts_with("Add revisions") => {
            match Text::new("Revisions (or notes for the agent)").prompt() {
                Ok(text) if !text.trim().is_empty() => {
                    SpecDecision::Revise(text.trim().to_string())
                }
                // Empty notes fall back to cancel: a blank revision would
                // re-review an unchanged document.
                _ => SpecDecision::Cancel,
            }
        }
        // Cancel, Esc, or any unknown pick stops safely with the spec kept.
        _ => SpecDecision::Cancel,
    }
}

/// List spec documents under `.heartflow/specs/`, newest first (the timestamp
/// prefix in the filename sorts chronologically).
pub(crate) fn list_specs(cwd: &Path) -> io::Result<Vec<PathBuf>> {
    let dir = cwd.join(".heartflow").join("specs");
    let mut specs: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect();
    specs.sort();
    specs.reverse();
    Ok(specs)
}
