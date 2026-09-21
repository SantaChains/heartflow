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
