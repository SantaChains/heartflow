mod config;
mod core;
mod doctor;
mod editor;
mod interact;
mod keymap;
mod markdown;
mod mascot;
mod permissions;
mod plan;
mod render;
mod settings;
mod shell;
mod storage;
mod theme;
mod tool_exec;
mod tui;
mod turn;
mod viewport_term;

// mimalloc serves the hot allocation streams (serde_json parsing on every
// SSE chunk, per-tool-call JSON round trips) measurably faster than the
// system allocator on Windows, with no code change beyond this line.
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use api::OpenAiClient;
use clap::Parser;
use crossterm::style::Stylize;
use inquire::Confirm;
use runtime::{
    execute_bash, is_dangerous_command, load_system_prompt, truncate_chars, AgentEvent,
    BashCommandInput, CompactionConfig, ContentBlock, ConversationRuntime, MessageRole, Session,
    TokenUsage, ToolSpec,
};
use store::{role_str, SearchHit, SearchMethod, Store};
use tokio_util::sync::CancellationToken;

use config::{load_merged_mcp, load_provider_selection, ConfigWatcher};
use core::{guide_context, guide_draft, HeartModel};
use provider::{
    load_catalog, load_merged_settings, persist_discovered, persist_model, AnthropicStreamClient,
    CassetteClient, CassetteMode, CatalogModel, ModelSource, ProviderCatalog, ProviderProtocol,
    ProviderSelection, ProviderSettings, TransportClient, CONFIG_VERSION,
};
use render::TerminalRenderer;
// Persistence subsystem (Phase 1 thinning): re-exported crate-wide so existing
// call sites (`home_dir`, `SessionShared`, `save_session_async`, ...) and
// `crate::`/`super::` paths in sibling modules and tests keep resolving. The
// wildcard is deliberate: some items are used only by the test build, so an
// explicit list would trip `unused_imports` in the non-test compilation.
#[allow(clippy::wildcard_imports)]
pub(crate) use storage::*;
// CLI argument surface (Phase 1 thinning): `run` parses `Cli` and dispatches on
// `Action`/`ConfigAction`; the clap subcommand types stay private to `shell`.
pub(crate) use shell::{Action, Cli, ConfigAction, ConfigSurface};
// Interactive terminal primitives (cli-thinning): the blocking REPL's
// permission prompter and `ask_user` question flow, re-exported crate-wide so
// `main.rs` call sites and `super::` test paths keep resolving.
#[allow(clippy::wildcard_imports)]
pub(crate) use interact::*;
// Tool routing + MCP connection glue (cli-thinning): `NativeToolExecutor`,
// `AgentToolExecutor`, `McpToolset` and the connect helpers, re-exported so the
// `AgentRuntime` alias, `build_runtime*` and `super::` test paths keep
// resolving.
#[allow(clippy::wildcard_imports)]
pub(crate) use tool_exec::*;
// Permission policy resolution (cli-thinning): the mode -> policy mapping, the
// prompt gates and the hard-gate `BlockPrompter`, re-exported so `main.rs`
// dispatch sites and `super::` test paths keep resolving.
#[allow(clippy::wildcard_imports)]
pub(crate) use permissions::*;
// Plan mode + Hermes task loop (cli-thinning): the plan-document helpers, the
// fresh-context task orchestrator and the reflection tail, re-exported so the
// `/plan` dispatch sites and `super::` test paths keep resolving.
#[allow(clippy::wildcard_imports)]
pub(crate) use plan::*;
// Interactive turn rendering + driving (cli-thinning): `TurnRenderer`,
// `TurnOutcome` and `run_turn_interactive`, re-exported so the REPL, `plan.rs`,
// `doctor` and `super::` test paths keep resolving.
#[allow(clippy::wildcard_imports)]
pub(crate) use turn::*;
// `hf doctor` diagnostics (cli-thinning): the health checks, FAQ and `--ai`
// repair turn, re-exported so the `run` dispatch site and `super::` test paths
// keep resolving.
#[allow(clippy::wildcard_imports)]
pub(crate) use doctor::*;

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

/// Cap on chained automatic restarts, guarding against a crash-restart loop.
const MAX_RESTART_DEPTH: usize = 5;

/// Tool results longer than this are folded in the terminal; the full text is
/// kept in the session state for `/expand`. Chosen to fit a typical screen.
pub(crate) const FOLD_TOOL_OUTPUT_LINES: usize = 40;

// The transport is wrapped in a cassette so a live session can be recorded and
// replayed offline (`HEARTFLOW_CASSETTE`). Unset, the wrapper is an exact
// passthrough: no relay task, no buffering, no behavioural delta.
pub(crate) type AgentRuntime =
    ConversationRuntime<CassetteClient<TransportClient>, AgentToolExecutor>;

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
    // `run` reports whether `/restart` was confirmed in the REPL: re-exec a
    // fresh process (picking up a replaced binary and a full config/MCP reload)
    // and exit with its status.
    match runtime.block_on(run()) {
        Ok(true) => perform_restart(),
        Ok(false) => {}
        Err(error) => {
            eprintln!("{error}");
            process::exit(1);
        }
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

async fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    // An explicit `-c/--config` file is the highest file layer; record it before
    // any provider resolution so every `resolve_selection` call sees it. Relative
    // paths anchor to the current directory for stable reads and display.
    provider::set_config_override(cli.config.clone().map(|path| {
        if path.is_absolute() {
            path
        } else {
            env::current_dir().map_or_else(|_| path.clone(), |cwd| cwd.join(&path))
        }
    }));
    // One conversation's mutable state (persistence, mirror, folded-output
    // stash); instance-scoped so a future parallel section owns its own.
    let state = new_session_state();
    let mut restart = false;
    match cli.into_action()? {
        Action::PrintSystemPrompt { cwd, date } => print_system_prompt(cwd, date),
        Action::ResumeSession {
            session_path,
            command,
            provider,
            model,
        } => {
            // Bare `-r`/`--resume` restores the most recent session; `list_sessions`
            // is mtime-descending, so its head is the last one touched.
            match session_path.or_else(|| list_sessions().into_iter().next()) {
                Some(session_path) => {
                    if let Some(command) = command {
                        // One-shot slash command run against the saved session, then exit.
                        resume_session(&session_path, &command);
                        return Ok(false);
                    }
                    // No --run: reopen the interactive REPL with the conversation restored.
                    let session = load_saved_session(&session_path)
                        .map_err(|error| format!("failed to restore session: {error}"))?;
                    // Continue this transcript in place: adopt its id so later
                    // turns overwrite the same file and update the same row.
                    adopt_session_path(&state, &session_path);
                    let selection = resolve_selection(provider.as_deref(), model.as_deref())?;
                    println!(
                        "Restored session from {} ({} messages).",
                        session_path.display(),
                        session.messages.len()
                    );
                    restart = run_repl(&state, selection, session).await?;
                }
                None => {
                    if command.is_some() {
                        return Err("no saved session to run --run against".to_string().into());
                    }
                    eprintln!("no saved session to resume; starting a fresh REPL");
                    let selection = resolve_selection(provider.as_deref(), model.as_deref())?;
                    restart = run_repl(&state, selection, Session::new()).await?;
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
                &state,
                Session::new(),
                selection,
                false,
                &default_permission_mode(false),
            )?;
            if quiet || json {
                let (text, usage) = run_turn_capture(&mut runtime, &prompt).await?;
                let saved = save_session_async(&state, runtime.session()).await.ok();
                if json {
                    println!("{}", turn_json(&text, usage.as_ref(), saved.as_deref()));
                } else {
                    println!("{text}");
                }
            } else {
                run_turn_interactive(&state, &mut runtime, &prompt, None).await?;
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
            restart = run_repl(&state, selection, Session::new()).await?;
        }
        Action::Config { action } => match action {
            ConfigAction::Export { surface, output } => export_config(surface, output)?,
            ConfigAction::Import { path } => import_config(&path)?,
        },
        Action::Doctor { fix, ai } => {
            let problems = run_doctor(fix)?;
            if ai {
                doctor_ai_repair(&state, &problems).await?;
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
    Ok(restart)
}

pub(crate) fn resolve_selection(
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<ProviderSelection, String> {
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    load_provider_selection(&cwd, &home_dir(), provider, model)
}

/// Export one merged config surface (no CLI flags, no secret values). `config`
/// is the provider + MCP connection file; `theme`/`keymap`/`settings` are the
/// shell's look/feel/behavior files. Each emits its fully-resolved effective
/// values, so the result is a ready-to-edit template rather than a partial file.
fn export_config(
    surface: ConfigSurface,
    output: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let home = home_dir();
    let text = match surface {
        ConfigSurface::Config => {
            let mut provider = load_merged_settings(&cwd, &home);
            provider.version = Some(CONFIG_VERSION);
            let mut text = provider.to_toml_string();
            text.push_str(&load_merged_mcp(&cwd, &home).to_toml_string());
            text
        }
        ConfigSurface::Theme => theme::Theme::load(&cwd, &home).to_toml_string(),
        ConfigSurface::Keymap => keymap::Keymap::load(&cwd, &home).to_toml_string(),
        ConfigSurface::Settings => settings::Settings::load(&cwd, &home).to_toml_string(),
        // Export emits the full merged catalog (seed + your additions) as a
        // ready-to-edit `provider.toml` template, matching the other surfaces'
        // "fully-resolved effective values" contract.
        ConfigSurface::Provider => load_catalog(&home).to_toml_string(),
    };
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
    let parent = target
        .parent()
        .ok_or_else(|| format!("config path {} has no parent directory", target.display()))?;
    fs::create_dir_all(parent)?;
    backup_existing(&target)?;
    let mut text = settings.to_toml_string();
    text.push_str(&mcp_settings.to_toml_string());
    fs::write(&target, text)?;
    println!("config imported -> {}", target.display());
    Ok(())
}

pub(crate) fn backup_existing(target: &Path) -> std::io::Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let Some(name) = target.file_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("config path {} has no file name", target.display()),
        ));
    };
    let backup = target.with_file_name(format!("{}.bak", name.to_string_lossy()));
    fs::rename(target, &backup)?;
    println!("previous config backed up -> {}", backup.display());
    Ok(())
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
    state: &SessionShared,
    selection: &ProviderSelection,
    mode: &str,
    runtime: &mut AgentRuntime,
    planning: &mut bool,
    plan_path: &mut Option<PathBuf>,
) {
    match build_runtime(state, Session::new(), selection.clone(), true, mode) {
        Ok(fresh) => {
            *runtime = fresh;
            *planning = false;
            *plan_path = None;
            // A cleared session is a new conversation: give it a fresh file and
            // history row instead of appending onto the one just wiped.
            rotate_session_id(state);
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
            // Cache the discovery into the catalog's persisted layer so `/model`
            // and completion can offer these ids next session, filed under the
            // provider that owns this base_url.
            let discovered: Vec<(String, Option<u64>)> = models
                .iter()
                .map(|info| (info.id.clone(), info.context_length.map(u64::from)))
                .collect();
            if let Err(error) = persist_discovered(
                &home_dir(),
                &profile.base_url,
                profile.protocol,
                &discovered,
            ) {
                println!("\n(could not cache discovered models: {error})");
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

pub(crate) fn stdin_is_terminal() -> bool {
    io::stdin().is_terminal()
}

async fn run_repl(
    state: &SessionShared,
    selection: ProviderSelection,
    session: Session,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Phase 2 dual-backend gate: `HEARTFLOW_TUI=1` opts into the full-screen
    // ratatui carrier layer; unset keeps the default blocking-line REPL, which
    // stays the proven fallback while the carrier shell is built out. The TUI
    // owns its own runtime and drives turns on an actor task, so the gate
    // consumes `session`/`selection` and returns early (no restart). The move
    // is flow-sensitive: the diverging branch leaves the fall-through path
    // below still owning them.
    if env::var("HEARTFLOW_TUI").is_ok_and(|v| v == "1") {
        let tui_mode = default_permission_mode(true);
        // Connect every MCP server once and share the toolset across sections:
        // N sections then cost one set of server processes, not N. `McpClient`
        // serializes requests per connection, so the shared handle is correct
        // even once sections run turns concurrently.
        let cwd = env::current_dir()?;
        let mcp = Arc::new(connect_mcp_servers(&cwd, &home_dir()));
        let runtime = build_runtime_with_mcp(
            state,
            session,
            selection.clone(),
            true,
            &tui_mode,
            mcp.clone(),
        )?;
        // Header context for the shell: every section shares this selection, so
        // model/cwd/context-window are captured once here, before `selection`
        // moves into the section factory below. The cwd is home-shortened so the
        // header stays compact on deep trees.
        let home = home_dir();
        let cwd_display = match cwd.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => String::from("~"),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => cwd.display().to_string(),
        };
        let chrome = tui::Chrome {
            model: selection.model().to_string(),
            cwd: cwd_display,
            context_window: resolve_context_window(&selection),
        };
        // Factory for each additional section: a fresh state handle (its own
        // session id + redaction cache) plus a runtime over a new empty session
        // sharing the one MCP toolset, so sections never share conversation
        // data or a save path. It owns the cloned selection/mode/mcp, so it
        // borrows nothing from this scope.
        let mode = tui_mode;
        let make_section =
            move || -> Result<(SessionShared, AgentRuntime), Box<dyn std::error::Error>> {
                let section_state = new_session_state();
                let section_runtime = build_runtime_with_mcp(
                    &section_state,
                    Session::new(),
                    selection.clone(),
                    true,
                    &mode,
                    mcp.clone(),
                )?;
                Ok((section_state, section_runtime))
            };
        tui::run_shell(state, runtime, make_section, chrome).await?;
        return Ok(false);
    }
    let mut selection = selection;
    let mut mode = default_permission_mode(true);
    let cwd = env::current_dir()?;
    let mut watcher = ConfigWatcher::new(&cwd, &home_dir());
    // The provider/model catalog backs `/model` display + completion. Loaded
    // once at startup and hot-reloaded on a `provider.toml` edit below.
    let mut catalog = load_catalog(&home_dir());
    let mut runtime = build_runtime(state, session, selection.clone(), true, &mode)?;
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
    // `/restart` confirmation unwinds the loop and reports upward so `main`
    // re-execs a fresh process; a plain quit leaves this false.
    let mut restart = false;
    mascot::draw_banner(&theme::Theme::current(), &mut io::stdout())?;
    print_repl_preamble();

    loop {
        deliver_queued_injection(
            state,
            &mut runtime,
            &mut model,
            planning,
            &mut prompter,
            &mut editor,
        )
        .await;
        // Ctrl+D / EOF is a documented quit path (see the banner): save the
        // session and print the resume command just like `/exit`, so the
        // conversation is never silently dropped on the EOF path.
        let ctx = build_completion_context(&selection, &catalog, &mode);
        let Some(line) = editor.read_line(&ctx)? else {
            exit_with_resume_hint(state, &runtime).await;
            break;
        };
        // Config edited on disk since the last turn: reload each changed surface
        // through its own path. Provider config re-resolves and rebuilds the
        // runtime (keeping the conversation and current mode); a theme edit just
        // swaps the global palette, which the next turn's renderer picks up.
        // keymap/settings drive the TUI only, so the blocking REPL ignores them.
        let changed = watcher.changed();
        if changed.config {
            reload_config(state, &cwd, &mut selection, &mut runtime, &mode);
            planning = false;
        }
        if changed.theme {
            theme::Theme::reload(&cwd, &home_dir());
        }
        if changed.provider {
            catalog = load_catalog(&home_dir());
        }
        let trimmed = line.trim();
        let control = if trimmed.starts_with('/') {
            dispatch_slash_command(
                state,
                trimmed,
                &mut runtime,
                &mut selection,
                &mut mode,
                &mut planning,
                &mut plan_path,
                &mut model,
                &mut prompter,
                &cwd,
                &catalog,
            )
            .await?
        } else if trimmed.starts_with('!') {
            handle_bang_command(state, &mode, trimmed).await;
            LoopControl::Continue
        } else {
            // Running turns cannot be submitted through the blocking line
            // editor yet (P4-c); keep the queue routing explicit so the
            // ratatui event loop only has to flip `begin_turn`.
            if model.is_running() {
                if !model.enqueue(trimmed) {
                    println!("follow-up queue is full; /queue to inspect");
                }
            } else {
                let outcome = if planning {
                    run_turn_interactive(state, &mut runtime, trimmed, Some(&mut BlockPrompter))
                        .await?
                } else {
                    run_turn_interactive(state, &mut runtime, trimmed, Some(&mut prompter)).await?
                };
                editor.note_turn(outcome.ok);
            }
            maybe_auto_compact(state, &mut runtime);
            LoopControl::Continue
        };
        match control {
            LoopControl::Continue => {}
            LoopControl::Exit => break,
            LoopControl::Restart => {
                restart = true;
                break;
            }
        }
    }
    Ok(restart)
}

/// What the REPL loop should do after one input line is fully handled.
enum LoopControl {
    Continue,
    Exit,
    /// `/restart` confirmed: unwind the REPL and ask `main` to re-exec.
    Restart,
}

/// Banner lines printed once before the loop starts.
fn print_repl_preamble() {
    println!("heartflow interactive mode");
    println!(
        "Input: Enter sends, Alt/Shift+Enter newline, Up/Down history, Tab completes / commands."
    );
    println!("Quit with /exit or Ctrl+D. Ctrl+C interrupts a running turn; on an idle line it just clears it.");
}

/// `/save` from the REPL: persist the live session and report the path or the
/// failure inline (a failed save is a status message, never an abort).
async fn save_and_report(state: &SessionShared, runtime: &AgentRuntime) {
    match save_session_async(state, runtime.session()).await {
        Ok(path) => println!("session saved -> {}", path.display()),
        Err(error) => println!("failed to save session: {error}"),
    }
}

/// Turn boundary: a finished turn's queued follow-ups are merged into one
/// injection message here (locked semantics: never interrupt, always deliver
/// after the turn). A refused/failed injection is re-queued by the caller, so
/// nothing is lost.
async fn deliver_queued_injection(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    model: &mut HeartModel,
    planning: bool,
    prompter: &mut CliPermissionPrompter,
    editor: &mut editor::ReplEditor,
) {
    let Some(injection) = model.queue_mut().drain_injection() else {
        return;
    };
    model.begin_turn();
    let delivery = if planning {
        run_turn_interactive(state, runtime, &injection, Some(&mut BlockPrompter)).await
    } else {
        run_turn_interactive(state, runtime, &injection, Some(prompter)).await
    };
    model.end_turn();
    match delivery {
        Ok(outcome) => {
            maybe_auto_compact(state, runtime);
            editor.note_turn(outcome.ok);
        }
        Err(error) => {
            println!("queued delivery failed: {error}");
            editor.note_turn(false);
            let _ = model.enqueue(&injection);
        }
    }
}

/// Route one `/`-prefixed line. Slash handling is inherently wide: the
/// subcommands touch every piece of REPL state, which is why this takes the
/// state pieces as separate `&mut`s instead of a bundled struct.
#[allow(clippy::too_many_arguments)]
async fn dispatch_slash_command(
    state: &SessionShared,
    trimmed: &str,
    runtime: &mut AgentRuntime,
    selection: &mut ProviderSelection,
    mode: &mut String,
    planning: &mut bool,
    plan_path: &mut Option<PathBuf>,
    model: &mut HeartModel,
    prompter: &mut CliPermissionPrompter,
    cwd: &Path,
    catalog: &ProviderCatalog,
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
            handle_clear_command(state, selection, mode, runtime, planning, plan_path);
            LoopControl::Continue
        }
        "/sessions" => {
            print_sessions_listing();
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
        _ if trimmed == "/guide" || trimmed.starts_with("/guide ") => {
            handle_guide_command(trimmed, runtime);
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
            *planning = false;
            LoopControl::Continue
        }
        _ if trimmed == "/model" || trimmed.starts_with("/model ") => {
            handle_model_command(state, trimmed, selection, runtime, mode, catalog);
            *planning = false;
            LoopControl::Continue
        }
        _ if trimmed == "/plan" || trimmed.starts_with("/plan ") => {
            handle_plan_command(
                state, trimmed, planning, plan_path, cwd, mode, runtime, prompter,
            )
            .await?;
            LoopControl::Continue
        }
        _ => {
            report_unknown_command(trimmed);
            LoopControl::Continue
        }
    })
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
    // Assembly lives in `core` so the ratatui Ctrl+G overlay builds the exact
    // same draft; this command only prints it.
    let context = guide_context(
        &runtime.session().messages,
        runtime.executor().todo_ledger().pending_tasks(),
    );
    let draft = guide_draft(&context, task);
    println!("guide draft (edit and resend, or paste into the next message):\n");
    println!("{draft}\n");
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
async fn handle_bang_command(state: &SessionShared, mode: &str, input: &str) {
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
        let markdown = fold_tool_output(state, "shell", &body, FOLD_TOOL_OUTPUT_LINES);
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
async fn exit_with_resume_hint(state: &SessionShared, runtime: &AgentRuntime) {
    match save_session_async(state, runtime.session()).await {
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
    state: &SessionShared,
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

    let mut next = selection.clone();
    next.set_model(requested);
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
                    requested,
                    profile.context_window.map(u64::from),
                    ModelSource::User,
                ) {
                    println!("(could not record model in catalog: {error})");
                }
            }
        }
        Err(error) => println!("failed to switch model: {error}"),
    }
}

/// `/model` with no argument: report the provider/protocol/base_url that is
/// *actually* active (so a transport failure is never misread as the wrong
/// vendor), the current model, and the catalog's known models for that
/// provider. Replaces the old hardcoded DeepSeek shortlist: the list now tracks
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

/// Cap on session ordinals offered to `/open ` completion: enough to be useful
/// without scanning an unbounded list into the dropdown every turn.
const SESSION_COMPLETION_MAX: usize = 20;

/// Build this turn's completion universe for the editor: the active provider's
/// catalog models (label = id, detail = context window + provenance), the
/// permission modes, recent session ordinals for `/open`, and the live
/// model/mode so the dropdown can tag the current value. Kept out of `run_repl`
/// so that function stays within its length budget, and rebuilt each turn so a
/// hot-reloaded catalog or a just-saved session shows up at once.
fn build_completion_context(
    selection: &ProviderSelection,
    catalog: &ProviderCatalog,
    mode: &str,
) -> editor::CompletionContext {
    let models = match selection {
        ProviderSelection::Profile(profile) => catalog
            .provider_by_host(&profile.base_url)
            .map(|entry| {
                entry
                    .models
                    .iter()
                    .map(|model| editor::CompletionItem::new(model.id.clone(), model_detail(model)))
                    .collect()
            })
            .unwrap_or_default(),
        // The legacy env-Anthropic path has no catalog entry to draw from.
        ProviderSelection::Env { .. } => Vec::new(),
    };
    let modes = KNOWN_PERMISSION_MODES
        .iter()
        .map(|mode| editor::CompletionItem::new(*mode, ""))
        .collect();
    let sessions = list_sessions()
        .iter()
        .take(SESSION_COMPLETION_MAX)
        .enumerate()
        .map(|(index, path)| {
            let detail = path
                .file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
            editor::CompletionItem::new((index + 1).to_string(), detail)
        })
        .collect();
    editor::CompletionContext {
        models,
        modes,
        sessions,
        current_model: Some(selection.model().to_string()),
        current_mode: Some(mode.to_string()),
    }
}

/// The muted detail column for a catalog model: a humanized context window plus
/// its provenance, e.g. `1M ctx · seed`, or just `discovered` when unknown.
fn model_detail(model: &CatalogModel) -> String {
    match model.context_window {
        Some(tokens) => format!(
            "{} ctx · {}",
            humanize_tokens(tokens),
            model.source.as_str()
        ),
        None => model.source.as_str().to_string(),
    }
}

/// Render a token count compactly: exact millions as `NM`, thousands as `NK`,
/// otherwise the raw number. Only for the completion detail column.
fn humanize_tokens(tokens: u64) -> String {
    const M: u64 = 1_000_000;
    const K: u64 = 1_000;
    if tokens >= M && tokens.is_multiple_of(M) {
        format!("{}M", tokens / M)
    } else if tokens >= K {
        format!("{}K", tokens / K)
    } else {
        tokens.to_string()
    }
}

/// Permission modes exposed to `/mode`, ordered from most to least restrictive.
const KNOWN_PERMISSION_MODES: &[&str] = &["read-only", "workspace-write", "full"];

/// Re-read config from disk and rebuild the runtime in place, preserving the
/// live conversation and permission mode. Used for between-turn hot reload.
fn reload_config(
    state: &SessionShared,
    cwd: &Path,
    selection: &mut ProviderSelection,
    runtime: &mut AgentRuntime,
    mode: &str,
) {
    match load_provider_selection(cwd, &home_dir(), None, None) {
        Ok(next) => match build_runtime(state, runtime.session().clone(), next.clone(), true, mode)
        {
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
fn maybe_auto_compact(state: &SessionShared, runtime: &mut AgentRuntime) {
    let config = runtime.compaction();
    if config.context_window_tokens == 0 {
        return;
    }
    if runtime.should_compact(config) {
        let result = runtime.compact(config);
        note_mirror_rewrite(state);
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
fn force_compact(state: &SessionShared, runtime: &mut AgentRuntime) {
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
        note_mirror_rewrite(state);
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
            println!("mode -> {normalized}");
        }
        Err(error) => println!("failed to switch mode: {error}"),
    }
}

/// Handle `/plan [goal|approve|end|status]`. Planning swaps the live runtime
/// to a hard-gated policy (only the plan document is writable) instead of
/// rebuilding it, so the todo ledger and conversation survive the transition
/// into execution.
#[allow(clippy::too_many_arguments)]
async fn handle_plan_command(
    state: &SessionShared,
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
        "approve" => approve_plan(state, planning, plan_path, mode, runtime, prompter).await?,
        goal => start_planning(state, goal, planning, plan_path, cwd, runtime).await?,
    }
    Ok(())
}

/// Begin a planning session: reserve the plan document, engage the gated
/// policy, and run the first research-and-plan turn under `BlockPrompter`.
async fn start_planning(
    state: &SessionShared,
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
    state: &SessionShared,
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
/// preview. The full text is stashed in the session state and reachable via
/// `/expand <id>`; short results render whole.
pub(crate) fn fold_tool_output(
    state: &SessionShared,
    name: &str,
    output: &str,
    fold_lines: usize,
) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= fold_lines {
        return format!("### Tool `{name}`\n\n```text\n{output}\n```\n");
    }
    let id = match state.lock() {
        Ok(mut guard) => {
            guard.expandable.push(output.to_string());
            guard.expandable.len()
        }
        // Poisoned state: render the whole output rather than lose it.
        Err(_) => return format!("### Tool `{name}`\n\n```text\n{output}\n```\n"),
    };
    let head = lines[..fold_lines].join("\n");
    format!(
        "### Tool `{name}`\n\n```text\n{head}\n```\n\n_\u{2026} {} more lines folded \u{2014} run `/expand {id}` to show._\n",
        lines.len() - fold_lines
    )
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
pub(crate) fn expand_attachments(text: &str) -> Vec<ContentBlock> {
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

pub(crate) fn build_runtime(
    state: &SessionShared,
    session: Session,
    selection: ProviderSelection,
    interactive: bool,
    mode: &str,
) -> Result<AgentRuntime, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    // Single-runtime paths own their MCP connections; the multi-section shell
    // connects once and shares one toolset via `build_runtime_with_mcp`.
    let mcp = Arc::new(connect_mcp_servers(&cwd, &home_dir()));
    build_runtime_with_mcp(state, session, selection, interactive, mode, mcp)
}

/// Assemble a runtime over an already-connected, shared MCP toolset. The
/// multi-section shell connects every server once and hands the same `Arc` to
/// each section, so N sections cost one set of MCP server processes rather than
/// N. `McpClient` serializes requests per connection (internal mutex), so the
/// shared handle stays correct even once sections run turns concurrently.
fn build_runtime_with_mcp(
    state: &SessionShared,
    session: Session,
    selection: ProviderSelection,
    interactive: bool,
    mode: &str,
    mcp: Arc<McpToolset>,
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
                register_secret(state, &key);
            }
        }
        ProviderSelection::Profile(profile) => {
            register_secret(state, &profile.api_key);
            if let Some(token) = &profile.auth_token {
                register_secret(state, token);
            }
        }
    }
    // A malformed `HEARTFLOW_CASSETTE` is fatal before any turn runs: silently
    // recording nothing would be worse than a clear startup error.
    let client = match CassetteMode::from_env()? {
        // Replay deliberately does not build a transport. That is what makes a
        // recorded session runnable offline — no endpoint, and no key to exist.
        Some(CassetteMode::Replay(path)) => CassetteClient::replay(path),
        cassette => {
            let live = match selection {
                ProviderSelection::Env { model } => {
                    TransportClient::Anthropic(AnthropicStreamClient::from_env(model, true)?)
                }
                ProviderSelection::Profile(profile) => {
                    TransportClient::from_profile(&profile, true)
                }
            };
            CassetteClient::live(live, cassette.unwrap_or(CassetteMode::Off))
        }
    };
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
        use super::{new_session_state, register_secret, registered_secrets};
        let state = new_session_state();
        let before = registered_secrets(&state);
        // A <4-char value is ordinary text and must never be registered.
        register_secret(&state, "ab");
        assert_eq!(registered_secrets(&state).len(), before.len());
        // Repeated registration of the same credential collapses to one entry.
        register_secret(&state, "sk-supersecret-value-xyz");
        register_secret(&state, "sk-supersecret-value-xyz");
        let after = registered_secrets(&state);
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
        use super::{adopt_session_path, current_session_id, new_session_state, rotate_session_id};
        use std::path::Path;
        let state = new_session_state();
        // Adopting a transcript rebinds persistence to its file stem, so a
        // resumed conversation keeps writing the same file and history row.
        adopt_session_path(&state, Path::new("/x/sessions/123-45.json"));
        assert_eq!(current_session_id(&state), "123-45");
        // Rotating (after /clear) mints a fresh, non-empty, different id.
        let before = current_session_id(&state);
        rotate_session_id(&state);
        let after = current_session_id(&state);
        assert!(!after.is_empty(), "a fresh id is never empty");
        assert_ne!(before, after, "rotate must start a new conversation id");
    }

    #[test]
    fn native_read_only_tools_are_the_only_concurrent_safe_ones() {
        use super::NativeToolExecutor;
        use runtime::ToolExecutor;
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
        // workspace-write: bash prompts, and writers prompt too so the
        // confinement gate gets to see them (an Allow tool never consults it).
        let write = permission_policy_for_mode("workspace-write", &[]);
        assert_eq!(write.mode_for("bash"), PermissionMode::Prompt);
        assert_eq!(write.mode_for("edit_file"), PermissionMode::Prompt);
        assert_eq!(write.mode_for("write_file"), PermissionMode::Prompt);
        assert_eq!(write.mode_for("apply_patch"), PermissionMode::Prompt);
        // Readers stay unattended.
        assert_eq!(write.mode_for("read_file"), PermissionMode::Allow);
        assert_eq!(write.mode_for("glob_search"), PermissionMode::Allow);
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

    #[tokio::test]
    async fn workspace_write_confirms_only_dangerous_bash() {
        use super::permission_policy_for_mode;
        use runtime::PermissionOutcome;
        let write = permission_policy_for_mode("workspace-write", &[]);
        // Routine command auto-runs without prompting (no prompter needed).
        assert!(matches!(
            write
                .authorize("bash", r#"{"command":"ls -la"}"#, None)
                .await,
            PermissionOutcome::Allow
        ));
        // Destructive command falls through to interactive approval.
        assert!(matches!(
            write
                .authorize("bash", r#"{"command":"rm -rf /"}"#, None)
                .await,
            PermissionOutcome::Deny { .. }
        ));
        // web_fetch always confirms.
        assert!(matches!(
            write
                .authorize("web_fetch", r#"{"url":"https://x"}"#, None)
                .await,
            PermissionOutcome::Deny { .. }
        ));
        // ...and so does a harmless command that asks to keep the agent's
        // credentials in its environment. The opt-out is the escalation.
        assert!(matches!(
            write
                .authorize(
                    "bash",
                    r#"{"command":"ls -la","dangerouslyDisableSandbox":true}"#,
                    None
                )
                .await,
            PermissionOutcome::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn workspace_write_confines_writes_to_the_workspace_root() {
        use super::permission_policy_for_mode_in;
        use runtime::PermissionOutcome;
        let workspace = std::env::temp_dir().join("hf-ws-confine-test");
        let _ = std::fs::create_dir_all(&workspace);
        let write = permission_policy_for_mode_in("workspace-write", &[], &workspace);
        let inside = workspace.join("src").join("lib.rs");
        let inside_json = serde_json::json!({ "path": inside.to_string_lossy() }).to_string();

        // A write that resolves inside the workspace still runs unattended.
        assert!(matches!(
            write.authorize("write_file", &inside_json, None).await,
            PermissionOutcome::Allow
        ));

        // `..` traversal is caught even though the path is not absolute.
        assert!(matches!(
            write
                .authorize("write_file", r#"{"path":"../../etc/passwd"}"#, None)
                .await,
            PermissionOutcome::Deny { .. }
        ));

        // A writer with no parseable path fails closed.
        assert!(matches!(
            write
                .authorize("write_file", r#"{"content":"no path here"}"#, None)
                .await,
            PermissionOutcome::Deny { .. }
        ));

        // An `apply_patch` batch is judged on every change, not just the first.
        assert!(matches!(
            write
                .authorize(
                    "apply_patch",
                    r#"{"changes":[{"path":"src/ok.rs"},{"path":"../escape.rs"}]}"#,
                    None
                )
                .await,
            PermissionOutcome::Deny { .. }
        ));

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn plan_mode_gates_writes_to_the_plan_document() {
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
            )
            .await,
            PermissionOutcome::Allow
        );
        // Windows separators still resolve to a plan document.
        assert_eq!(
            plan.authorize(
                "edit_file",
                r#"{"path":".heartflow\\plans\\plan.md"}"#,
                None
            )
            .await,
            PermissionOutcome::Allow
        );
        // Any other write reaches BlockPrompter and is refused (the hard gate).
        assert!(matches!(
            plan.authorize(
                "write_file",
                r#"{"path":"src/main.rs","content":"x"}"#,
                Some(&mut BlockPrompter)
            )
            .await,
            PermissionOutcome::Deny { .. }
        ));
        // Non-plan writes with no prompter cannot silently proceed.
        assert!(matches!(
            plan.authorize("write_file", r#"{"path":"README.md"}"#, None)
                .await,
            PermissionOutcome::Deny { .. }
        ));
        // A `..` escape spelled from inside the plan directory is not a plan doc.
        assert!(matches!(
            plan.authorize(
                "write_file",
                r#"{"path":".heartflow/plans/../../src/lib.rs"}"#,
                Some(&mut BlockPrompter)
            )
            .await,
            PermissionOutcome::Deny { .. }
        ));
        // An `apply_patch` that nominates the plans folder but also writes a
        // source file is refused: every change must be a plan document.
        assert!(matches!(
            plan.authorize(
                "apply_patch",
                r#"{"changes":[{"path":".heartflow/plans/p.md"},{"path":"src/lib.rs"}]}"#,
                Some(&mut BlockPrompter)
            )
            .await,
            PermissionOutcome::Deny { .. }
        ));
        // A non-Markdown file inside the plan directory is still not a plan doc.
        assert!(matches!(
            plan.authorize(
                "write_file",
                r#"{"path":".heartflow/plans/notes.txt"}"#,
                Some(&mut BlockPrompter)
            )
            .await,
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
        use super::{fold_tool_output, new_session_state};
        let state = new_session_state();
        let short = fold_tool_output(&state, "bash", "one\ntwo", 40);
        assert!(short.contains("one\ntwo"));
        assert!(!short.contains("/expand"), "short output must not fold");

        let long = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let folded = fold_tool_output(&state, "read_file", &long, 40);
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
    fn parses_prompt_short_flag() {
        // `-p` is the short spelling of the `prompt` subcommand: same positional
        // text, same subcommand flags, same fold into `Action::Prompt`.
        assert_eq!(
            action(&["hf", "-p", "hello", "world"]),
            Action::Prompt {
                instruction: "hello world".to_string(),
                provider: None,
                model: None,
                quiet: false,
                json: false,
            }
        );
        // Subcommand flags still apply through the short spelling.
        assert_eq!(
            action(&["hf", "-p", "summarize", "--quiet", "--json"]),
            Action::Prompt {
                instruction: "summarize".to_string(),
                provider: None,
                model: None,
                quiet: true,
                json: true,
            }
        );
        // Bare `-p` with no text -> empty instruction, prompt read from stdin.
        assert_eq!(
            action(&["hf", "-p"]),
            Action::Prompt {
                instruction: String::new(),
                provider: None,
                model: None,
                quiet: false,
                json: false,
            }
        );
        // A global flag composes with the short spelling.
        assert_eq!(
            action(&["hf", "--provider", "deepseek", "-p", "hi"]),
            Action::Prompt {
                instruction: "hi".to_string(),
                provider: Some("deepseek".to_string()),
                model: None,
                quiet: false,
                json: false,
            }
        );
        // The long subcommand name is unchanged.
        assert_eq!(
            action(&["hf", "prompt", "hi"]),
            Action::Prompt {
                instruction: "hi".to_string(),
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
        // Bare `--resume` -> most recent session (resolved in run()), no path.
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
        // `-r` is the short spelling of bare `--resume` (most recent session).
        assert_eq!(
            action(&["hf", "-r"]),
            Action::ResumeSession {
                session_path: None,
                command: None,
                provider: None,
                model: None,
            }
        );
        // `-r=PATH` carries an explicit path, same as `--resume=PATH`.
        assert_eq!(
            action(&["hf", "-r=s.json"]),
            Action::ResumeSession {
                session_path: Some(PathBuf::from("s.json")),
                command: None,
                provider: None,
                model: None,
            }
        );
        // `-c/--config` parses into the global config field (applied in run()).
        assert_eq!(
            Cli::try_parse_from(["hf", "-c", "custom.toml"])
                .expect("parses")
                .config,
            Some(PathBuf::from("custom.toml"))
        );
        assert_eq!(
            Cli::try_parse_from(["hf", "--config", "custom.json", "chat"])
                .expect("parses")
                .config,
            Some(PathBuf::from("custom.json"))
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
                action: super::ConfigAction::Export {
                    surface: super::ConfigSurface::Config,
                    output: None,
                },
            }
        );
        assert_eq!(
            action(&["hf", "config", "export", "--output=out.toml"]),
            Action::Config {
                action: super::ConfigAction::Export {
                    surface: super::ConfigSurface::Config,
                    output: Some(PathBuf::from("out.toml")),
                },
            }
        );
        // The SURFACE positional selects which file to emit; it composes with
        // --output and defaults to `config` when omitted (asserted above).
        assert_eq!(
            action(&["hf", "config", "export", "theme"]),
            Action::Config {
                action: super::ConfigAction::Export {
                    surface: super::ConfigSurface::Theme,
                    output: None,
                },
            }
        );
        assert_eq!(
            action(&["hf", "config", "export", "keymap", "--output=km.toml"]),
            Action::Config {
                action: super::ConfigAction::Export {
                    surface: super::ConfigSurface::Keymap,
                    output: Some(PathBuf::from("km.toml")),
                },
            }
        );
        assert_eq!(
            action(&["hf", "config", "export", "settings"]),
            Action::Config {
                action: super::ConfigAction::Export {
                    surface: super::ConfigSurface::Settings,
                    output: None,
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
    fn bare_resume_carries_no_explicit_path() {
        // Bare `--resume` folds to `session_path: None`; run() resolves it to the
        // most recent session (the interactive picker was retired).
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
