//! Command-line surface: the clap-derived argument types (`Cli`, `Command`,
//! the per-subcommand arg structs) and their normalization into a single
//! `Action`. Extracted verbatim from `main.rs` (Phase 1 thinning); no behavior
//! change. Only back-reference to the crate root is `current_date`.

use std::env;
use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

use super::current_date;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigAction {
    Export {
        surface: ConfigSurface,
        output: Option<PathBuf>,
    },
    Import {
        path: PathBuf,
    },
}

/// Which on-disk config surface `hf config export` emits. Defined once in
/// [`crate::config`] so the export command and the hot-reload watcher share a
/// single source of truth; re-exported here for the clap surface.
pub(crate) use crate::config::ConfigSurface;

/// Normalized action produced from the parsed CLI surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
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
    after_help = "INTERACTION CONTRACT\n  Interactive:  no args or `hf chat` opens the REPL (this is the only mode that can prompt for confirmation).\n  Resume:       `hf --resume[=PATH] [--run \"/cmd\"]` reopens a saved session (PATH omitted = most recent session; value form uses `=`).\n  Non-interactive: subcommands (prompt/search/...) never block on a human. Because there is no tty to answer a confirmation, `prompt` runs tools under the permission mode from HEARTFLOW_PERMISSION_MODE, defaulting to `full` (auto-allow). Set HEARTFLOW_PERMISSION_MODE=read-only for an unattended, read-only pipe.\n\nEXIT CODES\n  0  success\n  1  runtime/provider error (stream, config resolution, failed turn)\n  2  usage error (bad arguments; emitted by the argument parser)"
)]
pub(crate) struct Cli {
    /// Provider name (deepseek, anthropic, or a [provider] table entry).
    #[arg(long, global = true)]
    pub(crate) provider: Option<String>,
    /// Model override for the selected provider.
    #[arg(long, global = true)]
    pub(crate) model: Option<String>,
    /// Print version information. Accepts the conventional `-V` and the
    /// shorthand `-v` (as in node/npm); the built-in flag is disabled so this
    /// one owns both spellings.
    #[arg(short = 'v', visible_short_alias = 'V', long = "version", action = ArgAction::Version)]
    pub(crate) version: (),
    /// Resume a saved session; `-r`/`--resume` alone restores the most recent
    /// session, `--resume=PATH` opens a specific file. The value must use `=` so
    /// a bare flag never swallows a following subcommand token.
    #[arg(short = 'r', long, num_args = 0..=1, require_equals = true, value_name = "PATH")]
    // Two-level Option is the clap idiom for a tri-state flag: absent, bare
    // `--resume`, or `--resume=PATH`. An enum would fight the derive macros.
    #[allow(clippy::option_option)]
    pub(crate) resume: Option<Option<PathBuf>>,
    /// Load configuration from an explicit file, merged as the highest file
    /// layer above the project and user `config.toml`. Accepts TOML, or JSON
    /// when the path ends in `.json`. CLI `--provider`/`--model` still win.
    #[arg(short = 'c', long = "config", global = true, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,
    /// Slash command to run right after resuming (requires --resume).
    #[arg(long, value_name = "CMD", requires = "resume")]
    pub(crate) run: Option<String>,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
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
pub(crate) struct SystemPromptArgs {
    /// Working directory the prompt should describe.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
    /// Date to embed (YYYY-MM-DD).
    #[arg(long)]
    pub(crate) date: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    pub(crate) action: ConfigCommand,
}

#[derive(Args, Debug)]
pub(crate) struct DoctorArgs {
    /// Apply safe repairs: create missing directories, move unparseable config aside.
    #[arg(long)]
    pub(crate) fix: bool,
    /// Ask the built-in AI to analyze any problems found and suggest repairs.
    #[arg(long)]
    pub(crate) ai: bool,
}

#[derive(Args, Debug)]
pub(crate) struct InitArgs {
    /// Overwrite an existing AGENTS.md instead of leaving it untouched.
    #[arg(long)]
    pub(crate) force: bool,
}

#[derive(Args, Debug)]
pub(crate) struct ModelsArgs {
    /// Query the account balance (`DeepSeek`). Off by default because the
    /// balance endpoint counts against API quota; model listing is free.
    #[arg(long)]
    pub(crate) balance: bool,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCommand {
    /// Export a merged config surface (without secrets): `config` (default),
    /// `theme`, `keymap`, `settings`, or `provider` (the model catalog).
    Export {
        /// Which surface to export.
        #[arg(value_name = "SURFACE", default_value = "config")]
        surface: ConfigSurface,
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
    pub(crate) fn into_action(self) -> Result<Action, String> {
        let Cli {
            provider,
            model,
            resume,
            run,
            command,
            config: _,
            version: (),
        } = self;
        if let Some(session_path) = resume {
            if command.is_some() {
                return Err("--resume cannot be combined with a subcommand".to_string());
            }
            return Ok(Action::ResumeSession {
                // `Some(None)` == bare `--resume` -> most recent session.
                session_path,
                command: run,
                provider,
                model,
            });
        }
        match command {
            None | Some(Command::Chat) => Ok(Action::Repl { provider, model }),
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
                    ConfigCommand::Export { surface, output } => {
                        ConfigAction::Export { surface, output }
                    }
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
