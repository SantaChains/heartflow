mod config;
mod core;
mod editor;
mod mascot;
mod render;
mod theme;
mod viewport_term;

// mimalloc serves the hot allocation streams (serde_json parsing on every
// SSE chunk, per-tool-call JSON round trips) measurably faster than the
// system allocator on Windows, with no code change beyond this line.
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use api::OpenAiClient;
use clap::{ArgAction, Args, Parser, Subcommand};
use crossterm::style::Stylize;
use inquire::{Confirm, MultiSelect, Select, Text};
use mcp::{HttpTransport, McpClient, McpTool, StdioTransport, Transport};
use runtime::{
    execute_bash, is_dangerous_command, load_system_prompt, normalize_tool_schema, redact_session,
    truncate_chars, AgentEvent, BashCommandInput, CompactionConfig, ContentBlock,
    ConversationMessage, ConversationRuntime, MessageRole, PermissionMode, PermissionPolicy,
    PermissionPromptDecision, PermissionPrompter, PermissionRequest, Session, TokenUsage,
    ToolError, ToolExecutor, ToolSpec,
};
use store::{role_str, Integrity, SearchHit, SearchMethod, SessionMeta, Store, StoreError};
use tokio_util::sync::CancellationToken;
use tools::{task_id, todo_tool_spec, TodoLedger};

use config::{load_merged_mcp, load_provider_selection, ConfigWatcher, McpServerConfig};
use core::{build_guide, guide_log_line, GuideSections, HeartModel};
use provider::{
    config_file_paths, load_merged_settings, AnthropicStreamClient, ProviderProtocol,
    ProviderSelection, ProviderSettings, TransportClient, CONFIG_VERSION,
};
use render::{ColorTheme, Spinner, TerminalRenderer};

/// Fallback date only when the system clock reads before the Unix epoch; the
/// live value comes from `current_date()` so the prompt never hardcodes a date.
const DEFAULT_DATE: &str = "2026-03-31";

/// Today's date as `YYYY-MM-DD` derived from the system clock (UTC). UTC is
/// deliberate: the CLI ships no timezone database, and a UTC date is honest and
/// identical across machines. Falls back to `DEFAULT_DATE` only for a pre-epoch
/// clock, which never happens on real systems.
fn current_date() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => format_iso_date(i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)),
        Err(_) => DEFAULT_DATE.to_string(),
    }
}

/// Render Unix seconds as a UTC `YYYY-MM-DD` calendar date.
fn format_iso_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days-since-epoch to proleptic-Gregorian `(year, month, day)` via Howard
/// Hinnant's `civil_from_days` (400-year eras), so no calendar crate is needed.
/// Integer-only and correct across the epoch boundary.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let shifted_month = (5 * day_of_year + 2) / 153; // [0, 11] (Mar = 0)
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1; // [1, 31]
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Set by `/restart` to ask `main` to re-exec this binary after the REPL exits.
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Cap on chained automatic restarts, guarding against a crash-restart loop.
const MAX_RESTART_DEPTH: usize = 5;

/// Tool results longer than this are folded in the terminal; the full text is
/// kept in `EXPANDABLE` for `/expand`. Chosen to fit a typical screen.
const FOLD_TOOL_OUTPUT_LINES: usize = 40;

/// Full text of tool outputs folded during rendering, indexed for `/expand`.
/// The REPL renders on a single thread, so a mutex is only for `'static` access.
static EXPANDABLE: Mutex<Vec<String>> = Mutex::new(Vec::new());

type AgentRuntime = ConversationRuntime<TransportClient, AgentToolExecutor>;

fn main() {
    init_logging();
    let runtime_result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    let runtime = match runtime_result {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start async runtime: {error}");
            process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(run()) {
        eprintln!("{error}");
        process::exit(1);
    }
    // `/restart` was confirmed in the REPL: re-exec a fresh process (picking up a
    // replaced binary and a full config/MCP reload) and exit with its status.
    if RESTART_REQUESTED.load(Ordering::SeqCst) {
        perform_restart();
    }
}

/// Compute the next chained-restart depth from the inherited env marker.
/// Returns `None` when the loop guard is exhausted or the value is not numeric.
fn next_restart_depth(current: Option<&str>) -> Option<usize> {
    let depth = match current {
        Some(raw) => raw.trim().parse::<usize>().ok()?,
        None => 0,
    };
    if depth >= MAX_RESTART_DEPTH {
        return None;
    }
    Some(depth + 1)
}

/// Re-exec the current executable with the original arguments, carrying a depth
/// marker so a crash-restart loop is capped. Exits the process either way.
fn perform_restart() -> ! {
    let depth_guard = next_restart_depth(env::var("HEARTFLOW_RESTART_DEPTH").ok().as_deref());
    let Some(depth) = depth_guard else {
        eprintln!(
            "restart aborted: reached the {MAX_RESTART_DEPTH}-restart limit; relaunch manually"
        );
        process::exit(1);
    };
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("restart failed: cannot locate current executable: {error}");
            process::exit(1);
        }
    };
    let args: Vec<String> = env::args().skip(1).collect();
    match process::Command::new(&exe)
        .args(&args)
        .env("HEARTFLOW_RESTART_DEPTH", depth.to_string())
        .spawn()
    {
        Ok(mut child) => match child.wait() {
            Ok(status) => process::exit(status.code().unwrap_or(0)),
            Err(error) => {
                eprintln!("restart failed waiting for child: {error}");
                process::exit(1);
            }
        },
        Err(error) => {
            eprintln!("restart failed to launch {}: {error}", exe.display());
            process::exit(1);
        }
    }
}

/// Logs go to stderr and stay silent unless `HEARTFLOW_LOG` opts in.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = env::var("HEARTFLOW_LOG").unwrap_or_else(|_| "warn".to_string());
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .try_init();
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.into_action()? {
        Action::PrintSystemPrompt { cwd, date } => print_system_prompt(cwd, date),
        Action::ResumeSession {
            session_path,
            command,
            provider,
            model,
        } => {
            let session_path = match session_path {
                Some(path) => path,
                None => pick_session()?,
            };
            match command {
                // One-shot slash command run against the saved session, then exit.
                Some(command) => resume_session(&session_path, &command),
                // No --run: reopen the interactive REPL with the conversation restored.
                None => {
                    let session = load_saved_session(&session_path)
                        .map_err(|error| format!("failed to restore session: {error}"))?;
                    // Continue this transcript in place: adopt its id so later
                    // turns overwrite the same file and update the same row.
                    adopt_session_path(&session_path);
                    let selection = resolve_selection(provider.as_deref(), model.as_deref())?;
                    println!(
                        "Restored session from {} ({} messages).",
                        session_path.display(),
                        session.messages.len()
                    );
                    run_repl(selection, session).await?;
                }
            }
        }
        Action::Prompt {
            instruction,
            provider,
            model,
            quiet,
            json,
        } => {
            let prompt = compose_prompt(&instruction, read_piped_stdin().as_deref());
            if prompt.trim().is_empty() {
                return Err("no prompt: pass text as an argument or pipe it via stdin".into());
            }
            let selection = resolve_selection(provider.as_deref(), model.as_deref())?;
            let mut runtime = build_runtime(
                Session::new(),
                selection,
                false,
                &default_permission_mode(false),
            )?;
            if quiet || json {
                let (text, usage) = run_turn_capture(&mut runtime, &prompt).await?;
                let saved = save_session(runtime.session()).ok();
                if json {
                    println!("{}", turn_json(&text, usage.as_ref(), saved.as_deref()));
                } else {
                    println!("{text}");
                }
            } else {
                run_turn_interactive(&mut runtime, &prompt, None).await?;
            }
        }
        Action::Search { query, limit, json } => run_search(&query, limit, json)?,
        Action::Repl { provider, model } => {
            let selection = match resolve_selection(provider.as_deref(), model.as_deref()) {
                Ok(selection) => selection,
                Err(error) => {
                    eprintln!("cannot start the REPL: {error}");
                    eprintln!("hint: run `hf doctor` to diagnose provider/key resolution; the API-key env var must be set in THIS terminal session (restart the shell after setting it).");
                    process::exit(1);
                }
            };
            run_repl(selection, Session::new()).await?;
        }
        Action::Config { action } => match action {
            ConfigAction::Export { output } => export_config(output)?,
            ConfigAction::Import { path } => import_config(&path)?,
        },
        Action::Doctor { fix, ai } => {
            let problems = run_doctor(fix)?;
            if ai {
                doctor_ai_repair(&problems).await?;
            }
        }
        Action::Init { force } => {
            let cwd = env::current_dir().map_err(|error| error.to_string())?;
            println!("{}", write_agents_skeleton(&cwd, force)?);
        }
        Action::Models {
            provider,
            model,
            balance,
        } => run_models(provider, model, balance).await?,
    }
    Ok(())
}

fn resolve_selection(
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<ProviderSelection, String> {
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    load_provider_selection(&cwd, &home_dir(), provider, model)
}

/// Export the merged file-level config (no CLI flags, no secret values).
fn export_config(output: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let mut settings = load_merged_settings(&cwd, &home_dir());
    settings.version = Some(CONFIG_VERSION);
    let mut text = settings.to_toml_string();
    text.push_str(&load_merged_mcp(&cwd, &home_dir()).to_toml_string());
    match output {
        Some(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, text)?;
            println!("config exported -> {}", path.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// Import a config file into the user layer with fault-tolerant normalization.
fn import_config(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let table: toml::Table = toml::from_str(&contents)
        .map_err(|error| format!("{} is not valid TOML: {error}", path.display()))?;
    let mut settings = ProviderSettings::from_table(&table, &path.display().to_string());
    settings.version = Some(CONFIG_VERSION);
    let mcp_settings = crate::config::McpSettings::from_table(&table, &path.display().to_string());

    let target = home_dir().join(".heartflow").join("config.toml");
    fs::create_dir_all(
        target
            .parent()
            .expect("user config target always has a parent"),
    )?;
    backup_existing(&target)?;
    let mut text = settings.to_toml_string();
    text.push_str(&mcp_settings.to_toml_string());
    fs::write(&target, text)?;
    println!("config imported -> {}", target.display());
    Ok(())
}

fn backup_existing(target: &Path) -> std::io::Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let name = target
        .file_name()
        .expect("config target always has a file name")
        .to_string_lossy()
        .to_string();
    let backup = target.with_file_name(format!("{name}.bak"));
    fs::rename(target, &backup)?;
    println!("previous config backed up -> {}", backup.display());
    Ok(())
}

/// Diagnose config, directories, and provider resolution. `--fix` applies safe
/// repairs: create missing directories and move an unparseable config aside
/// (backed up, never deleted). Returns the human-readable list of problems found
/// so `--ai` can hand them to the built-in assistant for repair advice.
fn run_doctor(fix: bool) -> Result<Vec<String>, Box<dyn std::error::Error>> {
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

/// Above this size `hf doctor` falls back to SQLite's cheaper `quick_check` so
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
fn doctor_repair_prompt(problems: &[String]) -> String {
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
async fn doctor_ai_repair(problems: &[String]) -> Result<(), Box<dyn std::error::Error>> {
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
    let mut runtime = match build_runtime(Session::new(), selection, false, "read-only") {
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
    run_turn_interactive(&mut runtime, &doctor_repair_prompt(problems), None).await?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigAction {
    Export { output: Option<PathBuf> },
    Import { path: PathBuf },
}

/// Normalized action produced from the parsed CLI surface.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    PrintSystemPrompt {
        cwd: PathBuf,
        date: String,
    },
    ResumeSession {
        session_path: Option<PathBuf>,
        command: Option<String>,
        provider: Option<String>,
        model: Option<String>,
    },
    Prompt {
        instruction: String,
        provider: Option<String>,
        model: Option<String>,
        quiet: bool,
        json: bool,
    },
    Search {
        query: String,
        limit: i64,
        json: bool,
    },
    Repl {
        provider: Option<String>,
        model: Option<String>,
    },
    Config {
        action: ConfigAction,
    },
    Doctor {
        fix: bool,
        ai: bool,
    },
    Init {
        force: bool,
    },
    Models {
        provider: Option<String>,
        model: Option<String>,
        balance: bool,
    },
}

#[derive(Parser, Debug)]
#[command(
    name = "hf",
    version,
    disable_version_flag = true,
    about = "heartflow terminal AI agent\n\nWith no subcommand (or `hf chat`) hf starts the interactive REPL; every subcommand is non-interactive and pipe-friendly."
)]
#[command(subcommand_precedence_over_arg = true)]
#[command(
    after_help = "INTERACTION CONTRACT\n  Interactive:  no args or `hf chat` opens the REPL (this is the only mode that can prompt for confirmation).\n  Resume:       `hf --resume[=PATH] [--run \"/cmd\"]` reopens a saved session (PATH omitted = interactive picker; value form uses `=`).\n  Non-interactive: subcommands (prompt/search/...) never block on a human. Because there is no tty to answer a confirmation, `prompt` runs tools under the permission mode from HEARTFLOW_PERMISSION_MODE, defaulting to `full` (auto-allow). Set HEARTFLOW_PERMISSION_MODE=read-only for an unattended, read-only pipe.\n\nEXIT CODES\n  0  success\n  1  runtime/provider error (stream, config resolution, failed turn)\n  2  usage error (bad arguments; emitted by the argument parser)"
)]
struct Cli {
    /// Provider name (deepseek, anthropic, or a [provider] table entry).
    #[arg(long, global = true)]
    provider: Option<String>,
    /// Model override for the selected provider.
    #[arg(long, global = true)]
    model: Option<String>,
    /// Print version information. Accepts the conventional `-V` and the
    /// shorthand `-v` (as in node/npm); the built-in flag is disabled so this
    /// one owns both spellings.
    #[arg(short = 'v', visible_short_alias = 'V', long = "version", action = ArgAction::Version)]
    version: (),
    /// Resume a saved session; `--resume` alone picks interactively, `--resume=PATH`
    /// opens a specific file. The value must use `=` so a bare `--resume` never
    /// swallows a following subcommand token.
    #[arg(long, num_args = 0..=1, require_equals = true, value_name = "PATH")]
    resume: Option<Option<PathBuf>>,
    /// Slash command to run right after resuming (requires --resume).
    #[arg(long, value_name = "CMD", requires = "resume")]
    run: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the interactive REPL (same as running `hf` with no subcommand).
    Chat,
    /// Send one prompt and stream the response. When stdin is piped it is read
    /// as extra context, so `git diff | hf prompt "review this"` composes.
    Prompt {
        /// Prompt text (instruction). If omitted, the prompt is read from stdin.
        text: Vec<String>,
        /// Print only the assistant's answer (no spinner/usage/save lines).
        #[arg(long, short)]
        quiet: bool,
        /// Emit a JSON object `{text, usage, session_id}` instead of prose.
        #[arg(long)]
        json: bool,
    },
    /// Search saved conversation history (non-interactive, pipe-friendly).
    Search {
        /// Query text.
        query: Vec<String>,
        /// Maximum number of hits to return.
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Emit a JSON array instead of one line per hit.
        #[arg(long)]
        json: bool,
    },
    /// Print the assembled system prompt.
    SystemPrompt(SystemPromptArgs),
    /// Manage configuration (export/import).
    Config(ConfigArgs),
    /// Diagnose configuration, directories, and provider resolution.
    Doctor(DoctorArgs),
    /// Scaffold a starting AGENTS.md instruction file in the current directory.
    Init(InitArgs),
    /// Self-bootstrap a provider: list models and report the context window.
    /// Pass `--balance` to also query the account balance (uses quota).
    /// Needs an explicit provider, e.g. `hf --provider deepseek models`.
    Models(ModelsArgs),
}

#[derive(Args, Debug)]
struct SystemPromptArgs {
    /// Working directory the prompt should describe.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Date to embed (YYYY-MM-DD).
    #[arg(long)]
    date: Option<String>,
}

#[derive(Args, Debug)]
struct ConfigArgs {
    #[command(subcommand)]
    action: ConfigCommand,
}

#[derive(Args, Debug)]
struct DoctorArgs {
    /// Apply safe repairs: create missing directories, move unparseable config aside.
    #[arg(long)]
    fix: bool,
    /// Ask the built-in AI to analyze any problems found and suggest repairs.
    #[arg(long)]
    ai: bool,
}

#[derive(Args, Debug)]
struct InitArgs {
    /// Overwrite an existing AGENTS.md instead of leaving it untouched.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct ModelsArgs {
    /// Query the account balance (`DeepSeek`). Off by default because the
    /// balance endpoint counts against API quota; model listing is free.
    #[arg(long)]
    balance: bool,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Export the merged config (without secrets).
    Export {
        /// Output file; prints to stdout when omitted.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Import a config file into the user layer.
    Import {
        /// Path to a config file.
        path: PathBuf,
    },
}

impl Cli {
    /// Fold the parsed surface into a single `Action`, applying defaults.
    fn into_action(self) -> Result<Action, String> {
        let Cli {
            provider,
            model,
            resume,
            run,
            command,
            version: _,
        } = self;
        if let Some(session_path) = resume {
            if command.is_some() {
                return Err("--resume cannot be combined with a subcommand".to_string());
            }
            return Ok(Action::ResumeSession {
                // `Some(None)` == bare `--resume` -> interactive picker.
                session_path,
                command: run,
                provider,
                model,
            });
        }
        match command {
            None => Ok(Action::Repl { provider, model }),
            Some(Command::Chat) => Ok(Action::Repl { provider, model }),
            Some(Command::Prompt { text, quiet, json }) => Ok(Action::Prompt {
                instruction: text.join(" "),
                provider,
                model,
                quiet,
                json,
            }),
            Some(Command::Search { query, limit, json }) => {
                let joined = query.join(" ");
                if joined.trim().is_empty() {
                    return Err("search requires a query string".to_string());
                }
                Ok(Action::Search {
                    query: joined,
                    limit,
                    json,
                })
            }
            Some(Command::SystemPrompt(args)) => {
                let cwd = match args.cwd {
                    Some(path) => path,
                    None => env::current_dir().map_err(|error| error.to_string())?,
                };
                let date = args.date.unwrap_or_else(current_date);
                Ok(Action::PrintSystemPrompt { cwd, date })
            }
            Some(Command::Config(cfg)) => Ok(Action::Config {
                action: match cfg.action {
                    ConfigCommand::Export { output } => ConfigAction::Export { output },
                    ConfigCommand::Import { path } => ConfigAction::Import { path },
                },
            }),
            Some(Command::Doctor(args)) => Ok(Action::Doctor {
                fix: args.fix,
                ai: args.ai,
            }),
            Some(Command::Init(args)) => Ok(Action::Init { force: args.force }),
            Some(Command::Models(args)) => Ok(Action::Models {
                provider,
                model,
                balance: args.balance,
            }),
        }
    }
}

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
    selection: &ProviderSelection,
    mode: &str,
    runtime: &mut AgentRuntime,
    planning: &mut bool,
    plan_path: &mut Option<PathBuf>,
) {
    match build_runtime(Session::new(), selection.clone(), true, mode) {
        Ok(fresh) => {
            *runtime = fresh;
            *planning = false;
            *plan_path = None;
            // A cleared session is a new conversation: give it a fresh file and
            // history row instead of appending onto the one just wiped.
            rotate_session_id();
            println!("session cleared.");
        }
        Err(error) => println!("failed to reset session: {error}"),
    }
}

/// Human-readable platform name and version for the system-prompt environment
/// section, via `os_info`, so the model sees e.g. "Windows 11" / "macOS 14.5"
/// instead of the bare `env::consts::OS` paired with a literal "unknown".
fn os_platform() -> (String, String) {
    let info = os_info::get();
    let version = info.version().to_string();
    let version = if version.trim().is_empty() {
        String::from("unknown")
    } else {
        version
    };
    (info.os_type().to_string(), version)
}

fn print_system_prompt(cwd: PathBuf, date: String) {
    let (os_name, os_version) = os_platform();
    match load_system_prompt(cwd, home_dir(), date, os_name, os_version) {
        Ok(sections) => println!("{}", sections.join("\n\n")),
        Err(error) => {
            eprintln!("failed to build system prompt: {error}");
            process::exit(1);
        }
    }
}

/// Detect common toolchains from marker files so the scaffold can pre-fill the
/// build/test commands instead of leaving them all blank.
fn detect_toolchains(cwd: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    if cwd.join("Cargo.toml").exists() {
        lines.push(
            "- Rust (cargo): `cargo build`, `cargo test`, `cargo clippy --all-targets`".to_string(),
        );
    }
    if cwd.join("package.json").exists() {
        lines.push("- Node.js (npm): `npm install`, `npm test`, `npm run build`".to_string());
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("requirements.txt").exists() {
        lines.push("- Python: `pip install -e .`, `pytest`".to_string());
    }
    if cwd.join("go.mod").exists() {
        lines.push("- Go: `go build ./...`, `go test ./...`".to_string());
    }
    lines
}

/// Write a starting `AGENTS.md` into `cwd`. Never clobbers an existing file
/// unless `force` is set; heartflow loads `AGENTS.md` (and legacy `CLAUDE.md`)
/// as repo instructions, so this scaffolds the neutral standard name.
fn write_agents_skeleton(cwd: &Path, force: bool) -> Result<String, String> {
    let path = cwd.join("AGENTS.md");
    if path.exists() && !force {
        return Ok(format!(
            "AGENTS.md already exists at {} (pass --force to overwrite)",
            path.display()
        ));
    }
    let name = cwd.file_name().map_or_else(
        || "this repository".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let mut toolchains = detect_toolchains(cwd);
    if toolchains.is_empty() {
        toolchains.push("- TODO: list the build, test, and run commands".to_string());
    }
    let content = format!(
        "# AGENTS.md\n\
         \n\
         Guidance for AI agents working in `{name}`.\n\
         \n\
         ## Project overview\n\
         \n\
         TODO: one paragraph on what this project does and its key technologies.\n\
         \n\
         ## Commands\n\
         \n\
         Detected toolchains (verify and prune):\n\
         \n\
         {cmds}\n\
         \n\
         ## Architecture\n\
         \n\
         TODO: describe the module layout and the boundaries that must not be crossed.\n\
         \n\
         ## Conventions\n\
         \n\
         TODO: coding style, error handling, and the quality gates to run before committing.\n",
        cmds = toolchains.join("\n"),
    );
    fs::write(&path, content).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(format!("wrote AGENTS.md -> {}", path.display()))
}

/// Provider self-bootstrap (`hf models`): list advertised models, report the
/// current model's context window when the server discloses it, and (for
/// `DeepSeek`) fetch the account balance. Read-only; requires an explicit
/// provider so the transport is known. Other protocols are reported as pending.
async fn run_models(
    provider: Option<String>,
    model: Option<String>,
    balance: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let selection = resolve_selection(provider.as_deref(), model.as_deref())?;
    let ProviderSelection::Profile(profile) = &selection else {
        return Err("model self-discovery needs an explicit provider (e.g. `hf --provider deepseek models`); env-driven Anthropic exposes no model endpoint".into());
    };
    if profile.protocol != ProviderProtocol::OpenAi {
        return Err(format!(
            "self-bootstrap is only wired for the OpenAI-compatible (DeepSeek) protocol right now; '{}' speaks {}",
            profile.model,
            profile.protocol.as_str()
        )
        .into());
    }
    let client = OpenAiClient::new(profile.api_key.clone()).with_base_url(profile.base_url.clone());

    println!(
        "provider: {} (base {})",
        profile.protocol.as_str(),
        profile.base_url
    );
    println!("current model: {}", profile.model);

    match client.list_models().await {
        Ok(models) => {
            println!("\nmodels ({}):", models.len());
            for info in &models {
                let context = info.context_length.map_or_else(
                    || "context: n/a".to_string(),
                    |tokens| format!("context: {tokens}"),
                );
                let owner = info.owned_by.as_deref().unwrap_or("-");
                println!("  {:<28} {owner:<12} {context}", info.id);
            }
            match models
                .iter()
                .find(|info| info.id == profile.model)
                .and_then(|info| info.context_length)
            {
                Some(tokens) => println!(
                    "\n{} context window: {tokens} tokens (server-reported)",
                    profile.model
                ),
                None => println!(
                    "\n{} context window: not reported by this endpoint; set HEARTFLOW_AUTO_COMPACT_TOKENS to the model's context window to enable summarize-then-compact once a session crosses half of it",
                    profile.model
                ),
            }
        }
        Err(error) => println!("\nmodel list unavailable: {error}"),
    }

    if profile.base_url.contains("deepseek") {
        if !balance {
            // Balance queries hit a quota-consuming endpoint, so they are opt-in.
            println!("\nbalance: skipped (re-run with `--balance` to query; this uses API quota)");
            return Ok(());
        }
        match client.get_balance().await {
            Ok(balance) if !balance.is_available => {
                println!("\nbalance: unavailable for this account");
            }
            Ok(balance) => {
                println!("\nbalance:");
                for entry in &balance.balance_infos {
                    println!(
                        "  {}: total {} (granted {}, topped-up {})",
                        entry.currency,
                        entry.total_balance,
                        entry.granted_balance,
                        entry.topped_up_balance
                    );
                }
            }
            Err(error) => println!("\nbalance unavailable: {error}"),
        }
    }
    Ok(())
}

fn resume_session(session_path: &Path, command: &str) {
    let session = match load_saved_session(session_path) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("failed to restore session: {error}");
            process::exit(1);
        }
    };
    if !command.starts_with('/') {
        eprintln!("unsupported resumed command: {command}");
        process::exit(2);
    }
    let Some(result) = commands::handle_slash_command(
        command,
        &session,
        CompactionConfig {
            max_estimated_tokens: 0,
            ..CompactionConfig::default()
        },
    ) else {
        let hint = match editor::suggest_command(command) {
            Some(good) => format!(" (did you mean {good}?)"),
            None => String::new(),
        };
        eprintln!("unknown slash command: {command}{hint}");
        process::exit(2);
    };
    if let Err(error) = result.session.save_to_path(session_path) {
        eprintln!("failed to persist resumed session: {error}");
        process::exit(1);
    }
    println!("{}", result.message);
}

fn stdin_is_terminal() -> bool {
    io::stdin().is_terminal()
}

fn pick_session() -> Result<PathBuf, String> {
    let sessions = list_sessions();
    if sessions.is_empty() {
        return Err("no saved sessions found".to_string());
    }

    if stdin_is_terminal() {
        let labels: Vec<String> = sessions
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let chosen = Select::new("Resume which session?", labels)
            .prompt()
            .map_err(|_| "session selection cancelled".to_string())?;
        return sessions
            .into_iter()
            .find(|path| path.display().to_string() == chosen)
            .ok_or_else(|| "selected session vanished".to_string());
    }

    // Non-interactive fallback: numbered selection over a plain stream.
    println!("Saved sessions (newest first):");
    for (index, path) in sessions.iter().enumerate().take(9) {
        println!("  {}. {}", index + 1, path.display());
    }
    print!("Select session number: ");
    let _ = io::stdout().flush();

    let mut choice = String::new();
    io::stdin()
        .read_line(&mut choice)
        .map_err(|error| error.to_string())?;
    let index: usize = choice
        .trim()
        .parse()
        .map_err(|_| format!("invalid selection: {}", choice.trim()))?;
    sessions
        .get(index.wrapping_sub(1))
        .cloned()
        .ok_or_else(|| format!("selection out of range: {index}"))
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn sessions_dir() -> PathBuf {
    home_dir().join(".heartflow").join("sessions")
}

/// System-level SQLite history database (searchable mirror of saved sessions).
fn store_path() -> PathBuf {
    home_dir().join(".heartflow").join("heartflow.db")
}

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default()
}

/// Restore a saved session: the snapshot file plus any append-only segment
/// beside it. A segment that cannot be placed on the snapshot never fails the
/// restore — the snapshot is complete on its own — so it is dropped with a note
/// on stderr. See `Session::load_with_segment`.
fn load_saved_session(path: &Path) -> Result<Session, String> {
    let (session, segment) = Session::load_with_segment(path).map_err(|error| error.to_string())?;
    if let Some(warning) = segment.warning() {
        eprintln!("{}: {warning}", path.display());
    }
    Ok(session)
}

fn save_session(session: &Session) -> io::Result<PathBuf> {
    let dir = sessions_dir();
    fs::create_dir_all(&dir)?;
    // Never persist a live credential: scrub the transcript that reaches disk
    // (both the authoritative JSON and the SQLite mirror) on a clone, so the
    // in-memory session that talks to the provider is left verbatim.
    let secrets = registered_secrets();
    let redacted = redact_session(session, &secrets);
    // One file per conversation, keyed by the stable id (see current_session_id):
    // each turn overwrites it via the atomic temp+rename in `save_to_path`, so a
    // crash keeps the last complete snapshot rather than a truncated tail.
    let path = dir.join(format!("{}.json", current_session_id()));
    redacted
        .save_to_path(&path)
        .map_err(|error| io::Error::other(error.to_string()))?;
    // Best-effort mirror into the searchable store; the JSON file stays
    // authoritative, so a store failure must never fail the save.
    mirror_to_store(&path, &redacted);
    Ok(path)
}

/// Credential literals learned this run (the resolved API key / auth token).
/// Registered when a runtime is built and replayed against every saved
/// transcript so a key that matches no structural redaction pattern is still
/// scrubbed before it reaches disk. De-duplicated; a redeployed provider just
/// re-registers its (identical) value.
static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn register_secret(value: &str) {
    let value = value.trim();
    // Mirrors redact_text's floor: a shorter string is ordinary text.
    if value.len() < 4 {
        return;
    }
    if let Ok(mut guard) = SECRETS.lock() {
        if !guard.iter().any(|existing| existing == value) {
            guard.push(value.to_string());
        }
    }
}

fn registered_secrets() -> Vec<String> {
    SECRETS
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Process-lifetime read-write handle to the history store, opened on the first
/// write. Reusing one connection across turns avoids re-running the pragmas and
/// migration on every save; WAL still lets other processes read/search safely.
static SHARED_STORE: OnceLock<Option<Store>> = OnceLock::new();

fn shared_store() -> Option<&'static Store> {
    SHARED_STORE
        .get_or_init(|| {
            let path = store_path();
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            Store::open(&path).ok()
        })
        .as_ref()
}

fn mirror_to_store(json_path: &Path, session: &Session) {
    let now = unix_secs();
    let meta = SessionMeta {
        created_at: now,
        updated_at: now,
        source_path: Some(json_path.display().to_string()),
        provider: None,
        model: None,
    };
    // Store unavailable (e.g. unwritable home): the JSON file is authoritative,
    // so a mirror failure is logged and never blocks the save.
    let Some(store) = shared_store() else {
        return;
    };
    let Ok(mut guard) = MIRROR.lock() else {
        return;
    };
    let tracker = guard.get_or_insert_with(|| MirrorTracker {
        id: current_session_id(),
        last_len: 0,
        force_full: true,
    });

    // Append is only sound when the transcript is a strict extension of the last
    // mirrored prefix; a shrink (compaction) or forced rewrite (pin/task reset)
    // falls through to the full rewrite, which is always correct.
    let len = session.messages.len();
    let can_append = tracker.can_append(len);
    let result = if can_append {
        store.append_messages(
            &tracker.id,
            &meta,
            tracker.last_len,
            &session.messages[tracker.last_len..],
            tracker.last_len,
        )
    } else {
        store.save_session(&tracker.id, &meta, &session.messages)
    };

    match result {
        Ok(()) => {
            tracker.last_len = len;
            tracker.force_full = false;
        }
        // The stored base moved under us (another process rewrote it, or the DB
        // was rebuilt): recover once with a full rewrite, which cannot drift.
        Err(StoreError::Drift { .. }) => {
            if store
                .save_session(&tracker.id, &meta, &session.messages)
                .is_ok()
            {
                tracker.last_len = len;
                tracker.force_full = false;
            } else {
                tracing::debug!("history store mirror fallback failed");
            }
        }
        Err(error) => tracing::debug!("history store mirror failed: {error}"),
    }
}

/// Per-process mirror bookkeeping so the SQLite history is written incrementally
/// instead of re-dumping the whole transcript every turn.
///
/// The JSON transcript stays authoritative and keeps its per-save file naming;
/// this governs only the *mirror*. A single stable `id` represents the current
/// conversation inside the history DB, so search returns one row per
/// conversation rather than a snapshot per turn. `last_len` is how many messages
/// are already mirrored under `id`; `force_full` marks that the stored prefix is
/// no longer a trustworthy base (an in-place pin toggle, a compaction, or a
/// task-context reset changed/shortened already-mirrored rows), so the next
/// write must be a full rewrite rather than a tail append.
struct MirrorTracker {
    id: String,
    last_len: usize,
    force_full: bool,
}

impl MirrorTracker {
    /// Whether this turn's mirror may be an incremental append onto the existing
    /// base. Sound only when the transcript strictly extends the last mirrored
    /// prefix: no rewrite was forced (pin/compaction/task-reset), at least one
    /// row is already stored (otherwise there is no base to extend), and the
    /// session did not shrink (a shorter transcript means earlier rows changed).
    /// When false, the caller performs a full `save_session` rewrite.
    #[must_use]
    fn can_append(&self, len: usize) -> bool {
        !self.force_full && self.last_len > 0 && len >= self.last_len
    }
}

static MIRROR: Mutex<Option<MirrorTracker>> = Mutex::new(None);

/// Force the next mirror to fully rewrite. Called after any operation that
/// mutates already-mirrored rows without necessarily growing the transcript.
fn note_mirror_rewrite() {
    if let Ok(mut guard) = MIRROR.lock() {
        if let Some(tracker) = guard.as_mut() {
            tracker.force_full = true;
        }
    }
}

/// Stable on-disk identity of the conversation this process persists. A single
/// `hf` run is one conversation, so `save_session` keeps writing the SAME file
/// (`sessions/<id>.json`) instead of minting a fresh timestamped snapshot every
/// turn. The old per-turn naming fragmented one conversation across N files and
/// re-dumped the whole transcript each turn; one atomic snapshot per
/// conversation is durable and keeps `/sessions` at one row each. The id is a
/// start timestamp so listings stay newest-first; it is adopted when a run
/// resumes/opens a transcript and rotated on `/clear`.
static SESSION_ID: Mutex<Option<String>> = Mutex::new(None);

/// A fresh conversation id: start millis then PID, unique across concurrent
/// `hf` processes sharing the history DB and sortable by recency.
fn new_session_id() -> String {
    format!("{}-{}", unix_millis(), std::process::id())
}

/// The active conversation id, minted on first use. A poisoned lock falls back
/// to a fresh id rather than blocking persistence.
fn current_session_id() -> String {
    match SESSION_ID.lock() {
        Ok(mut guard) => guard.get_or_insert_with(new_session_id).clone(),
        Err(_) => new_session_id(),
    }
}

/// Rebind persistence to `id`: point future saves at that conversation and drop
/// the mirror tracker so the next write re-initializes (full, not append) under
/// the same id, keeping the JSON file and the SQLite row keyed identically.
fn rebind_conversation(id: String) {
    if let Ok(mut guard) = SESSION_ID.lock() {
        *guard = Some(id);
    }
    if let Ok(mut guard) = MIRROR.lock() {
        *guard = None;
    }
}

/// Start a brand-new conversation (after `/clear`): a fresh id for both the
/// transcript file and the history mirror.
fn rotate_session_id() {
    rebind_conversation(new_session_id());
}

/// Adopt the conversation persisted at `path` (resume / `/open`): continuing it
/// overwrites the same file and updates the same history row instead of
/// branching into a new one.
fn adopt_session_path(path: &Path) {
    let id = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(new_session_id);
    rebind_conversation(id);
}

fn list_sessions() -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(sessions_dir())
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    entries.sort_by_key(|path| {
        std::cmp::Reverse(
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    });
    entries
}

async fn run_repl(
    selection: ProviderSelection,
    session: Session,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut selection = selection;
    let mut mode = default_permission_mode(true);
    let cwd = env::current_dir()?;
    let mut watcher = ConfigWatcher::new(&cwd, &home_dir());
    let mut runtime = build_runtime(session, selection.clone(), true, &mode)?;
    let mut prompter = CliPermissionPrompter::new();
    // Planning runs on the *same* runtime (policy swapped in place) so the todo
    // ledger and conversation survive the plan -> execute handoff. `plan_path`
    // is the exact document the CLI reserved for the current planning session.
    let mut planning = false;
    let mut plan_path: Option<PathBuf> = None;
    let history_path = home_dir().join(".heartflow").join("history.txt");
    let mut editor = editor::ReplEditor::new(&history_path);
    // Single live-turn state (queue/guide core). Under the blocking line
    // editor the model is running only inside a turn, so the queue stays
    // empty until P4-c wires keyboard polling; the injection point below is
    // already the final one.
    let mut model = HeartModel::new();
    mascot::draw_banner(theme::Theme::current(), &mut io::stdout())?;
    println!("heartflow interactive mode");
    println!(
        "Input: Enter sends, Alt/Shift+Enter newline, Up/Down history, Tab completes / commands."
    );
    println!("Quit with /exit or Ctrl+D. Ctrl+C interrupts a running turn; on an idle line it just clears it.");

    loop {
        // Turn boundary: a finished turn's queued follow-ups are merged into
        // one injection message here (locked semantics: never interrupt, always
        // deliver after the turn). A refused/failed injection is re-queued by
        // the submit path below, so nothing is lost.
        if let Some(injection) = model.queue_mut().drain_injection() {
            model.begin_turn();
            let delivery = if planning {
                run_turn_interactive(&mut runtime, &injection, Some(&mut BlockPrompter)).await
            } else {
                run_turn_interactive(&mut runtime, &injection, Some(&mut prompter)).await
            };
            model.end_turn();
            match delivery {
                Ok(outcome) => {
                    maybe_auto_compact(&mut runtime);
                    editor.note_turn(outcome.ok);
                }
                Err(error) => {
                    println!("queued delivery failed: {error}");
                    editor.note_turn(false);
                    let _ = model.enqueue(&injection);
                }
            }
        }
        // Ctrl+D / EOF is a documented quit path (see the banner): save the
        // session and print the resume command just like `/exit`, so the
        // conversation is never silently dropped on the EOF path.
        let Some(line) = editor.read_line()? else {
            exit_with_resume_hint(&runtime);
            break;
        };
        // Config edited on disk since the last turn: re-resolve the provider and
        // rebuild the runtime, keeping the conversation and current mode.
        if watcher.changed() {
            reload_config(&cwd, &mut selection, &mut runtime, &mode);
            planning = false;
        }
        let trimmed = line.trim();
        match trimmed {
            "" => {}
            "/exit" | "/quit" => {
                exit_with_resume_hint(&runtime);
                break;
            }
            "/help" => print_repl_help(),
            "/status" => print_status(&runtime, selection.model(), &mode),
            "/save" => match save_session(runtime.session()) {
                Ok(path) => println!("session saved -> {}", path.display()),
                Err(error) => println!("failed to save session: {error}"),
            },
            "/clear" => handle_clear_command(
                &selection,
                &mode,
                &mut runtime,
                &mut planning,
                &mut plan_path,
            ),
            "/sessions" => print_sessions_listing(),
            _ if trimmed == "/open" || trimmed.starts_with("/open ") => {
                handle_open_command(trimmed, &selection, &mode, &mut runtime);
            }
            _ if trimmed == "/remember" || trimmed.starts_with("/remember ") => {
                handle_remember_command(trimmed);
            }
            "/compact" => force_compact(&mut runtime),
            "/pin" => handle_pin_command(&mut runtime),
            "/mcp" => print_mcp_status(&runtime),
            "/restart" => {
                if confirm_restart() {
                    RESTART_REQUESTED.store(true, Ordering::SeqCst);
                    break;
                }
                println!("restart cancelled.");
            }
            _ if trimmed == "/search" || trimmed.starts_with("/search ") => {
                handle_search_command(trimmed);
            }
            _ if trimmed == "/expand" || trimmed.starts_with("/expand ") => {
                handle_expand_command(trimmed);
            }
            _ if trimmed == "/queue" || trimmed.starts_with("/queue ") => {
                handle_queue_command(trimmed, &mut model);
            }
            _ if trimmed == "/guide" || trimmed.starts_with("/guide ") => {
                handle_guide_command(trimmed, &runtime);
            }
            "/init" => match write_agents_skeleton(&cwd, false) {
                Ok(message) => println!("{message}"),
                Err(error) => println!("failed to write AGENTS.md: {error}"),
            },
            _ if trimmed == "/mode" || trimmed.starts_with("/mode ") => {
                handle_mode_command(trimmed, &mut mode, &selection, &mut runtime);
                planning = false;
            }
            _ if trimmed == "/model" || trimmed.starts_with("/model ") => {
                handle_model_command(trimmed, &mut selection, &mut runtime, &mode);
                planning = false;
            }
            _ if trimmed == "/plan" || trimmed.starts_with("/plan ") => {
                handle_plan_command(
                    trimmed,
                    &mut planning,
                    &mut plan_path,
                    &cwd,
                    &mode,
                    &mut runtime,
                    &mut prompter,
                )
                .await?;
            }
            _ if trimmed.starts_with('!') => {
                handle_bang_command(trimmed).await;
            }
            _ if trimmed.starts_with('/') => report_unknown_command(trimmed),
            _ => {
                // Running turns cannot be submitted through the blocking line
                // editor yet (P4-c); keep the queue routing explicit so the
                // ratatui event loop only has to flip `begin_turn`.
                if model.is_running() {
                    if !model.enqueue(trimmed) {
                        println!("follow-up queue is full; /queue to inspect");
                    }
                } else {
                    let outcome = if planning {
                        run_turn_interactive(&mut runtime, trimmed, Some(&mut BlockPrompter))
                            .await?
                    } else {
                        run_turn_interactive(&mut runtime, trimmed, Some(&mut prompter)).await?
                    };
                    editor.note_turn(outcome.ok);
                }
                maybe_auto_compact(&mut runtime);
            }
        }
    }
    Ok(())
}

/// `/queue` — inspect and edit the pending follow-up messages.
fn handle_queue_command(input: &str, model: &mut HeartModel) {
    let arg = input.trim().strip_prefix("/queue").unwrap_or("").trim();
    match arg {
        "" => {
            if model.queue().is_empty() {
                println!("queue is empty (messages sent during a running turn wait here)");
                return;
            }
            println!("{} queued:", model.queue().len());
            for (index, text) in model.queue().items().iter().enumerate() {
                println!("  {}. {}", index + 1, truncate_chars(text, 120));
            }
        }
        "pop" => match model.queue_mut().cancel_last() {
            Some(text) => println!("withdrawd last queued message:\n{text}"),
            None => println!("queue is empty"),
        },
        "clear" => {
            let drained = model.queue_mut().drain_injection();
            match drained {
                Some(_) => println!("queue cleared."),
                None => println!("queue is empty"),
            }
        }
        other => println!("usage: /queue [pop|clear] (`{other}` is not a subcommand)"),
    }
}

/// `/guide <TASK>` — assemble the three-part guide draft (prior work, current
/// state, next task) locally with zero token spend, mirroring Qoder's
/// guidance flow; the operator edits the printed draft and resends it.
fn handle_guide_command(input: &str, runtime: &AgentRuntime) {
    let task = input.trim().strip_prefix("/guide").unwrap_or("").trim();
    if task.is_empty() {
        println!("usage: /guide <下一步任务描述>");
        return;
    }
    let messages = &runtime.session().messages;
    let mut work_log = String::new();
    for message in messages.iter().rev().take(6).rev() {
        let role = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            // System prompt and raw tool turns are noise for re-entry context;
            // the guide summarizes the human/assistant thread only.
            MessageRole::System | MessageRole::Tool => continue,
        };
        let text = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        if !text.trim().is_empty() {
            work_log.push_str(&guide_log_line(role, &text, 160));
            work_log.push('\n');
        }
    }
    let state = format!(
        "{} pending tasks in the ledger",
        runtime.executor().todo_ledger().pending_tasks()
    );
    let draft = build_guide(GuideSections {
        work_log: &work_log,
        state: &state,
        task,
    });
    println!("guide draft (edit and resend, or paste into the next message):\n");
    println!("{draft}\n");
}

/// `!<cmd>` — run a shell command directly through the same bash tool path the
/// model uses (pwsh on Windows, UTF-8 wrapped), with no model round-trip.
/// Dangerous commands are confirmed first; output is folded like tool output
/// and stays reachable via `/expand`.
async fn handle_bang_command(input: &str) {
    let command = input.trim().strip_prefix('!').unwrap_or("").trim();
    if command.is_empty() {
        println!("usage: !<shell command>   e.g. !git status");
        return;
    }
    if is_dangerous_command(command)
        && !Confirm::new(&format!(
            "Run potentially destructive command?\n  {command}"
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
            println!("{}", format!("shell error: {error}").red());
            return;
        }
        Err(task) => {
            println!("{}", format!("shell task failed: {task}").red());
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
        println!("{}", "(no output)".dark_grey());
    } else {
        let markdown = fold_tool_output("shell", &body, FOLD_TOOL_OUTPUT_LINES);
        println!("{}", TerminalRenderer::new().render_markdown(&markdown));
    }
    if output.interrupted {
        println!("{}", "! interrupted (timeout)".yellow());
    } else if let Some(interpretation) = &output.return_code_interpretation {
        println!("{}", format!("! {interpretation}").dark_grey());
    }
}

/// Print the saved-session list for the `/sessions` command.
fn print_sessions_listing() {
    let sessions = list_sessions();
    if sessions.is_empty() {
        println!("no saved sessions found");
        return;
    }
    println!("saved sessions (newest first):");
    for (index, path) in sessions.iter().enumerate().take(9) {
        println!("  {}. {}", index + 1, path.display());
    }
    println!("resume with: heartflow --resume");
}

/// Save the live session on the way out and print the exact command to resume
/// this section later, so `/exit` leaves a clear path back to the conversation.
fn exit_with_resume_hint(runtime: &AgentRuntime) {
    match save_session(runtime.session()) {
        Ok(path) => {
            println!("session saved -> {}", path.display());
            println!("to resume this section: hf --resume=\"{}\"", path.display());
        }
        Err(error) => println!("failed to save session: {error}"),
    }
}

/// Jump the live REPL to a previously saved section: reload session N (using the
/// same newest-first numbering as `/sessions`) into a freshly built runtime,
/// swapping the in-place conversation without restarting the process. The
/// connected MCP servers and provider are reconstructed by `build_runtime`.
fn handle_open_command(
    input: &str,
    selection: &ProviderSelection,
    mode: &str,
    runtime: &mut AgentRuntime,
) {
    let arg = input.trim().strip_prefix("/open").unwrap_or("").trim();
    if arg.is_empty() {
        println!("usage: /open <N>   (N is a 1-based index from /sessions)");
        return;
    }
    let Ok(index) = arg.parse::<usize>() else {
        println!("`{arg}` is not a valid session number");
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
    let path = &sessions[index - 1];
    match load_saved_session(path) {
        Ok(session) => match build_runtime(session, selection.clone(), true, mode) {
            Ok(fresh) => {
                *runtime = fresh;
                // Own this transcript's identity so subsequent turns keep
                // writing the same file and history row.
                adopt_session_path(path);
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
fn remember_note(home: &Path, text: &str) -> io::Result<bool> {
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

/// Models known to work with the built-in `DeepSeek` provider (per the official
/// API docs, 2026-09). `deepseek-v4-flash` is accepted by the endpoint but is
/// served by `DeepSeek-V4.1-Flash`.
const KNOWN_DEEPSEEK_MODELS: &[&str] = &["deepseek-flash", "deepseek-v4-flash", "deepseek-v4-pro"];

fn handle_model_command(
    input: &str,
    selection: &mut ProviderSelection,
    runtime: &mut AgentRuntime,
    mode: &str,
) {
    let requested = input
        .strip_prefix("/model")
        .map(str::trim)
        .unwrap_or_default();
    if requested.is_empty() {
        println!("model: {}", selection.model());
        println!("known models: {}", KNOWN_DEEPSEEK_MODELS.join(", "));
        println!("switch with: /model NAME");
        return;
    }

    let mut next = selection.clone();
    next.set_model(requested);
    match build_runtime(runtime.session().clone(), next.clone(), true, mode) {
        Ok(rebuilt) => {
            *runtime = rebuilt;
            *selection = next;
            println!("model -> {}", selection.model());
        }
        Err(error) => println!("failed to switch model: {error}"),
    }
}

/// Permission modes exposed to `/mode`, ordered from most to least restrictive.
const KNOWN_PERMISSION_MODES: &[&str] = &["read-only", "workspace-write", "full"];

/// Re-read config from disk and rebuild the runtime in place, preserving the
/// live conversation and permission mode. Used for between-turn hot reload.
fn reload_config(
    cwd: &Path,
    selection: &mut ProviderSelection,
    runtime: &mut AgentRuntime,
    mode: &str,
) {
    match load_provider_selection(cwd, &home_dir(), None, None) {
        Ok(next) => match build_runtime(runtime.session().clone(), next.clone(), true, mode) {
            Ok(rebuilt) => {
                *runtime = rebuilt;
                *selection = next;
                println!("config reloaded (model={})", selection.model());
            }
            Err(error) => println!("config changed but reload failed: {error}"),
        },
        Err(error) => println!("config changed but is not usable: {error}"),
    }
}

/// Context window (tokens) from `HEARTFLOW_AUTO_COMPACT_TOKENS`, interpreted as
/// the model's window. Returns `None` when unset, unparsable or zero, so a
/// `config.toml` `[provider] context_window` can supply the value instead.
fn env_context_window() -> Option<usize> {
    env::var("HEARTFLOW_AUTO_COMPACT_TOKENS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
}

/// Resolve the compaction window by precedence: the env var beats the resolved
/// provider profile's `context_window`; 0 disables window-based compaction.
fn resolve_context_window(selection: &ProviderSelection) -> usize {
    let from_profile = || match selection {
        ProviderSelection::Env { .. } => None,
        ProviderSelection::Profile(profile) => profile
            .context_window
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
    };
    env_context_window().or_else(from_profile).unwrap_or(0)
}

/// Provider-keyed compaction tuning: how many recent messages stay verbatim
/// and how deep the verbatim tool-result replay tail reaches. Anthropic keeps
/// a longer verbatim prefix because its prompt cache is billed on an exact
/// message prefix — collapsing fewer recent turns maximises cache hits — while
/// the stateless OpenAI/DeepSeek dialects keep a shorter window at no cache cost
/// (mirrors the per-provider compaction handlers in mature agent runtimes).
#[must_use]
fn compaction_profile(protocol: Option<ProviderProtocol>) -> (usize, usize) {
    match protocol {
        Some(ProviderProtocol::Anthropic) => (8, 16),
        Some(ProviderProtocol::OpenAi | ProviderProtocol::OpenAiResponses) | None => (6, 12),
    }
}

/// The shared compaction policy for one provider: `preserve_recent_messages`
/// recent turns are kept verbatim, and the trigger is half of
/// `context_window_tokens` (a zero window falls back to the absolute
/// threshold). The window comes from the resolved profile (env override wins);
/// the preserve/replay counts come from the provider's compaction profile.
fn compaction_config(selection: &ProviderSelection) -> CompactionConfig {
    let context_window = resolve_context_window(selection);
    let protocol = match selection {
        ProviderSelection::Env { .. } => None,
        ProviderSelection::Profile(profile) => Some(profile.protocol),
    };
    let (preserve_recent_messages, default_replay_tail) = compaction_profile(protocol);
    let replay_verbatim_tail = env::var("HEARTFLOW_REPLAY_VERBATIM_TAIL")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        // 0 would stub even the tool result the model just produced; treat it as
        // unset and keep the provider default.
        .filter(|n| *n > 0)
        .unwrap_or(default_replay_tail);
    CompactionConfig {
        preserve_recent_messages,
        context_window_tokens: context_window,
        replay_verbatim_tail,
        ..CompactionConfig::default()
    }
}

/// After-turn safety net for the in-turn window pre-compaction wired in
/// `build_runtime`. A turn's final streamed message can push the session past
/// half the window after the last in-loop check, so re-evaluate once the turn
/// ends. Reuses the runtime's own policy so the window and preserve count stay
/// identical to the in-turn check.
fn maybe_auto_compact(runtime: &mut AgentRuntime) {
    let config = runtime.compaction();
    if config.context_window_tokens == 0 {
        return;
    }
    if runtime.should_compact(config) {
        let result = runtime.compact(config);
        note_mirror_rewrite();
        println!(
            "auto-compacted {} older messages into a summary (context pressure).",
            result.removed_message_count
        );
    }
}

/// Manual `/compact`: force a compaction immediately by zeroing both the window
/// and absolute thresholds so the auto gates can't skip it, while still keeping
/// the recent-message count the active policy preserves. Reports when the
/// session is still too short to have anything to compact.
fn force_compact(runtime: &mut AgentRuntime) {
    let config = CompactionConfig {
        preserve_recent_messages: runtime.compaction().preserve_recent_messages,
        max_estimated_tokens: 0,
        context_window_tokens: 0,
        ..CompactionConfig::default()
    };
    let result = runtime.compact(config);
    if result.removed_message_count == 0 {
        println!("nothing to compact: session is within the preserve window.");
    } else {
        note_mirror_rewrite();
        println!(
            "Compacted {} messages into a resumable summary.",
            result.removed_message_count
        );
    }
}

/// Manual `/pin`: toggle the never-compacted flag on the most recent
/// assistant/user turn. A pinned message survives every compaction verbatim
/// instead of being folded into the summary, so key constraints or decisions
/// stay in context. Reports the new state and how many messages are pinned.
fn handle_pin_command(runtime: &mut AgentRuntime) {
    match runtime.toggle_pin_latest() {
        Some(pinned) => {
            // Pinning flips a flag on an already-mirrored row without changing
            // the transcript length, so the mirror must be fully rewritten.
            note_mirror_rewrite();
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
        runtime.session().clone(),
        selection.clone(),
        true,
        normalized,
    ) {
        Ok(rebuilt) => {
            *runtime = rebuilt;
            *mode = normalized.to_string();
            println!("mode -> {normalized}");
        }
        Err(error) => println!("failed to switch mode: {error}"),
    }
}

/// Handle `/plan [goal|approve|end|status]`. Planning swaps the live runtime
/// to a hard-gated policy (only the plan document is writable) instead of
/// rebuilding it, so the todo ledger and conversation survive the transition
/// into execution.
async fn handle_plan_command(
    input: &str,
    planning: &mut bool,
    plan_path: &mut Option<PathBuf>,
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
            println!("planning is {}", if *planning { "ON" } else { "off" });
            println!("  /plan <goal>    research and draft a plan (writes gated to the plan file)");
            println!("  /plan approve   load the approved plan into the task list and execute it");
            println!("  /plan end       leave planning without executing");
        }
        "status" => {
            println!("planning is {}", if *planning { "ON" } else { "off" });
            if let Some(path) = plan_path {
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
        "end" => end_planning(planning, mode, runtime),
        "approve" => approve_plan(planning, plan_path, mode, runtime, prompter).await?,
        goal => start_planning(goal, planning, plan_path, cwd, runtime).await?,
    }
    Ok(())
}

/// Begin a planning session: reserve the plan document, engage the gated
/// policy, and run the first research-and-plan turn under `BlockPrompter`.
async fn start_planning(
    goal: &str,
    planning: &mut bool,
    plan_path: &mut Option<PathBuf>,
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
    *planning = true;
    *plan_path = Some(path.clone());
    set_runtime_mode_policy(runtime, "plan");
    println!("planning mode ON - writes are blocked except the plan document.");
    println!("plan file: {}", path.display());
    run_turn_interactive(runtime, &plan_brief(goal, &path), Some(&mut BlockPrompter)).await?;
    println!();
    println!("Refine by typing notes (still planning), then `/plan approve` to execute or `/plan end` to stop.");
    Ok(())
}

/// Leave planning and restore the session's normal permission policy in place.
fn end_planning(planning: &mut bool, mode: &str, runtime: &mut AgentRuntime) {
    if !*planning {
        println!("not planning.");
        return;
    }
    set_runtime_mode_policy(runtime, mode);
    *planning = false;
    println!("planning mode off - back to {mode}.");
}

/// Approve the plan: parse its checkboxes, seed the todo ledger deterministically,
/// restore the execution policy, and run the first execution turn.
async fn approve_plan(
    planning: &mut bool,
    plan_path: &mut Option<PathBuf>,
    mode: &str,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
) -> io::Result<()> {
    if !*planning {
        println!("not planning; start with `/plan <goal>`.");
        return Ok(());
    }
    let Some(path) = plan_path.clone() else {
        println!("no plan file to approve.");
        *planning = false;
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
    *planning = false;
    println!(
        "plan approved - {} tasks, execution mode {mode}. Each task runs in a fresh context.",
        tasks.len()
    );
    let mut memory: Vec<String> = Vec::new();
    let mut escalation = InteractiveEscalation;
    let status = run_task_loop(
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

fn print_repl_help() {
    println!("Available commands:");
    println!("  /help          Show help");
    println!("  /status        Show session status");
    println!("  /model [NAME]  Show or switch the model");
    println!("  /mode [NAME]   Show or switch the permission mode");
    println!("  /plan <GOAL>   Plan first (writes gated to .heartflow/plans), then /plan approve to execute");
    println!("  /compact       Compact session history");
    println!("  /pin           Toggle never-compacted on the last message (survives /compact)");
    println!("  /save          Persist the session now");
    println!("  /clear         Start a fresh session");
    println!("  /sessions      List saved sessions");
    println!("  /open <N>      Jump the live REPL back to saved session N");
    println!("  /remember <T>  Persist a durable pitfall/preference to MEMORY.md");
    println!("  /search <Q>    Full-text search saved conversation history");
    println!("  /mcp           List connected MCP servers and tools");
    println!("  /expand [ID]   Re-show a folded tool output (default: latest)");
    println!("  /queue [pop|clear]  Inspect/withdraw follow-ups queued during a turn");
    println!("  /guide <TASK>  Assemble a prior-work/current-state/task draft to send");
    println!(
        "  !<CMD>         Run a shell command directly (no model); output folds like a tool result"
    );
    println!("  /init          Scaffold a starting AGENTS.md in the current directory");
    println!("  /restart       Re-launch the program with a fresh config/MCP load");
    println!("  /exit          Quit the REPL");
    println!();
    println!("Input: Enter sends, Alt/Shift+Enter inserts a newline, Up/Down walks history,");
    println!("  Tab completes a slash command, Ctrl+C clears the idle line, Ctrl+D quits.");
    println!("Ctrl+C while a turn is running interrupts that turn; on an empty prompt");
    println!("it just clears the line. Quit with /exit or Ctrl+D.");
}

/// Render a tool result, folding outputs longer than `fold_lines` to a head
/// preview. The full text is stashed in `EXPANDABLE` and reachable via
/// `/expand <id>`; short results render whole.
fn fold_tool_output(name: &str, output: &str, fold_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= fold_lines {
        return format!("### Tool `{name}`\n\n```text\n{output}\n```\n");
    }
    let id = {
        let mut log = EXPANDABLE.lock().expect("expandable log poisoned");
        log.push(output.to_string());
        log.len()
    };
    let head = lines[..fold_lines].join("\n");
    format!(
        "### Tool `{name}`\n\n```text\n{head}\n```\n\n_\u{2026} {} more lines folded \u{2014} run `/expand {id}` to show._\n",
        lines.len() - fold_lines
    )
}

/// Handle `/expand [ID]`: reprint a folded tool output (default: the latest).
fn handle_expand_command(input: &str) {
    let log = EXPANDABLE.lock().expect("expandable log poisoned");
    if log.is_empty() {
        println!("nothing to expand yet.");
        return;
    }
    let requested = input
        .strip_prefix("/expand")
        .map(str::trim)
        .filter(|arg| !arg.is_empty());
    let index = match requested {
        Some(arg) => match arg.parse::<usize>() {
            Ok(n) if (1..=log.len()).contains(&n) => n - 1,
            _ => {
                println!("no folded output #{arg} (valid: 1..{}).", log.len());
                return;
            }
        },
        None => log.len() - 1,
    };
    println!("{}", log[index]);
}

/// Search saved conversation history (the SQLite store) for a text query.
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
fn format_search_hits(hits: &[SearchHit]) -> Vec<String> {
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

/// Compose the final prompt from a CLI instruction and piped stdin, Unix-style:
/// instruction + stdin data become "instruction\n\n<data>"; stdin alone is used
/// verbatim; an instruction with no stdin is unchanged.
fn compose_prompt(instruction: &str, stdin_data: Option<&str>) -> String {
    let instr = instruction.trim();
    let data = stdin_data.map(str::trim).filter(|s| !s.is_empty());
    match (instr.is_empty(), data) {
        (false, Some(d)) => format!("{instr}\n\n{d}"),
        (false, None) => instr.to_string(),
        (true, Some(d)) => d.to_string(),
        (true, None) => String::new(),
    }
}

/// Read all of stdin when it is piped/redirected (not an interactive terminal).
/// Returns `None` for a TTY or empty input, so interactive runs never block.
fn read_piped_stdin() -> Option<String> {
    if io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    match io::stdin().read_to_string(&mut buffer) {
        Ok(_) if !buffer.trim().is_empty() => Some(buffer),
        _ => None,
    }
}

/// Paths named by `@mention` in one line. A bare mention runs to the next space;
/// a quoted one (`@"my shot.png"`) may contain spaces, which paths routinely do.
/// Only a mention starting at a word boundary counts, so `mail@example.com` is
/// left alone.
fn mention_paths(text: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    for (index, _) in text.match_indices('@') {
        if text[..index]
            .chars()
            .next_back()
            .is_some_and(|ch| !ch.is_whitespace())
        {
            continue;
        }
        let rest = &text[index + 1..];
        let candidate = match rest.strip_prefix('"') {
            Some(quoted) => match quoted.find('"') {
                Some(end) => &quoted[..end],
                None => continue,
            },
            None => &rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())],
        };
        let path = candidate
            .trim_matches('\'')
            .trim_end_matches([',', '.', ';', ':', ')', ']', '}']);
        if !path.is_empty() {
            paths.push(path);
        }
    }
    paths
}

/// Turn the typed line into content blocks, promoting every `@path` that names
/// a supported image into an attachment. The text is kept verbatim so the model
/// can still tell which file each image came from; an unreadable attachment is
/// reported and skipped instead of failing the turn.
fn expand_attachments(text: &str) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::Text {
        text: text.to_string(),
    }];
    let mut paths: Vec<&str> = Vec::new();
    for candidate in mention_paths(text) {
        if tools::attachment_media_type(candidate).is_none() || paths.contains(&candidate) {
            continue;
        }
        paths.push(candidate);
    }
    for path in paths {
        match tools::read_image_attachment(path) {
            Ok(block) => {
                println!("· attached {path}");
                blocks.push(block);
            }
            Err(error) => eprintln!("skipped {path}: {error}"),
        }
    }
    blocks
}

/// Drive one turn silently and return the final assistant text plus its usage.
/// Used by `--quiet`/`--json` so scripts receive only the answer.
async fn run_turn_capture(
    runtime: &mut AgentRuntime,
    input: &str,
) -> Result<(String, Option<TokenUsage>), Box<dyn std::error::Error>> {
    let cancel = CancellationToken::new();
    let mut sink = |_event: &AgentEvent| {};
    if let Err(error) = runtime
        .run_turn_with_blocks(expand_attachments(input), None, &mut sink, &cancel)
        .await
    {
        return Err(error.to_string().into());
    }
    let session = runtime.session();
    let last_assistant = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant);
    let text = last_assistant
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let usage = last_assistant.and_then(|message| message.usage);
    Ok((text, usage))
}

/// Serialize a captured turn to a stable JSON object for scripting.
fn turn_json(text: &str, usage: Option<&TokenUsage>, saved: Option<&Path>) -> String {
    let session_id = saved
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned());
    let value = serde_json::json!({
        "text": text,
        "usage": usage.map(|u| serde_json::json!({
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "cache_creation_input_tokens": u.cache_creation_input_tokens,
            "cache_read_input_tokens": u.cache_read_input_tokens,
            "total_tokens": u.total_tokens(),
        })),
        "session_id": session_id,
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Non-interactive history search over the store; `--json` emits an array.
fn run_search(query: &str, limit: i64, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let path = store_path();
    if !path.exists() {
        eprintln!("no conversation history yet (finish a turn or run /save to create it).");
        process::exit(1);
    }
    let hits = Store::open_read_only(&path)?.search(query, None, limit)?;
    if json {
        let array: Vec<serde_json::Value> = hits.iter().map(search_hit_json).collect();
        println!("{}", serde_json::to_string(&array)?);
    } else if hits.is_empty() {
        println!("no matches for {query:?}.");
    } else {
        for line in format_search_hits(&hits) {
            println!("{line}");
        }
    }
    Ok(())
}

fn search_hit_json(hit: &SearchHit) -> serde_json::Value {
    serde_json::json!({
        "session_id": hit.session_id,
        "seq": hit.seq,
        "role": role_str(hit.role),
        "snippet": hit.snippet,
        "method": match hit.method {
            SearchMethod::Fts => "fts",
            SearchMethod::Like => "like",
        },
    })
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

/// Terminal-side rendering state for one interactive turn.
struct TurnRenderer {
    spinner: Spinner,
    theme: ColorTheme,
    renderer: TerminalRenderer,
    spinner_active: bool,
    saw_text: bool,
    last_usage: Option<TokenUsage>,
    /// Accumulated assistant text for the current message segment; rendered
    /// as finished markdown on `MessageStop`.
    assistant_text: String,
}

impl TurnRenderer {
    fn new() -> Self {
        let renderer = TerminalRenderer::new();
        let theme = *renderer.color_theme();
        Self {
            spinner: Spinner::new(),
            theme,
            renderer,
            spinner_active: true,
            saw_text: false,
            last_usage: None,
            assistant_text: String::new(),
        }
    }

    fn render(&mut self, event: &AgentEvent) {
        let mut out = io::stdout();
        match event {
            AgentEvent::TextDelta(delta) => {
                if self.spinner_active {
                    self.spinner.finish("Response", &self.theme, &mut out).ok();
                    self.spinner_active = false;
                }
                self.saw_text = true;
                self.assistant_text.push_str(delta.as_ref());
                print!("{}", delta.as_str().with(self.theme.muted()));
                out.flush().ok();
            }
            AgentEvent::ThinkingDelta(delta) => {
                print!("{}", delta.as_str().with(self.theme.muted()).italic());
                io::stdout().flush().ok();
            }
            AgentEvent::ToolUse { name, .. } => {
                if self.spinner_active {
                    self.spinner
                        .tick(&format!("Running `{name}`"), &self.theme, &mut out)
                        .ok();
                } else {
                    println!("{}", format!("· running `{name}`").with(self.theme.muted()));
                }
            }
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
                ..
            } => {
                let label = if *is_error {
                    format!("`{name}` failed")
                } else {
                    format!("`{name}` done")
                };
                if self.spinner_active {
                    self.spinner.finish(&label, &self.theme, &mut out).ok();
                    self.spinner_active = false;
                } else {
                    println!("{}", format!("· {label}").with(self.theme.muted()));
                }
                let markdown = fold_tool_output(name, output, FOLD_TOOL_OUTPUT_LINES);
                writeln!(out, "{}", self.renderer.render_markdown(&markdown)).ok();
                out.flush().ok();
            }
            AgentEvent::Usage(usage) => {
                self.last_usage = Some(*usage);
            }
            AgentEvent::MessageStop => {
                if !self.assistant_text.is_empty() {
                    writeln!(out).ok();
                    writeln!(
                        out,
                        "{}",
                        self.renderer.render_markdown(&self.assistant_text)
                    )
                    .ok();
                    self.assistant_text.clear();
                    out.flush().ok();
                }
            }
            AgentEvent::Truncated(reason) => {
                if self.spinner_active {
                    self.spinner.finish("Truncated", &self.theme, &mut out).ok();
                    self.spinner_active = false;
                }
                writeln!(
                    out,
                    "{}",
                    format!("· response truncated by the provider ({reason})")
                        .with(self.theme.muted())
                )
                .ok();
                out.flush().ok();
            }
            AgentEvent::Error(_) => {}
        }
    }
}

/// Deterministic signals the task-loop verifier needs from one interactive
/// turn: whether the runtime errored, how many tool results came back as
/// errors, and a short tail of the assistant's final message for memory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnOutcome {
    ok: bool,
    tool_errors: usize,
    conclusion: Option<String>,
}

/// Count tool results flagged as errors across a turn's transcript entries.
fn count_tool_errors(results: &[ConversationMessage]) -> usize {
    results
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
        .count()
}

/// The assistant's final text, truncated for a high-density memory line.
fn last_assistant_conclusion(messages: &[ConversationMessage]) -> Option<String> {
    let text = messages
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
                .join(" ")
        })
        .map(|joined| joined.trim().to_string())
        .filter(|joined| !joined.is_empty())?;
    Some(truncate_chars(&text, 240))
}

/// Run one interactive turn: stream events to the terminal, abort on Ctrl+C,
/// and auto-save the session afterwards.
///
/// Returns a [`TurnOutcome`] the task loop verifies against; the failure is
/// rendered here so callers can keep running.
async fn run_turn_interactive(
    runtime: &mut AgentRuntime,
    input_text: &str,
    prompter: Option<&mut dyn PermissionPrompter>,
) -> io::Result<TurnOutcome> {
    let cancel = CancellationToken::new();
    let listener = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        }
    });

    let mut turn = TurnRenderer::new();
    let mut stdout = io::stdout();
    turn.spinner.tick("Thinking", &turn.theme, &mut stdout)?;

    let mut notify = |event: &AgentEvent| turn.render(event);
    let result = runtime
        .run_turn_with_blocks(
            expand_attachments(input_text),
            prompter,
            &mut notify,
            &cancel,
        )
        .await;
    listener.abort();

    let outcome = match result {
        Ok(summary) => {
            if turn.saw_text {
                writeln!(stdout)?;
            } else {
                turn.spinner.finish("Done", &turn.theme, &mut stdout)?;
            }
            TurnOutcome {
                ok: true,
                tool_errors: count_tool_errors(&summary.tool_results),
                conclusion: last_assistant_conclusion(&summary.assistant_messages),
            }
        }
        Err(error) => {
            let interrupted = error.to_string().contains("cancelled");
            if turn.spinner_active {
                if interrupted {
                    turn.spinner
                        .cancel("Interrupted", &turn.theme, &mut stdout)?;
                } else {
                    turn.spinner.fail("Turn failed", &turn.theme, &mut stdout)?;
                }
            } else {
                writeln!(stdout)?;
            }
            if interrupted {
                println!(
                    "{}",
                    "turn interrupted; the transcript stays consistent - give the next instruction or /exit to quit"
                        .dark_grey()
                );
            } else {
                println!("{}", format!("✘ {error}").red());
            }
            if let Ok(path) = save_session(runtime.session()) {
                println!(
                    "{}",
                    format!("· session saved to {}", path.display()).dark_grey()
                );
            }
            return Ok(TurnOutcome {
                ok: false,
                tool_errors: 0,
                conclusion: None,
            });
        }
    };

    if let Some(usage) = turn.last_usage {
        println!(
            "{}",
            format!(
                "[in {} / out {} / cache read {}]",
                usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
            )
            .dark_grey()
        );
    }
    if let Ok(path) = save_session(runtime.session()) {
        println!("{}", format!("· saved {}", path.display()).dark_grey());
    }
    Ok(outcome)
}

fn build_runtime(
    session: Session,
    selection: ProviderSelection,
    interactive: bool,
    mode: &str,
) -> Result<AgentRuntime, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let (os_name, os_version) = os_platform();
    let system_prompt =
        load_system_prompt(cwd.clone(), home_dir(), current_date(), os_name, os_version)?;
    let compaction = compaction_config(&selection);
    // Remember the concrete credential values so redaction can scrub them from
    // disk even when they match no structural secret pattern.
    match &selection {
        ProviderSelection::Env { .. } => {
            if let Ok(key) = env::var("ANTHROPIC_API_KEY") {
                register_secret(&key);
            }
        }
        ProviderSelection::Profile(profile) => {
            register_secret(&profile.api_key);
            if let Some(token) = &profile.auth_token {
                register_secret(token);
            }
        }
    }
    let client = match selection {
        ProviderSelection::Env { model } => {
            TransportClient::Anthropic(AnthropicStreamClient::from_env(model, true)?)
        }
        ProviderSelection::Profile(profile) => TransportClient::from_profile(&profile, true),
    };
    let mcp = connect_mcp_servers(&cwd, &home_dir());
    let mcp_read_only = mcp.read_only_tool_names();
    let questioner: Option<Arc<dyn UserQuestioner>> =
        interactive.then(|| Arc::new(InteractiveQuestioner) as Arc<dyn UserQuestioner>);
    Ok(ConversationRuntime::new(
        session,
        client,
        AgentToolExecutor::new(mcp, questioner),
        permission_policy_for_mode(mode, &mcp_read_only),
        system_prompt,
    )
    .with_compaction(compaction))
}

/// Terminal-backed permission prompt. `allow_all` latches for the rest of
/// the session once the user answers "all".
struct CliPermissionPrompter {
    allow_all: bool,
}

impl CliPermissionPrompter {
    fn new() -> Self {
        Self { allow_all: false }
    }
}

impl PermissionPrompter for CliPermissionPrompter {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
        if self.allow_all {
            return PermissionPromptDecision::Allow;
        }
        let deny = || PermissionPromptDecision::Deny {
            reason: "user denied the tool call".to_string(),
        };
        let preview: String = request.input.chars().take(200).collect();
        let mut stdout = io::stdout();
        let _ = writeln!(stdout, "\npermission requested: {}", request.tool_name);
        let _ = writeln!(stdout, "  {preview}");

        if stdin_is_terminal() {
            let options = vec![
                "Allow once".to_string(),
                "Allow all for this session".to_string(),
                "Deny".to_string(),
            ];
            let chosen = Select::new(&format!("Allow `{}`?", request.tool_name), options).prompt();
            return match chosen.as_deref() {
                Ok("Allow once") => PermissionPromptDecision::Allow,
                Ok("Allow all for this session") => {
                    self.allow_all = true;
                    PermissionPromptDecision::Allow
                }
                _ => deny(),
            };
        }

        // Non-interactive fallback: single-line y/a/n read over a plain stream.
        let _ = write!(stdout, "allow? [y]es / [a]ll / [n]o: ");
        let _ = stdout.flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return PermissionPromptDecision::Deny {
                reason: "stdin unavailable".to_string(),
            };
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => PermissionPromptDecision::Allow,
            "a" | "all" => {
                self.allow_all = true;
                PermissionPromptDecision::Allow
            }
            _ => deny(),
        }
    }
}

/// One option rendered by the `ask_user` tool.
struct QuestionOption {
    label: String,
    description: Option<String>,
}

/// Terminal-backed question flow for the `ask_user` tool.
trait UserQuestioner: Send + Sync {
    fn ask(
        &self,
        question: &str,
        options: &[QuestionOption],
        multi: bool,
    ) -> Result<String, String>;
}

struct InteractiveQuestioner;

impl UserQuestioner for InteractiveQuestioner {
    fn ask(
        &self,
        question: &str,
        options: &[QuestionOption],
        multi: bool,
    ) -> Result<String, String> {
        let answers = if stdin_is_terminal() {
            Self::ask_inquire(question, options, multi)?
        } else {
            Self::ask_manual(question, options, multi)?
        };
        serde_json::to_string(&serde_json::json!({ "answers": answers }))
            .map_err(|error| error.to_string())
    }
}

impl InteractiveQuestioner {
    /// Interactive path backed by `inquire` list/text prompts. A trailing
    /// sentinel lets the user reject the offered options and type freely.
    fn ask_inquire(
        question: &str,
        options: &[QuestionOption],
        multi: bool,
    ) -> Result<Vec<String>, String> {
        const CUSTOM: &str = "Type your own answer";
        let cancelled = || "answer cancelled".to_string();
        let prompt_custom = || Text::new("Your answer").prompt().map_err(|_| cancelled());

        if options.is_empty() {
            let text = Text::new(question).prompt().map_err(|_| cancelled())?;
            return Ok(vec![text]);
        }

        let labels: Vec<String> = options
            .iter()
            .map(|option| match &option.description {
                Some(desc) => format!("{} - {}", option.label, desc),
                None => option.label.clone(),
            })
            .collect();
        let label_at = |display: &str| {
            labels
                .iter()
                .position(|label| label == display)
                .and_then(|index| options.get(index))
                .map_or_else(|| display.to_string(), |option| option.label.clone())
        };

        if multi {
            let mut choices = labels.clone();
            choices.push(CUSTOM.to_string());
            let picked = MultiSelect::new(question, choices)
                .prompt()
                .map_err(|_| cancelled())?;
            let mut answers = Vec::new();
            for choice in &picked {
                if choice == CUSTOM {
                    answers.push(prompt_custom()?);
                } else {
                    answers.push(label_at(choice));
                }
            }
            if answers.is_empty() {
                return Err("no option selected".to_string());
            }
            Ok(answers)
        } else {
            let mut choices = labels.clone();
            choices.push(CUSTOM.to_string());
            let picked = Select::new(question, choices)
                .prompt()
                .map_err(|_| cancelled())?;
            if picked == CUSTOM {
                return Ok(vec![prompt_custom()?]);
            }
            Ok(vec![label_at(&picked)])
        }
    }

    /// Non-interactive fallback: numbered selection or free text on a plain
    /// stream, preserved for piped input and automated runs.
    fn ask_manual(
        question: &str,
        options: &[QuestionOption],
        multi: bool,
    ) -> Result<Vec<String>, String> {
        let mut stdout = io::stdout();
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "? {question}");
        for (index, option) in options.iter().enumerate() {
            match &option.description {
                Some(text) => {
                    let _ = writeln!(stdout, "  {}) {} - {text}", index + 1, option.label);
                }
                None => {
                    let _ = writeln!(stdout, "  {}) {}", index + 1, option.label);
                }
            }
        }
        if multi {
            let _ = write!(
                stdout,
                "select numbers (comma-separated), or type your own answer: "
            );
        } else {
            let _ = write!(
                stdout,
                "select a number, press Enter for 1, or type your own answer: "
            );
        }
        stdout.flush().map_err(|error| error.to_string())?;

        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        let answer = line.trim();
        if answer.is_empty() {
            return Ok(vec![options
                .first()
                .map(|option| option.label.clone())
                .ok_or("no options offered; type an answer")?]);
        }
        if looks_like_selection(answer) && !options.is_empty() {
            let indices: Vec<usize> = answer
                .split(',')
                .filter(|token| !token.trim().is_empty())
                .map(|token| token.trim().parse::<usize>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "invalid selection".to_string())?;
            return Ok(indices
                .into_iter()
                .map(|index| {
                    options
                        .get(index.wrapping_sub(1))
                        .map_or_else(|| index.to_string(), |option| option.label.clone())
                })
                .collect());
        }
        Ok(vec![answer.to_string()])
    }
}

fn looks_like_selection(answer: &str) -> bool {
    !answer.is_empty()
        && answer
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == ' ')
}

struct NativeToolExecutor {
    todo: Arc<TodoLedger>,
    questioner: Option<Arc<dyn UserQuestioner>>,
}

impl NativeToolExecutor {
    fn new(questioner: Option<Arc<dyn UserQuestioner>>) -> Self {
        Self {
            todo: Arc::new(TodoLedger::new()),
            questioner,
        }
    }

    fn run_ask_user(&self, input: &str) -> Result<String, ToolError> {
        let questioner = self.questioner.as_ref().ok_or_else(|| {
            ToolError::new("ask_user requires an interactive session".to_string())
        })?;
        let value: serde_json::Value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let question = value
            .get("question")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::new("ask_user needs a question".to_string()))?;
        let options: Vec<QuestionOption> = value
            .get("options")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| QuestionOption {
                        label: item
                            .get("label")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        description: item
                            .get("description")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let multi = value
            .get("multi")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        questioner
            .ask(question, &options, multi)
            .map_err(ToolError::new)
    }
}

impl ToolExecutor for NativeToolExecutor {
    fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if tool_name == "todo_write" {
            return self.todo.write(input).map_err(ToolError::new);
        }
        if tool_name == "ask_user" {
            return self.run_ask_user(input);
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        tools::execute_tool(tool_name, &value).map_err(ToolError::new)
    }

    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = tools::mvp_tool_specs()
            .into_iter()
            .map(|spec| ToolSpec {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                input_schema: spec.input_schema,
            })
            .collect::<Vec<_>>();
        for spec in [
            todo_tool_spec(),
            tools::ask_user_tool_spec(),
            tools::verify_graphics_tool_spec(),
            tools::web_fetch_tool_spec(),
            tools::web_search_tool_spec(),
            tools::generate_image_tool_spec(),
            // Document search needs the external rga binary; advertise the
            // tool only when it exists so the model never sees a dead entry.
        ]
        .into_iter()
        .chain(if runtime::rga_available() {
            vec![tools::search_documents_tool_spec()]
        } else {
            Vec::new()
        }) {
            specs.push(ToolSpec {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                input_schema: spec.input_schema,
            });
        }
        specs
    }

    fn pending_tasks(&self) -> usize {
        self.todo.pending_tasks()
    }

    fn is_concurrent_safe(&self, tool_name: &str) -> bool {
        // Pure reads, searches and a network fetch mutate no workspace state, so
        // several may overlap. bash and write/edit/patch/generate_image touch the
        // workspace, todo_write mutates the shared ledger, and ask_user needs the
        // terminal; each must run alone.
        matches!(
            tool_name,
            "read_file"
                | "glob_search"
                | "grep_search"
                | "search_files"
                | "search_documents"
                | "verify_graphics"
                | "web_fetch"
                | "web_search"
        )
    }

    fn seed_plan(&self, input: &str) -> Result<String, ToolError> {
        self.todo.write(input).map_err(ToolError::new)
    }
}

/// One connected MCP server with its advertised tools.
struct McpServerTools {
    name: String,
    client: McpClient,
    tools: Vec<McpTool>,
    /// `[mcp.servers.NAME] read_only = true`: force every tool here to be
    /// treated as read-only, overriding the server's own annotations.
    config_read_only: bool,
}

/// MCP tool namespace: every tool is exposed as `mcp__<server>__<tool>` so
/// native tools keep precedence and names stay collision-free.
struct McpToolset {
    servers: Vec<McpServerTools>,
}

impl McpToolset {
    /// Namespaced names of every MCP tool that is safe to expose in
    /// `read-only`/`plan` modes: those the server annotates `readOnlyHint`, plus
    /// every tool of a server the config marks `read_only`.
    fn read_only_tool_names(&self) -> Vec<String> {
        self.servers
            .iter()
            .flat_map(|server| server.tools.iter().map(move |tool| (server, tool)))
            .filter(|(server, tool)| server.config_read_only || tool.read_only)
            .map(|(server, tool)| format!("mcp__{}__{}", server.name, tool.name))
            .collect()
    }
}

/// Routes tool calls between the native tools and connected MCP servers.
struct AgentToolExecutor {
    native: NativeToolExecutor,
    mcp: McpToolset,
}

impl AgentToolExecutor {
    fn new(mcp: McpToolset, questioner: Option<Arc<dyn UserQuestioner>>) -> Self {
        Self {
            native: NativeToolExecutor::new(questioner),
            mcp,
        }
    }

    /// Shared handle to the plan ledger. The task-loop orchestrator reads this
    /// to verify completion without going through the model.
    fn todo_ledger(&self) -> Arc<TodoLedger> {
        Arc::clone(&self.native.todo)
    }

    /// Namespaced read-only MCP tool names, so a rebuilt permission policy can
    /// keep them usable under `read-only`/`plan` after a mode switch.
    fn mcp_read_only_names(&self) -> Vec<String> {
        self.mcp.read_only_tool_names()
    }

    fn call_mcp(&self, route: &str, input: &str) -> Result<String, ToolError> {
        let (server_name, tool_name) = route
            .split_once("__")
            .ok_or_else(|| ToolError::new(format!("malformed mcp tool name: mcp__{route}")))?;
        let server = self
            .mcp
            .servers
            .iter()
            .find(|server| server.name == server_name)
            .ok_or_else(|| ToolError::new(format!("unknown mcp server: {server_name}")))?;
        let arguments = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let result = server
            .client
            .call_tool(tool_name, &arguments)
            .map_err(|error| ToolError::new(error.to_string()))?;
        if result.is_error {
            Err(ToolError::new(result.text))
        } else {
            Ok(result.text)
        }
    }
}

impl ToolExecutor for AgentToolExecutor {
    fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if let Some(route) = tool_name.strip_prefix("mcp__") {
            return self.call_mcp(route, input);
        }
        self.native.execute(tool_name, input)
    }

    fn pending_tasks(&self) -> usize {
        self.native.pending_tasks()
    }

    fn seed_plan(&self, input: &str) -> Result<String, ToolError> {
        self.native.seed_plan(input)
    }

    fn is_concurrent_safe(&self, tool_name: &str) -> bool {
        if tool_name.starts_with("mcp__") {
            // A namespaced MCP tool overlaps only when it (or its server) is
            // annotated read-only; mutating tools run alone. Reuses the same
            // read-only set the permission policy relies on.
            self.mcp
                .read_only_tool_names()
                .iter()
                .any(|name| name == tool_name)
        } else {
            self.native.is_concurrent_safe(tool_name)
        }
    }

    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.native.specs();
        for server in &self.mcp.servers {
            for tool in &server.tools {
                let mut input_schema = tool.input_schema.clone();
                normalize_tool_schema(&mut input_schema);
                specs.push(ToolSpec {
                    name: format!("mcp__{}__{}", server.name, tool.name),
                    description: format!("[mcp:{}] {}", server.name, tool.description),
                    input_schema,
                });
            }
        }
        specs
    }
}

/// Connect every configured MCP server; failures isolate to one server and
/// never block startup.
fn connect_mcp_servers(cwd: &Path, home: &Path) -> McpToolset {
    let settings = load_merged_mcp(cwd, home);
    let mut servers = Vec::new();
    for (name, config) in settings.servers {
        match connect_mcp_server(&name, &config) {
            Ok(handle) => {
                tracing::debug!(server = %name, tools = handle.tools.len(), "mcp server connected");
                servers.push(handle);
            }
            Err(error) => {
                tracing::warn!(server = %name, error = %error, "mcp server failed; skipped");
            }
        }
    }
    McpToolset { servers }
}

fn connect_mcp_server(name: &str, config: &McpServerConfig) -> Result<McpServerTools, String> {
    // The handshake (`initialize` + `tools/list`) is idempotent, so it is safe
    // to retry with backoff on a transient failure. Tool *calls* are not
    // retried here: they can mutate state, so a failure is surfaced to the
    // model, which decides whether to try again.
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_error = String::from("mcp server connection failed");
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200 * u64::from(attempt)));
        }
        match try_connect_mcp(name, config) {
            Ok(server) => return Ok(server),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn try_connect_mcp(name: &str, config: &McpServerConfig) -> Result<McpServerTools, String> {
    let transport = build_mcp_transport(config)?;
    let client = McpClient::connect(transport).map_err(|error| error.to_string())?;
    let tools = client.list_tools().map_err(|error| error.to_string())?;
    Ok(McpServerTools {
        name: name.to_string(),
        client,
        tools,
        config_read_only: config.read_only,
    })
}

/// Pick the transport from the config: an `url` selects Streamable-HTTP, else
/// spawn the stdio `command`. HTTP header values may reference `${VAR}`.
fn build_mcp_transport(config: &McpServerConfig) -> Result<Box<dyn Transport>, String> {
    if let Some(url) = &config.url {
        let headers = config
            .headers
            .iter()
            .map(|(key, value)| (key.clone(), expand_env_vars(value)))
            .collect();
        let bearer = config
            .bearer_token_env
            .as_ref()
            .and_then(|var| env::var(var).ok())
            .filter(|token| !token.is_empty());
        HttpTransport::new(url.clone(), headers, bearer)
            .map(|transport| Box::new(transport) as Box<dyn Transport>)
            .map_err(|error| error.to_string())
    } else {
        StdioTransport::spawn(&config.command, &config.args, &config.env)
            .map(|transport| Box::new(transport) as Box<dyn Transport>)
            .map_err(|error| error.to_string())
    }
}

/// Substitute every `${NAME}` in `value` with the environment variable, using
/// an empty string when it is unset (so a missing secret yields an empty
/// header rather than a literal placeholder).
fn expand_env_vars(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let name = &after[..end];
            out.push_str(&env::var(name).unwrap_or_default());
            rest = &after[end + 1..];
        } else {
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Default permission mode: from `HEARTFLOW_PERMISSION_MODE`, else
/// `workspace-write` when interactive and `full` for one-shot runs.
fn default_permission_mode(interactive: bool) -> String {
    env::var("HEARTFLOW_PERMISSION_MODE").unwrap_or_else(|_| {
        if interactive {
            "workspace-write".to_string()
        } else {
            "full".to_string()
        }
    })
}

/// Permission resolution: `read-only` allows pure readers, `full`/`auto` run
/// everything without asking, and the default `workspace-write` runs routine
/// commands but confirms only high-blast-radius ones (a `bash` command is
/// auto-allowed unless it looks destructive; `web_fetch` always confirms).
///
/// `mcp_read_only` names (from `McpToolset::read_only_tool_names`) are added to
/// the Allow set for the two Deny-default modes, so read-only MCP tools stay
/// usable in `read-only`/`plan` without opening up write-capable ones.
fn permission_policy_for_mode(mode: &str, mcp_read_only: &[String]) -> PermissionPolicy {
    let base = match mode {
        "read-only" => PermissionPolicy::new(PermissionMode::Deny)
            .with_tool_mode("read_file", PermissionMode::Allow)
            .with_tool_mode("glob_search", PermissionMode::Allow)
            .with_tool_mode("grep_search", PermissionMode::Allow)
            .with_tool_mode("search_files", PermissionMode::Allow)
            .with_tool_mode("search_documents", PermissionMode::Allow)
            .with_tool_mode("todo_write", PermissionMode::Allow)
            .with_tool_mode("verify_graphics", PermissionMode::Allow)
            .with_tool_mode("ask_user", PermissionMode::Allow),
        "full" | "auto" => PermissionPolicy::new(PermissionMode::Allow),
        // Planning: read/research freely, but the only mutation allowed is the
        // plan document itself. Writers route to `Prompt`; the gate auto-allows
        // `.heartflow/plans/*.md` and every other write hits `BlockPrompter`
        // (a hard gate). `bash` and everything else fall to the `Deny` default.
        "plan" => PermissionPolicy::new(PermissionMode::Deny)
            .with_tool_mode("read_file", PermissionMode::Allow)
            .with_tool_mode("glob_search", PermissionMode::Allow)
            .with_tool_mode("grep_search", PermissionMode::Allow)
            .with_tool_mode("search_files", PermissionMode::Allow)
            .with_tool_mode("search_documents", PermissionMode::Allow)
            .with_tool_mode("web_fetch", PermissionMode::Allow)
            .with_tool_mode("web_search", PermissionMode::Allow)
            .with_tool_mode("todo_write", PermissionMode::Allow)
            .with_tool_mode("verify_graphics", PermissionMode::Allow)
            .with_tool_mode("ask_user", PermissionMode::Allow)
            .with_tool_mode("write_file", PermissionMode::Prompt)
            .with_tool_mode("edit_file", PermissionMode::Prompt)
            .with_tool_mode("apply_patch", PermissionMode::Prompt)
            .with_prompt_gate(plan_gate),
        _ => PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_mode("bash", PermissionMode::Prompt)
            // Network egress leaves the sandbox; ask like a dangerous command does.
            .with_tool_mode("web_fetch", PermissionMode::Prompt)
            .with_tool_mode("web_search", PermissionMode::Prompt)
            .with_tool_mode("generate_image", PermissionMode::Prompt)
            .with_prompt_gate(confirm_only_when_risky),
    };
    // The Deny-default modes must still reach read-only MCP tools; Allow modes
    // already permit them, so adding them again is a harmless no-op there.
    match mode {
        "read-only" | "plan" => mcp_read_only.iter().fold(base, |policy, name| {
            policy.with_tool_mode(name.clone(), PermissionMode::Allow)
        }),
        _ => base,
    }
}

/// Swap a live runtime to `mode`, preserving the connected servers' read-only
/// MCP tools. The read-only names are computed before the mutable borrow so the
/// executor can be inspected while the policy is replaced.
fn set_runtime_mode_policy(runtime: &mut AgentRuntime, mode: &str) {
    let mcp_read_only = runtime.executor().mcp_read_only_names();
    runtime.set_permission_policy(permission_policy_for_mode(mode, &mcp_read_only));
}

/// Prompt gate for `workspace-write`: `bash` needs confirmation only when the
/// command looks destructive; every other `Prompt`-mode tool always confirms.
fn confirm_only_when_risky(tool_name: &str, input: &str) -> bool {
    if tool_name != "bash" {
        return true;
    }
    runtime::is_dangerous_command(&input_str_field(input, "command"))
}

/// Read a top-level string field from a tool's JSON input, falling back to the
/// raw input when it is not structured JSON (the model sometimes sends plain
/// text). Shared by the planning gate so path checks mirror the bash checks.
fn input_str_field(input: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|value| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| input.to_string())
}

/// Prompt gate for `plan` mode. Returns `true` when confirmation is required
/// (which `BlockPrompter` then refuses). Only writes to a plan document under
/// `.heartflow/plans/*.md` are auto-allowed and therefore return `false`.
fn plan_gate(_tool_name: &str, input: &str) -> bool {
    !is_plan_doc_input(input)
}

/// Whether a write targets a planning document: a `.md` under `.heartflow/plans/`.
fn is_plan_doc_input(input: &str) -> bool {
    let path = input_str_field(input, "path")
        .replace('\\', "/")
        .to_lowercase();
    path.contains(".heartflow/plans/")
        && std::path::Path::new(&path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Prompter used while planning: any action the plan policy routed to `Prompt`
/// (a write outside the plan document) is refused outright, which is what turns
/// the policy into a hard gate. Plan-document writes never reach here because
/// `plan_gate` auto-allows them.
struct BlockPrompter;

impl PermissionPrompter for BlockPrompter {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
        PermissionPromptDecision::Deny {
            reason: format!(
                "planning mode: `{}` is blocked. Only the plan document under .heartflow/plans/ may be written; get approval with `/plan approve` before making real changes.",
                request.tool_name
            ),
        }
    }
}

/// Create `.heartflow/plans/` under `cwd` and reserve a unique plan path for
/// `goal`. The CLI owns the exact path so the model writes where we can find it.
fn plan_file_path(cwd: &Path, goal: &str) -> Result<PathBuf, String> {
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
fn plan_brief(goal: &str, plan_path: &Path) -> String {
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
fn parse_plan_tasks(markdown: &str) -> Vec<(String, &'static str)> {
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
fn plan_seed_json(tasks: &[(String, &'static str)]) -> String {
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
enum TaskLoopStatus {
    Completed,
    Aborted,
}

/// Max automated attempts per task before the operator is asked.
const MAX_TASK_ATTEMPTS: usize = 3;

/// The escalation the operator chooses after a task exhausts its attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Escalation {
    Retry,
    Skip,
    Abort,
    Reword(String),
}

/// Decides what to do when a task fails every attempt. Kept behind a trait so
/// the loop can be driven by a stub in tests and by the terminal otherwise.
trait EscalationHandler {
    fn handle(&mut self, task_id: &str, task_content: &str) -> Escalation;
}

struct InteractiveEscalation;

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
fn attempt_note(attempt: usize) -> &'static str {
    match attempt {
        0 | 1 => "",
        2 => "PREVIOUS ATTEMPT FAILED. Retry the same approach once; the failure may be transient.\n",
        _ => "ALL PRIOR ATTEMPTS FAILED. Change strategy: pursue a fundamentally different approach.\n",
    }
}

/// Seed JSON for a single focused task, keeping its stable plan id so the
/// ledger, plan checkbox, and reflection line all refer to the same task.
#[must_use]
fn task_seed_json(id: &str, content: &str) -> String {
    serde_json::json!({
        "todos": [{ "id": id, "content": content, "status": "in_progress" }]
    })
    .to_string()
}

/// Kickoff user message for one task on a given attempt.
#[must_use]
fn task_kickoff(id: &str, content: &str, attempt: usize) -> String {
    format!(
        "TASK [{id}]: {content}\n{note}Do only this task now. Your todo list holds just this one item: finish it, verify it against the plan's ## Verification, then mark it `completed` with `todo_write` - the loop cannot advance until you do. Report the concrete result.",
        note = attempt_note(attempt)
    )
}

/// Fresh-context seeds for a task: prior tasks' high-density conclusions, so
/// knowledge carries forward while the raw transcript does not.
#[must_use]
fn build_task_seeds(memory: &[String]) -> Vec<ConversationMessage> {
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
fn task_memory_note(id: &str, content: &str, outcome: &TurnOutcome) -> String {
    let detail = outcome.conclusion.as_deref().unwrap_or("(no text summary)");
    format!("[{id}] {content} -> {detail}")
}

/// Flip the `target_index`-th plan checkbox to checked, mirroring the
/// `parse_plan_tasks` order. Idempotent when already `[x]`.
fn mark_plan_task_done(path: &Path, target_index: usize) -> io::Result<()> {
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
async fn run_task_loop(
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
        if run_one_task(runtime, task, prompter, escalation, memory).await?
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
            note_mirror_rewrite();
            let outcome = run_turn_interactive(
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
fn extract_plan_goal(markdown: &str, fallback: &str) -> String {
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
fn build_reflection_doc(
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
fn write_reflection(
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
fn skill_slug(goal: &str) -> String {
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
fn maybe_sink_skill(
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

#[cfg(test)]
mod tests {
    use super::{
        compose_prompt, expand_attachments, mention_paths, write_agents_skeleton, Action, Cli,
    };
    use clap::Parser;
    use std::path::PathBuf;

    fn action(args: &[&str]) -> Action {
        Cli::try_parse_from(args)
            .expect("args should parse")
            .into_action()
            .expect("action should build")
    }

    #[test]
    fn mirror_append_routing_tracks_transcript_growth() {
        use super::MirrorTracker;
        let tracker = |last_len: usize, force_full: bool| MirrorTracker {
            id: String::from("x"),
            last_len,
            force_full,
        };
        // Nothing mirrored yet: always full-write, never append onto an empty base.
        assert!(!tracker(0, true).can_append(5));
        assert!(!tracker(0, false).can_append(5), "no base to extend");
        // A grown transcript over a trustworthy base extends by append.
        assert!(tracker(5, false).can_append(6));
        // A shrink (compaction) invalidated the prefix: fall back to full rewrite.
        assert!(!tracker(5, false).can_append(3));
        // A forced rewrite (pin/task reset) must full-write even when it grew.
        assert!(
            !tracker(5, true).can_append(8),
            "forced rewrite wins over growth"
        );
    }

    #[test]
    fn anthropic_keeps_a_longer_verbatim_compaction_window() {
        use super::{compaction_profile, ProviderProtocol};
        let anthropic = compaction_profile(Some(ProviderProtocol::Anthropic));
        let openai = compaction_profile(Some(ProviderProtocol::OpenAi));
        let responses = compaction_profile(Some(ProviderProtocol::OpenAiResponses));
        let env = compaction_profile(None);
        assert!(
            anthropic.0 > openai.0 && anthropic.1 > openai.1,
            "anthropic preserves more recent + deeper replay tail"
        );
        // Stateless dialects and env mode share the shorter default profile.
        assert_eq!(openai, responses);
        assert_eq!(openai, env);
    }

    #[test]
    fn secret_registry_dedups_and_ignores_short_values() {
        use super::{register_secret, registered_secrets};
        let before = registered_secrets();
        // A <4-char value is ordinary text and must never be registered.
        register_secret("ab");
        assert_eq!(registered_secrets().len(), before.len());
        // Repeated registration of the same credential collapses to one entry.
        register_secret("sk-supersecret-value-xyz");
        register_secret("sk-supersecret-value-xyz");
        let after = registered_secrets();
        assert_eq!(
            after
                .iter()
                .filter(|value| *value == "sk-supersecret-value-xyz")
                .count(),
            1,
            "credential literals are de-duplicated"
        );
    }

    #[test]
    fn session_identity_adopts_a_transcript_and_rotates_to_a_new_id() {
        use super::{adopt_session_path, current_session_id, rotate_session_id};
        use std::path::Path;
        // Adopting a transcript rebinds persistence to its file stem, so a
        // resumed conversation keeps writing the same file and history row.
        adopt_session_path(Path::new("/x/sessions/123-45.json"));
        assert_eq!(current_session_id(), "123-45");
        // Rotating (after /clear) mints a fresh, non-empty, different id.
        let before = current_session_id();
        rotate_session_id();
        let after = current_session_id();
        assert!(!after.is_empty(), "a fresh id is never empty");
        assert_ne!(before, after, "rotate must start a new conversation id");
    }

    #[test]
    fn native_read_only_tools_are_the_only_concurrent_safe_ones() {
        use super::{NativeToolExecutor, ToolExecutor};
        let exec = NativeToolExecutor::new(None);
        // Pure reads/searches/fetches may overlap.
        for tool in [
            "read_file",
            "glob_search",
            "grep_search",
            "search_files",
            "verify_graphics",
            "web_fetch",
            "web_search",
        ] {
            assert!(
                exec.is_concurrent_safe(tool),
                "{tool} must be concurrent-safe"
            );
        }
        // Anything that mutates the workspace, the ledger, or needs the
        // terminal runs alone.
        for tool in [
            "bash",
            "write_file",
            "edit_file",
            "apply_patch",
            "generate_image",
            "todo_write",
            "ask_user",
        ] {
            assert!(
                !exec.is_concurrent_safe(tool),
                "{tool} must run sequentially"
            );
        }
        // Unknown/fail-closed.
        assert!(!exec.is_concurrent_safe("mcp__server__do_thing"));
    }

    #[test]
    fn iso_date_matches_known_epoch_anchors() {
        use super::format_iso_date;
        assert_eq!(format_iso_date(0), "1970-01-01");
        assert_eq!(format_iso_date(946_684_800), "2000-01-01");
        // Leap day: proves the Gregorian century rules are honored.
        assert_eq!(format_iso_date(1_709_164_800), "2024-02-29");
        // One day before the epoch exercises the negative-side era math.
        assert_eq!(format_iso_date(-86_400), "1969-12-31");
    }

    #[test]
    fn current_date_has_iso_shape() {
        use super::current_date;
        let date = current_date();
        let bytes = date.as_bytes();
        assert_eq!(bytes.len(), 10, "expected YYYY-MM-DD, got {date}");
        assert!(bytes[..4].iter().all(u8::is_ascii_digit));
        assert_eq!(bytes[4], b'-');
        assert!(bytes[5..7].iter().all(u8::is_ascii_digit));
        assert_eq!(bytes[7], b'-');
        assert!(bytes[8..].iter().all(u8::is_ascii_digit));
    }

    #[test]
    fn remember_note_appends_once_and_dedups() {
        use super::{remember_note, DEFAULT_DATE};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos();
        let home = std::env::temp_dir().join(format!("hf-memory-{DEFAULT_DATE}-{nanos}"));

        assert!(remember_note(&home, "use Select-String on pwsh").expect("first write"));
        // Exact duplicate (ignoring surrounding whitespace) is not rewritten.
        assert!(
            !remember_note(&home, "  use Select-String on pwsh  ").expect("dup write"),
            "duplicate must be skipped"
        );
        // A distinct note still appends.
        assert!(remember_note(&home, "--offline is a hard constraint").expect("second"));

        let text = std::fs::read_to_string(home.join(".heartflow/MEMORY.md")).expect("read back");
        assert_eq!(
            text.lines()
                .filter(|l| l.trim() == "- use Select-String on pwsh")
                .count(),
            1
        );
        assert!(text.contains("- --offline is a hard constraint"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn permission_modes_gate_bash_and_writers() {
        use super::permission_policy_for_mode;
        use runtime::PermissionMode;
        // read-only (plan mode): writers and bash denied, readers allowed.
        let plan = permission_policy_for_mode("read-only", &[]);
        assert_eq!(plan.mode_for("bash"), PermissionMode::Deny);
        assert_eq!(plan.mode_for("edit_file"), PermissionMode::Deny);
        assert_eq!(plan.mode_for("read_file"), PermissionMode::Allow);
        // workspace-write: bash prompts, writers allowed.
        let write = permission_policy_for_mode("workspace-write", &[]);
        assert_eq!(write.mode_for("bash"), PermissionMode::Prompt);
        assert_eq!(write.mode_for("edit_file"), PermissionMode::Allow);
        // web_fetch reaches the network: prompts in workspace-write, denied in read-only.
        assert_eq!(write.mode_for("web_fetch"), PermissionMode::Prompt);
        assert_eq!(plan.mode_for("web_fetch"), PermissionMode::Deny);
        // web_search reaches the network too: prompts in workspace-write, denied in read-only.
        assert_eq!(write.mode_for("web_search"), PermissionMode::Prompt);
        assert_eq!(plan.mode_for("web_search"), PermissionMode::Deny);
        // full: everything auto-approves.
        assert_eq!(
            permission_policy_for_mode("full", &[]).mode_for("bash"),
            PermissionMode::Allow
        );
    }

    #[test]
    fn workspace_write_confirms_only_dangerous_bash() {
        use super::permission_policy_for_mode;
        use runtime::PermissionOutcome;
        let write = permission_policy_for_mode("workspace-write", &[]);
        // Routine command auto-runs without prompting (no prompter needed).
        assert!(matches!(
            write.authorize("bash", r#"{"command":"ls -la"}"#, None),
            PermissionOutcome::Allow
        ));
        // Destructive command falls through to interactive approval.
        assert!(matches!(
            write.authorize("bash", r#"{"command":"rm -rf /"}"#, None),
            PermissionOutcome::Deny { .. }
        ));
        // web_fetch always confirms.
        assert!(matches!(
            write.authorize("web_fetch", r#"{"url":"https://x"}"#, None),
            PermissionOutcome::Deny { .. }
        ));
    }

    #[test]
    fn plan_mode_gates_writes_to_the_plan_document() {
        use super::{permission_policy_for_mode, BlockPrompter};
        use runtime::{PermissionMode, PermissionOutcome};
        let plan = permission_policy_for_mode("plan", &[]);
        // Readers and research allowed; bash and unknown actions denied.
        assert_eq!(plan.mode_for("read_file"), PermissionMode::Allow);
        assert_eq!(plan.mode_for("web_fetch"), PermissionMode::Allow);
        assert_eq!(plan.mode_for("bash"), PermissionMode::Deny);
        assert_eq!(plan.mode_for("write_file"), PermissionMode::Prompt);
        // Writing the plan document clears the gate and auto-runs (no prompter).
        assert_eq!(
            plan.authorize(
                "write_file",
                r#"{"path":".heartflow/plans/1-add-auth.md","content":"x"}"#,
                None
            ),
            PermissionOutcome::Allow
        );
        // Windows separators still resolve to a plan document.
        assert_eq!(
            plan.authorize(
                "edit_file",
                r#"{"path":".heartflow\\plans\\plan.md"}"#,
                None
            ),
            PermissionOutcome::Allow
        );
        // Any other write reaches BlockPrompter and is refused (the hard gate).
        assert!(matches!(
            plan.authorize(
                "write_file",
                r#"{"path":"src/main.rs","content":"x"}"#,
                Some(&mut BlockPrompter)
            ),
            PermissionOutcome::Deny { .. }
        ));
        // Non-plan writes with no prompter cannot silently proceed.
        assert!(matches!(
            plan.authorize("write_file", r#"{"path":"README.md"}"#, None),
            PermissionOutcome::Deny { .. }
        ));
    }

    #[test]
    fn read_only_mcp_tools_are_allowed_in_denying_modes() {
        use super::permission_policy_for_mode;
        use runtime::PermissionMode;
        let names = vec!["mcp__db__query".to_string()];
        for mode in ["read-only", "plan"] {
            let policy = permission_policy_for_mode(mode, &names);
            // The advertised read-only MCP tool is reachable...
            assert_eq!(policy.mode_for("mcp__db__query"), PermissionMode::Allow);
            // ...but a write-capable MCP tool on the same server is still denied.
            assert_eq!(policy.mode_for("mcp__db__drop"), PermissionMode::Deny);
        }
        // Allow-default modes ignore the list (everything already permitted).
        let write = permission_policy_for_mode("workspace-write", &names);
        assert_eq!(write.mode_for("mcp__db__drop"), PermissionMode::Allow);
    }

    #[test]
    fn expand_env_vars_substitutes_and_keeps_literals() {
        use super::expand_env_vars;
        std::env::set_var("HF_TEST_TOKEN", "secret");
        assert_eq!(expand_env_vars("Bearer ${HF_TEST_TOKEN}"), "Bearer secret");
        // Unset variables collapse to empty rather than leaking the placeholder.
        assert_eq!(expand_env_vars("x-${HF_TEST_UNSET}y"), "x-y");
        // A dangling `${` without a closing brace is left verbatim.
        assert_eq!(expand_env_vars("100${"), "100${");
        std::env::remove_var("HF_TEST_TOKEN");
    }

    #[test]
    fn parse_plan_tasks_reads_checkboxes() {
        use super::parse_plan_tasks;
        let md = "# Plan\n## Tasks\n- [ ] write the module\n  - [X] already done\n* [ ] star bullet\n- not a task\n## Verification\nplain line\n";
        let tasks = parse_plan_tasks(md);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0], (String::from("write the module"), "pending"));
        assert_eq!(tasks[1], (String::from("already done"), "completed"));
        assert_eq!(tasks[2], (String::from("star bullet"), "pending"));
    }

    #[test]
    fn plan_seed_json_is_valid_todo_input() {
        use super::plan_seed_json;
        let tasks = vec![
            (String::from("a"), "pending"),
            (String::from("b"), "completed"),
        ];
        let value: serde_json::Value =
            serde_json::from_str(&plan_seed_json(&tasks)).expect("valid json");
        assert_eq!(value["todos"][0]["content"], "a");
        assert_eq!(value["todos"][0]["status"], "pending");
        assert_eq!(value["todos"][1]["status"], "completed");
    }

    #[test]
    fn plan_seed_json_carries_ids() {
        use super::plan_seed_json;
        let tasks = vec![
            (String::from("a"), "pending"),
            (String::from("b"), "completed"),
        ];
        let value: serde_json::Value =
            serde_json::from_str(&plan_seed_json(&tasks)).expect("valid json");
        assert_eq!(value["todos"][0]["id"], "t1");
        assert_eq!(value["todos"][1]["id"], "t2");
    }

    #[test]
    fn task_seed_and_kickoff_target_one_focused_task() {
        use super::{attempt_note, task_kickoff, task_seed_json};
        let value: serde_json::Value =
            serde_json::from_str(&task_seed_json("t4", "add auth")).expect("valid json");
        assert_eq!(value["todos"].as_array().expect("array").len(), 1);
        assert_eq!(value["todos"][0]["id"], "t4");
        assert_eq!(value["todos"][0]["status"], "in_progress");

        // Attempt ladder: clean, retry, then change strategy.
        assert_eq!(attempt_note(1), "");
        assert!(attempt_note(2).contains("Retry the same approach"));
        assert!(attempt_note(3).contains("fundamentally different"));
        assert!(task_kickoff("t4", "add auth", 1).contains("TASK [t4]: add auth"));
    }

    #[test]
    fn build_task_seeds_carries_prior_memory() {
        use super::build_task_seeds;
        assert_eq!(build_task_seeds(&[]).len(), 0);
        let seeds = build_task_seeds(&[
            String::from("[t1] scaffold -> done"),
            String::from("[t2] wire -> done"),
        ]);
        assert_eq!(seeds.len(), 1);
        let text = match &seeds[0].blocks[0] {
            runtime::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text seed"),
        };
        assert!(text.contains("[t1] scaffold"));
        assert!(text.contains("[t2] wire"));
    }

    #[test]
    fn turn_outcome_scans_tool_errors_and_conclusion() {
        use super::{count_tool_errors, last_assistant_conclusion, task_memory_note, TurnOutcome};
        use runtime::{ContentBlock, ConversationMessage};

        let errs = vec![
            ConversationMessage::tool_result("1", "bash", "boom", true),
            ConversationMessage::tool_result("2", "read_file", "fine", false),
        ];
        assert_eq!(count_tool_errors(&errs), 1);
        assert_eq!(count_tool_errors(&[]), 0);

        let msgs = vec![
            ConversationMessage::user_text("do it"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "  shipped the endpoint  ".to_string(),
            }]),
        ];
        assert_eq!(
            last_assistant_conclusion(&msgs).as_deref(),
            Some("shipped the endpoint")
        );
        assert_eq!(last_assistant_conclusion(&[]), None);

        let outcome = TurnOutcome {
            ok: true,
            tool_errors: 0,
            conclusion: Some("shipped the endpoint".to_string()),
        };
        assert_eq!(
            task_memory_note("t3", "add auth", &outcome),
            "[t3] add auth -> shipped the endpoint"
        );
    }

    #[test]
    fn mark_plan_task_done_flips_only_the_target() {
        use super::mark_plan_task_done;
        let dir = std::env::temp_dir().join(format!("hf-plan-mark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("plan.md");
        let original = "# Plan\n- [ ] one\n- [x] two\n* [ ] three\n- plain\n";
        std::fs::write(&path, original).expect("write plan");

        mark_plan_task_done(&path, 2).expect("mark index 2 (three)");
        let updated = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            updated,
            "# Plan\n- [ ] one\n- [x] two\n* [x] three\n- plain\n"
        );
        // Idempotent: re-marking the already-checked target leaves it be.
        mark_plan_task_done(&path, 1).expect("mark index 1 (already done)");
        let again = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(again, updated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reflect_doc_lists_each_task_with_status() {
        use super::{build_reflection_doc, extract_plan_goal, skill_slug};
        use runtime::TokenUsage;
        let tasks = vec![
            (String::from("write module"), "pending"),
            (String::from("add tests"), "completed"),
        ];
        let memory = vec![String::from("[t1] write module -> shipped the endpoint")];
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 5,
        };
        let doc = build_reflection_doc(
            "1700ms since epoch (UTC)",
            &PathBuf::from(".heartflow/plans/p.md"),
            "ship feature",
            &tasks,
            &memory,
            &usage,
        );
        assert!(doc.contains("[t1] write module - not completed"));
        assert!(doc.contains("[t2] add tests - completed"));
        assert!(doc.contains("shipped the endpoint"));
        assert!(doc.contains("total 145"));

        // Goal extraction and slug shaping both have deterministic fallbacks.
        assert_eq!(
            extract_plan_goal("## Goal\n  Do the thing \n## Tasks\n- [ ] x", "fb"),
            "Do the thing"
        );
        assert_eq!(extract_plan_goal("no goal here", "fb"), "fb");
        assert_eq!(skill_slug("Add Auth & Tests!"), "add-auth-tests");
        assert_eq!(skill_slug("!!!"), "plan-reflection");
    }

    #[test]
    fn skill_sink_writes_nothing_without_confirmation() {
        use super::maybe_sink_skill;
        let dir = std::env::temp_dir().join(format!("hf-skill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let reflection = dir.join(".heartflow/reflections/1.md");
        let mut confirm = || false;
        let written =
            maybe_sink_skill(&dir, "add auth", &reflection, true, &mut confirm).expect("ok");
        assert!(written.is_none());
        assert!(!dir.join(".agent").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_depth_guard_increments_and_caps() {
        use super::{next_restart_depth, MAX_RESTART_DEPTH};
        assert_eq!(next_restart_depth(None), Some(1));
        assert_eq!(next_restart_depth(Some("2")), Some(3));
        assert_eq!(next_restart_depth(Some("not-a-number")), None);
        assert_eq!(
            next_restart_depth(Some(&MAX_RESTART_DEPTH.to_string())),
            None
        );
    }

    #[test]
    fn folds_long_tool_output_and_keeps_short() {
        use super::fold_tool_output;
        let short = fold_tool_output("bash", "one\ntwo", 40);
        assert!(short.contains("one\ntwo"));
        assert!(!short.contains("/expand"), "short output must not fold");

        let long = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let folded = fold_tool_output("read_file", &long, 40);
        assert!(folded.contains("line 0") && folded.contains("line 39"));
        assert!(!folded.contains("line 49"), "tail must be folded away");
        assert!(folded.contains("/expand"), "fold must advertise /expand");
        assert!(folded.contains("10 more lines folded"));
    }

    #[test]
    fn formats_search_hits_with_role_and_truncated_cjk() {
        use super::format_search_hits;
        use runtime::MessageRole;
        use store::{SearchHit, SearchMethod};

        let long = "汉".repeat(200);
        let hits = vec![SearchHit {
            session_id: "sess-1".to_string(),
            seq: 3,
            role: MessageRole::User,
            snippet: long,
            method: SearchMethod::Like,
        }];
        let lines = format_search_hits(&hits);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line.contains("[sess-1 #3] user:"), "got: {line}");
        // Snippet trimmed to 120 code points, never splitting a multi-byte char.
        assert!(line.contains(&"汉".repeat(120)), "should keep 120 chars");
        assert!(!line.contains(&"汉".repeat(121)), "must truncate to 120");
    }

    #[test]
    fn defaults_to_repl_when_no_args() {
        assert_eq!(
            action(&["hf"]),
            Action::Repl {
                provider: None,
                model: None,
            }
        );
    }

    #[test]
    fn parses_provider_and_model_flags() {
        assert_eq!(
            action(&["hf", "--provider=deepseek", "--model", "deepseek-v4-pro"]),
            Action::Repl {
                provider: Some("deepseek".to_string()),
                model: Some("deepseek-v4-pro".to_string()),
            }
        );
    }

    #[test]
    fn parses_doctor_subcommand_and_fix_flag() {
        assert_eq!(
            action(&["hf", "doctor"]),
            Action::Doctor {
                fix: false,
                ai: false
            }
        );
        assert_eq!(
            action(&["hf", "doctor", "--fix"]),
            Action::Doctor {
                fix: true,
                ai: false
            }
        );
        assert_eq!(
            action(&["hf", "doctor", "--ai"]),
            Action::Doctor {
                fix: false,
                ai: true
            }
        );
    }

    #[test]
    fn doctor_repair_prompt_lists_each_problem() {
        use super::doctor_repair_prompt;
        assert_eq!(doctor_repair_prompt(&[]), "");
        let prompt = doctor_repair_prompt(&[
            "provider: api key missing".to_string(),
            "/x is not valid TOML".to_string(),
        ]);
        assert!(prompt.contains("provider: api key missing"));
        assert!(prompt.contains("- /x is not valid TOML"));
    }

    #[test]
    fn parses_prompt_subcommand() {
        assert_eq!(
            action(&["hf", "prompt", "hello", "world"]),
            Action::Prompt {
                instruction: "hello world".to_string(),
                provider: None,
                model: None,
                quiet: false,
                json: false,
            }
        );
    }

    #[test]
    fn parses_prompt_quiet_and_json_flags() {
        assert_eq!(
            action(&["hf", "prompt", "summarize", "--quiet"]),
            Action::Prompt {
                instruction: "summarize".to_string(),
                provider: None,
                model: None,
                quiet: true,
                json: false,
            }
        );
        assert_eq!(
            action(&["hf", "prompt", "x", "--json"]),
            Action::Prompt {
                instruction: "x".to_string(),
                provider: None,
                model: None,
                quiet: false,
                json: true,
            }
        );
    }

    #[test]
    fn parses_search_subcommand() {
        assert_eq!(
            action(&["hf", "search", "中文", "笔记", "--limit", "5", "--json"]),
            Action::Search {
                query: "中文 笔记".to_string(),
                limit: 5,
                json: true,
            }
        );
        assert_eq!(
            action(&["hf", "search", "with"]),
            Action::Search {
                query: "with".to_string(),
                limit: 20,
                json: false,
            }
        );
        assert!(Cli::try_parse_from(["hf", "search", "  "])
            .unwrap()
            .into_action()
            .is_err());
    }

    #[test]
    fn parses_init_subcommand() {
        assert_eq!(action(&["hf", "init"]), Action::Init { force: false });
        assert_eq!(
            action(&["hf", "init", "--force"]),
            Action::Init { force: true }
        );
    }

    #[test]
    fn parses_models_subcommand_with_global_flags() {
        assert_eq!(
            action(&["hf", "models"]),
            Action::Models {
                provider: None,
                model: None,
                balance: false
            }
        );
        assert_eq!(
            action(&[
                "hf",
                "--provider",
                "deepseek",
                "--model",
                "deepseek-chat",
                "models",
                "--balance"
            ]),
            Action::Models {
                provider: Some("deepseek".to_string()),
                model: Some("deepseek-chat".to_string()),
                balance: true
            }
        );
    }

    #[test]
    fn parses_resume_flag_forms() {
        // Bare `--resume` -> interactive picker, no path.
        assert_eq!(
            action(&["hf", "--resume"]),
            Action::ResumeSession {
                session_path: None,
                command: None,
                provider: None,
                model: None,
            }
        );
        // `--resume=PATH --run CMD` is the single-flag carried form.
        assert_eq!(
            action(&["hf", "--resume=s.json", "--run", "/compact"]),
            Action::ResumeSession {
                session_path: Some(PathBuf::from("s.json")),
                command: Some("/compact".to_string()),
                provider: None,
                model: None,
            }
        );
        // A bare flag must not swallow a following subcommand token.
        assert!(Cli::try_parse_from(["hf", "--resume", "prompt", "hi"]).is_ok());
        // --run is meaningless without --resume.
        assert!(Cli::try_parse_from(["hf", "--run", "/compact"]).is_err());
        // Combining resume with a subcommand is rejected at fold time.
        assert!(Cli::try_parse_from(["hf", "--resume", "chat"])
            .expect("parses")
            .into_action()
            .is_err());
    }

    #[test]
    fn write_agents_skeleton_creates_then_refuses_overwrite() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("hf-init-{nanos}"));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("Cargo.toml"), "[package]").expect("marker");

        let first = write_agents_skeleton(&dir, false).expect("write");
        assert!(first.contains("wrote AGENTS.md"), "got {first}");
        let body = std::fs::read_to_string(dir.join("AGENTS.md")).expect("read");
        assert!(body.starts_with("# AGENTS.md"));
        assert!(
            body.contains("Rust (cargo)"),
            "toolchain should be detected"
        );

        // Without force a second run must not clobber an edited file.
        std::fs::write(dir.join("AGENTS.md"), "hand edited").expect("edit");
        let second = write_agents_skeleton(&dir, false).expect("second");
        assert!(second.contains("already exists"), "got {second}");
        assert_eq!(
            std::fs::read_to_string(dir.join("AGENTS.md")).expect("read"),
            "hand edited"
        );

        // Force overwrites with a fresh skeleton.
        write_agents_skeleton(&dir, true).expect("force");
        assert!(std::fs::read_to_string(dir.join("AGENTS.md"))
            .expect("read")
            .starts_with("# AGENTS.md"));

        std::fs::remove_dir_all(dir).expect("cleanup temp dir");
    }

    #[test]
    fn compose_prompt_handles_instruction_stdin_and_pipe_only() {
        assert_eq!(compose_prompt("review", None), "review");
        assert_eq!(compose_prompt("review", Some("  \n  ")), "review");
        assert_eq!(
            compose_prompt("review", Some("diff body\n")),
            "review\n\ndiff body"
        );
        assert_eq!(compose_prompt("", Some("piped text")), "piped text");
        assert_eq!(compose_prompt("", None), "");
    }

    #[test]
    fn parses_config_export_and_import() {
        assert_eq!(
            action(&["hf", "config", "export"]),
            Action::Config {
                action: super::ConfigAction::Export { output: None },
            }
        );
        assert_eq!(
            action(&["hf", "config", "export", "--output=out.toml"]),
            Action::Config {
                action: super::ConfigAction::Export {
                    output: Some(PathBuf::from("out.toml")),
                },
            }
        );
        assert_eq!(
            action(&["hf", "config", "import", "incoming.toml"]),
            Action::Config {
                action: super::ConfigAction::Import {
                    path: PathBuf::from("incoming.toml"),
                },
            }
        );
    }

    #[test]
    fn parses_system_prompt_options() {
        assert_eq!(
            action(&[
                "hf",
                "system-prompt",
                "--cwd",
                "/tmp/project",
                "--date",
                "2026-04-01",
            ]),
            Action::PrintSystemPrompt {
                cwd: PathBuf::from("/tmp/project"),
                date: "2026-04-01".to_string(),
            }
        );
    }

    #[test]
    fn parses_resume_flag_with_slash_command() {
        assert_eq!(
            action(&["hf", "--resume=session.json", "--run", "/compact"]),
            Action::ResumeSession {
                session_path: Some(PathBuf::from("session.json")),
                command: Some("/compact".to_string()),
                provider: None,
                model: None,
            }
        );
        assert_eq!(
            action(&["hf", "--resume=session.json"]),
            Action::ResumeSession {
                session_path: Some(PathBuf::from("session.json")),
                command: None,
                provider: None,
                model: None,
            }
        );
    }

    #[test]
    fn bare_resume_defers_to_session_picker() {
        assert_eq!(
            action(&["hf", "--resume"]),
            Action::ResumeSession {
                session_path: None,
                command: None,
                provider: None,
                model: None,
            }
        );
    }

    #[test]
    fn mention_paths_tolerate_quotes_and_trailing_punctuation() {
        assert_eq!(mention_paths("@shot.png"), vec!["shot.png"]);
        assert_eq!(mention_paths("@\"my shot.png\""), vec!["my shot.png"]);
        assert_eq!(mention_paths("@a/b/c.gif,"), vec!["a/b/c.gif"]);
        assert_eq!(mention_paths("@mock(1).webp"), vec!["mock(1).webp"]);
        assert_eq!(mention_paths("@'shot.png'"), vec!["shot.png"]);
        // Mid-word `@` is an address, not a mention; empty mentions vanish.
        assert!(mention_paths("mail@example.com").is_empty());
        assert!(mention_paths("@").is_empty());
        assert!(mention_paths("@'").is_empty());
        // An unclosed quote names nothing rather than swallowing the line.
        assert!(mention_paths("@\"unclosed.png").is_empty());
    }

    #[test]
    fn expand_attachments_reads_only_supported_images() {
        let dir = std::env::temp_dir().join(format!("hf-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let png = dir.join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\npayload").expect("write png");
        let notes = dir.join("notes.md");
        std::fs::write(&notes, "plain text").expect("write md");

        // Quoted because the temp path contains a space; the same file mentioned
        // twice attaches once, and non-image mentions stay plain text.
        let text = format!(
            "compare @\"{0}\" and @\"{0}\" against @\"{1}\"",
            png.display(),
            notes.display()
        );
        let blocks = expand_attachments(&text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], runtime::ContentBlock::Text { text });
        match &blocks[1] {
            runtime::ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "iVBORw0KGgpwYXlsb2Fk");
            }
            other => panic!("expected an image block, got {other:?}"),
        }

        // A mention that does not resolve leaves the turn usable.
        assert_eq!(expand_attachments("@ghost.png").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
