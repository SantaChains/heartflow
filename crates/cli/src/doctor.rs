//! `hf doctor` diagnostics (cli-thinning): config/directory/provider/database
//! health checks, the built-in common-issues FAQ, and the optional `--ai`
//! repair-advice turn. Extracted verbatim from `main.rs` and re-exported
//! crate-wide so the `run` dispatch site and `super::` test paths keep
//! resolving. Assembly helpers it needs (`resolve_selection`, `build_runtime`,
//! `backup_existing`) stay in `main.rs` and are reached through the crate root.

use std::env;
use std::fs;
use std::io;
use std::path::Path;

use provider::config_file_paths;
use runtime::Session;
use store::{Integrity, Store};

use crate::config::load_provider_selection;
use crate::{
    backup_existing, build_runtime, home_dir, resolve_selection, run_turn_interactive,
    sessions_dir, store_path, SessionShared,
};

/// Diagnose config, directories, and provider resolution. `--fix` applies safe
/// repairs: create missing directories and move an unparseable config aside
/// (backed up, never deleted). Returns the human-readable list of problems found
/// so `--ai` can hand them to the built-in assistant for repair advice.
pub(crate) fn run_doctor(fix: bool) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let home = home_dir();
    let mut problems: Vec<String> = Vec::new();
    println!("heartflow doctor{}", if fix { " (fix mode)" } else { "" });

    // Every config layer must be absent or valid TOML.
    for path in config_file_paths(&cwd, &home) {
        match fs::read_to_string(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                println!("  ok   {} (absent)", path.display());
            }
            Err(error) => {
                let message = format!("{} is unreadable: {error}", path.display());
                println!("  fail {message}");
                problems.push(message);
            }
            Ok(contents) => match toml::from_str::<toml::Table>(&contents) {
                Ok(_) => println!("  ok   {}", path.display()),
                Err(error) => {
                    let message = format!("{} is not valid TOML: {error}", path.display());
                    println!("  fail {message}");
                    problems.push(message);
                    if fix {
                        match backup_existing(&path) {
                            Ok(()) => println!("       moved it aside as a .bak backup"),
                            Err(error) => println!("       could not back it up: {error}"),
                        }
                    }
                }
            },
        }
    }

    // Working directories must exist and be usable.
    for dir in [home.join(".heartflow"), sessions_dir()] {
        match ensure_dir(&dir, fix) {
            DirState::Ok => println!("  ok   {} (directory)", dir.display()),
            DirState::Created => println!("  fix  created {}", dir.display()),
            DirState::Missing => {
                let message = format!("{} is missing", dir.display());
                println!("  fail {message}");
                problems.push(message);
            }
            DirState::NotDirectory => {
                let message = format!("{} exists but is not a directory", dir.display());
                println!("  fail {message}");
                problems.push(message);
            }
        }
    }

    // Resolve the provider end-to-end (surfaces a missing api-key env var, etc.).
    match load_provider_selection(&cwd, &home, None, None) {
        Ok(selection) => println!("  ok   provider resolves (model={})", selection.model()),
        Err(error) => {
            let message = format!("provider: {error}");
            println!("  fail {message}");
            problems.push(message);
        }
    }

    // The effective transport proxy: source, mode, and probe verdict. A dead
    // proxy is a warning, not a problem — it self-heals to direct.
    let resolution = proxy::resolution();
    if resolution.is_dead() {
        println!("  warn proxy: {} (routing direct)", resolution.describe());
    } else {
        println!("  ok   proxy: {}", resolution.describe());
    }
    for warning in resolution.warnings() {
        println!("       proxy warning: {warning}");
    }

    // Phase 0 compaction observation: durable counters written by every
    // process that compacted or repaired an orphan. `repair_dropped > 0` is
    // the evidence gate for the Phase 1 compaction rework (see roadmap §4).
    let compaction = runtime::compaction_stats();
    if compaction.count == 0 && compaction.repair_dropped == 0 {
        println!("  ok   compaction: no events recorded yet");
    } else {
        println!(
            "  ok   compaction: {} event(s) (window-half {}, absolute {}), {} message(s) summarized, {} orphan tool_result(s) repaired",
            compaction.count,
            compaction.window_half,
            compaction.absolute,
            compaction.removed_total,
            compaction.repair_dropped
        );
        if compaction.repair_dropped > 0 {
            println!(
                "       orphan repairs observed: compaction cut alignment or a resumed session lost pairing (roadmap §4 evidence gate)"
            );
        }
    }

    // §6 checkpoints: a snapshot failure means a write-class tool mutated the
    // project without a restore point. Degrade-but-count policy, surfaced here
    // so a persistently broken git setup is visible instead of silent.
    let failures = runtime::snapshot_failures();
    if failures > 0 {
        println!(
            "  warn checkpoints: {failures} snapshot(s) failed this process - writes proceeded without a restore point (is git on PATH?)"
        );
        problems.push(format!(
            "{failures} checkpoint snapshot(s) failed; write tools had no restore point"
        ));
    } else {
        println!("  ok   checkpoints: no snapshot failures");
    }

    // §6 post-edit diagnostics: degradation ledger for skipped cargo checks.
    // no-manifest is normal for non-Rust projects (display, not a problem);
    // timeout/spawn errors mean a Rust project lost its check and are warned.
    let ledger = runtime::diagnostics_ledger();
    if ledger.no_manifest > 0 || ledger.no_cargo > 0 {
        println!(
            "  info diagnostics: {} skip(s) with no manifest, {} with no cargo binary - expected outside Rust projects",
            ledger.no_manifest, ledger.no_cargo
        );
    }
    if ledger.timeout > 0 || ledger.spawn_error > 0 {
        let detail = runtime::diagnostics_degradation_detail();
        println!(
            "  warn diagnostics: {} timed-out and {} failed-to-spawn cargo check(s){}",
            ledger.timeout,
            ledger.spawn_error,
            detail
                .as_deref()
                .map(|d| format!("; last: {d}"))
                .unwrap_or_default()
        );
        problems.push(
            "post-edit cargo checks degraded (timeout or spawn failure); Rust edits may lack compiler verdicts"
                .to_string(),
        );
    } else {
        println!("  ok   diagnostics: post-edit cargo checks healthy");
    }

    // §1 subagent jobs: the registry itself never blocks on disk, so a broken
    // jobs root shows up only as a counted degradation (the in-memory record
    // stays the source of truth). Any nonzero count is worth surfacing.
    let job_failures = runtime::disk_write_failures();
    if job_failures > 0 {
        println!(
            "  warn jobs: {job_failures} job record/event write(s) failed this process - /jobs still works from memory, disk records may be stale"
        );
    } else {
        println!("  ok   jobs: job records persisting");
    }

    // §1 mission ledger + schedule store: disk-level reads (doctor runs in its
    // own process, so process-local counters are meaningless here). A blocked
    // mission or an unreadable state file is exactly the thing an operator
    // wants surfaced before starting a session.
    let missions_summary = runtime::summarize_on_disk(&crate::storage::sessions_dir().join("missions"));
    if missions_summary.corrupted > 0 {
        problems.push(format!(
            "{} mission state file(s) unreadable - /mission will skip them; check ~/.heartflow/sessions/missions",
            missions_summary.corrupted
        ));
    }
    println!(
        "  ok   missions: {} active, {} paused, {} blocked, {} complete{}",
        missions_summary.active,
        missions_summary.paused,
        missions_summary.blocked,
        missions_summary.complete,
        if missions_summary.budget_exhausted > 0 {
            format!(", {} over budget (open, reminders paused)", missions_summary.budget_exhausted)
        } else {
            String::new()
        }
    );
    let schedule_store = runtime::ScheduleStore::load(&crate::storage::sessions_dir().join("schedules"));
    if schedule_store.corrupted_on_load() {
        problems.push(String::from(
            "schedules.json is unreadable - schedules start empty; salvage or delete ~/.heartflow/sessions/schedules/schedules.json",
        ));
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_u64, |d| d.as_millis() as u64);
        let all = schedule_store.list();
        let due = all.iter().filter(|s| s.is_due(now)).count();
        let paused = all.iter().filter(|s| !s.enabled).count();
        println!(
            "  ok   schedules: {} total, {due} due, {paused} paused (run-due drains them; no daemon)",
            all.len()
        );
    }

    // The derived history database, if present, must be structurally sound.
    check_history_store(&mut problems);

    let count = problems.len();
    if count == 0 {
        println!("no problems found");
    } else if fix {
        println!("{count} problem(s) reported; verify the repairs above");
    } else {
        println!("{count} problem(s): rerun with --fix to apply safe repairs");
    }
    print_common_issues();
    Ok(problems)
}

/// Above this size `hf doctor` falls back to `SQLite`'s cheaper `quick_check` so
/// the diagnostic stays prompt; below it the full `integrity_check` runs because
/// the scan is cheap and maximally thorough (index-vs-row cross-checks included).
const QUICK_CHECK_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// Verify the derived history database is not corrupted. Read-only: it never
/// mutates data. The JSON transcripts stay authoritative, so a corrupt database
/// is reported with the rebuild path rather than silently patched.
fn check_history_store(problems: &mut Vec<String>) {
    let path = store_path();
    if !path.exists() {
        println!("  ok   history database absent (created on first save)");
        return;
    }
    let size = fs::metadata(&path).map_or(0, |meta| meta.len());
    let quick = size > QUICK_CHECK_THRESHOLD_BYTES;
    let mode = if quick {
        "quick_check"
    } else {
        "integrity_check"
    };
    match Store::open_read_only(&path).and_then(|store| {
        if quick {
            store.quick_check()
        } else {
            store.integrity_check()
        }
    }) {
        Ok(Integrity::Ok) => {
            println!("  ok   history database is sound ({mode}, {size} bytes)");
        }
        Ok(Integrity::Corrupt {
            problems: found,
            truncated,
        }) => {
            let suffix = if truncated {
                ", further errors suppressed"
            } else {
                ""
            };
            let first = found.first().map_or("unknown", String::as_str);
            let message = format!(
                "history database is corrupt: {} issue(s){suffix}; first: {first}",
                found.len()
            );
            println!("  fail {message}");
            println!("       JSON transcripts under ~/.heartflow/sessions stay authoritative;");
            println!(
                "       delete {} to rebuild the search index from future saves",
                path.display()
            );
            problems.push(message);
        }
        Err(error) => {
            let message = format!("history database could not be checked: {error}");
            println!("  fail {message}");
            problems.push(message);
        }
    }
}

/// Built-in "常见问题 + 处理清单". These mirror the real failure modes this CLI
/// can hit (provider key/base URL, shell resolution, config precedence, encoding,
/// session dir), so users can self-remediate issues doctor cannot fix automatically.
fn print_common_issues() {
    const FAQ: [(&str, &str, &str); 7] = [
        (
            "provider: api key missing",
            "the key env var named by the provider config is unset",
            "set that env var (or point config to a key already present); rerun doctor",
        ),
        (
            "request fails / wrong endpoint",
            "base_url is missing the version segment (needs /v1) or points at the wrong host",
            "config: provider base_url must include the version path, e.g. https://api.deepseek.com/v1",
        ),
        (
            "bash tool cannot run a shell",
            "neither pwsh, powershell nor sh is on PATH for the current mode",
            "install PowerShell 7 (pwsh) or ensure sh is available on PATH",
        ),
        (
            "config edits seem ignored",
            "a higher-precedence layer overrides the file you changed",
            "project .heartflow/ wins over user ~/.heartflow/; edit the layer that wins",
        ),
        (
            "garbled / non-UTF-8 output",
            "legacy console code page or a stray BOM in a saved file",
            "use a UTF-8 terminal; reading already strips BOM and normalizes drive paths",
        ),
        (
            "session not saved",
            "sessions directory missing or not writable",
            "rerun `hf doctor --fix` to recreate the sessions directory",
        ),
        (
            "history database is corrupt",
            "the derived ~/.heartflow/heartflow.db was damaged (torn write, disk fault, tampering)",
            "JSON transcripts are authoritative: delete heartflow.db and it rebuilds on the next save (search covers future turns)",
        ),
    ];
    println!("\ncommon issues and fixes:");
    for (symptom, cause, fix_hint) in FAQ {
        println!("  - {symptom}");
        println!("      cause: {cause}");
        println!("      fix:   {fix_hint}");
    }
}

/// Build the repair-advice prompt handed to the built-in assistant. Empty when
/// there is nothing to diagnose.
#[must_use]
pub(crate) fn doctor_repair_prompt(problems: &[String]) -> String {
    if problems.is_empty() {
        return String::new();
    }
    let mut prompt = String::from(
        "The heartflow `doctor` diagnostics reported the problems below. As a terse support \n\
         engineer, give concrete, ordered repair steps for each (exact commands, config keys, \n\
         and environment variables). Advice only.\n\nProblems:\n",
    );
    for problem in problems {
        prompt.push_str("- ");
        prompt.push_str(problem);
        prompt.push('\n');
    }
    prompt
}

/// Hand doctor's findings to the built-in assistant for repair advice. Degrades
/// gracefully when no provider is configured (the common offline case).
pub(crate) async fn doctor_ai_repair(
    state: &SessionShared,
    problems: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    if problems.is_empty() {
        println!("\nno problems to diagnose; skipping AI repair.");
        return Ok(());
    }
    let selection = match resolve_selection(None, None) {
        Ok(selection) => selection,
        Err(error) => {
            println!("\nAI repair needs a working provider, but none resolved: {error}");
            println!("configure a provider or set its API key, then rerun `hf doctor --ai`.");
            return Ok(());
        }
    };
    // read-only + non-interactive prompter: the assistant can only advise, never write.
    let mut runtime = match build_runtime(state, Session::new(), selection, false, "read-only") {
        Ok(runtime) => runtime,
        Err(error) => {
            println!("\nAI repair could not start a session: {error}");
            return Ok(());
        }
    };
    println!(
        "\nasking the built-in assistant to analyze {} problem(s)...\n",
        problems.len()
    );
    run_turn_interactive(state, &mut runtime, &doctor_repair_prompt(problems), None).await?;
    Ok(())
}

enum DirState {
    Ok,
    Created,
    Missing,
    NotDirectory,
}

fn ensure_dir(dir: &Path, fix: bool) -> DirState {
    match dir.metadata() {
        Ok(metadata) if metadata.is_dir() => DirState::Ok,
        Ok(_) => DirState::NotDirectory,
        Err(_) => {
            if fix && fs::create_dir_all(dir).is_ok() {
                DirState::Created
            } else {
                DirState::Missing
            }
        }
    }
}
