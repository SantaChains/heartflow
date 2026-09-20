use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tracing::{debug, warn};

pub const DEFAULT_MODEL: &str = "mimo-v2.5-pro";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Config schema version. Readers accept older files (missing fields default),
/// tolerate unknown fields (forward compatibility), and skip broken fields.
pub const CONFIG_VERSION: u32 = 1;

const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// Wire protocol spoken by a provider endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocol {
    /// Anthropic `/v1/messages` dialect (also used by bridged endpoints).
    Anthropic,
    /// `OpenAI` `chat/completions` dialect (`DeepSeek` native among others).
    OpenAi,
    /// `OpenAI` `Responses` (`/v1/responses`) dialect.
    OpenAiResponses,
}

impl ProviderProtocol {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(Self::Anthropic),
            "openai" | "openai-compatible" | "openai_compat" => Some(Self::OpenAi),
            "openai-responses" | "responses" | "openai_responses" => Some(Self::OpenAiResponses),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiResponses => "openai-responses",
        }
    }
}

/// Fully resolved transport configuration for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfile {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub api_key: String,
    pub auth_token: Option<String>,
    pub model: String,
    pub max_tokens: u32,
    pub reasoning_effort: Option<String>,
    /// Model context window in tokens, used to drive window-based compaction.
    /// `None` leaves window compaction off unless the env var sets it.
    pub context_window: Option<u32>,
}

/// What the CLI resolved for the API transport before constructing a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSelection {
    /// Legacy environment-driven setup (`ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY`).
    Env {
        model: String,
    },
    Profile(ProviderProfile),
}

impl ProviderSelection {
    #[must_use]
    pub fn model(&self) -> &str {
        match self {
            Self::Env { model } => model,
            Self::Profile(profile) => &profile.model,
        }
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        let model = model.into();
        match self {
            Self::Env { model: slot } => *slot = model,
            Self::Profile(profile) => profile.model = model,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BuiltinProvider {
    protocol: ProviderProtocol,
    base_url: &'static str,
    api_key_env: &'static str,
    model: Option<&'static str>,
}

fn builtin_provider(name: &str) -> Option<BuiltinProvider> {
    match name {
        "deepseek" => Some(BuiltinProvider {
            protocol: ProviderProtocol::OpenAi,
            base_url: DEEPSEEK_BASE_URL,
            api_key_env: "DEEPSEEK_API_KEY",
            model: Some("deepseek-flash"),
        }),
        "anthropic" => Some(BuiltinProvider {
            protocol: ProviderProtocol::Anthropic,
            base_url: ANTHROPIC_BASE_URL,
            api_key_env: "ANTHROPIC_API_KEY",
            model: None,
        }),
        _ => None,
    }
}

/// Source-level `[provider]` settings, safe to export: secret values never
/// appear here, only the names of the environment variables holding them.
///
/// Every field is optional so older files load unchanged and unknown fields
/// are ignored; a broken field is skipped instead of failing the whole file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderSettings {
    pub name: Option<String>,
    pub protocol: Option<ProviderProtocol>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// Inline plaintext key written directly in `config.toml`. Distinct from
    /// `api_key_env` (which only names an env var). Env wins when set, inline is
    /// the fallback; the value is never emitted by `to_toml_string` (export stays
    /// secret-free).
    pub api_key: Option<String>,
    pub auth_token_env: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
    /// Model context window in tokens for window-based compaction.
    pub context_window: Option<u32>,
    pub version: Option<u32>,
}

impl ProviderSettings {
    /// Field-by-field extraction: one bad entry never poisons the rest.
    #[must_use]
    pub fn from_table(table: &toml::Table, source: &str) -> Self {
        let version = u32_field(table, "version", source);
        if let Some(found) = version {
            if found > CONFIG_VERSION {
                warn!(
                    file = %source,
                    found,
                    supported = CONFIG_VERSION,
                    "config version is newer; parsing with current schema"
                );
            }
        }

        let Some(provider) = table.get("provider").and_then(toml::Value::as_table) else {
            return Self {
                version,
                ..Self::default()
            };
        };

        for key in provider.keys() {
            if !matches!(
                key.as_str(),
                "name"
                    | "protocol"
                    | "base_url"
                    | "api_key_env"
                    | "api_key"
                    | "auth_token_env"
                    | "model"
                    | "max_tokens"
                    | "reasoning_effort"
                    | "context_window"
            ) {
                debug!(file = %source, field = %key, "unknown config field; ignored");
            }
        }

        let protocol = string_field(provider, "protocol", source).and_then(|text| {
            let parsed = ProviderProtocol::parse(&text);
            if parsed.is_none() {
                warn!(file = %source, value = %text, "unknown provider protocol; skipped");
            }
            parsed
        });

        Self {
            name: string_field(provider, "name", source),
            protocol,
            base_url: string_field(provider, "base_url", source),
            api_key_env: string_field(provider, "api_key_env", source),
            api_key: string_field(provider, "api_key", source),
            auth_token_env: string_field(provider, "auth_token_env", source),
            model: string_field(provider, "model", source),
            max_tokens: u32_field(provider, "max_tokens", source),
            reasoning_effort: string_field(provider, "reasoning_effort", source),
            context_window: u32_field(provider, "context_window", source),
            version,
        }
    }

    /// Later layers win per field; `None` fields never erase earlier values.
    pub fn merge(&mut self, source: ProviderSettings) {
        if source.name.is_some() {
            self.name = source.name;
        }
        if source.protocol.is_some() {
            self.protocol = source.protocol;
        }
        if source.base_url.is_some() {
            self.base_url = source.base_url;
        }
        if source.api_key_env.is_some() {
            self.api_key_env = source.api_key_env;
        }
        if source.api_key.is_some() {
            self.api_key = source.api_key;
        }
        if source.auth_token_env.is_some() {
            self.auth_token_env = source.auth_token_env;
        }
        if source.model.is_some() {
            self.model = source.model;
        }
        if source.max_tokens.is_some() {
            self.max_tokens = source.max_tokens;
        }
        if source.reasoning_effort.is_some() {
            self.reasoning_effort = source.reasoning_effort;
        }
        if source.context_window.is_some() {
            self.context_window = source.context_window;
        }
        if source.version.is_some() {
            self.version = source.version;
        }
    }

    /// Render as canonical config.toml text (version header plus `[provider]`).
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        use std::fmt::Write as _;

        let mut out = format!("version = {}\n", self.version.unwrap_or(CONFIG_VERSION));
        out.push_str("\n[provider]\n");
        if let Some(protocol) = self.protocol {
            let _ = writeln!(out, "protocol = \"{}\"", protocol.as_str());
        }
        for (key, value) in [
            ("name", &self.name),
            ("base_url", &self.base_url),
            ("api_key_env", &self.api_key_env),
            ("auth_token_env", &self.auth_token_env),
            ("model", &self.model),
        ] {
            if let Some(text) = value {
                let _ = writeln!(out, "{key} = {}", quote_toml_string(text));
            }
        }
        if let Some(tokens) = self.max_tokens {
            let _ = writeln!(out, "max_tokens = {tokens}");
        }
        if let Some(window) = self.context_window {
            let _ = writeln!(out, "context_window = {window}");
        }
        if let Some(effort) = &self.reasoning_effort {
            let _ = writeln!(out, "reasoning_effort = {}", quote_toml_string(effort));
        }
        out
    }
}

fn quote_toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn string_field(table: &toml::Table, key: &str, source: &str) -> Option<String> {
    match table.get(key).map(toml::Value::as_str) {
        Some(Some(text)) => Some(text.to_string()),
        Some(None) => {
            warn!(file = %source, field = key, "config field type mismatch; skipped");
            None
        }
        None => None,
    }
}

fn u32_field(table: &toml::Table, key: &str, source: &str) -> Option<u32> {
    match table.get(key).map(toml::Value::as_integer) {
        Some(Some(value)) => u32::try_from(value).map_or_else(
            |_| {
                warn!(file = %source, field = key, value, "config field out of range; skipped");
                None
            },
            Some,
        ),
        Some(None) => {
            warn!(file = %source, field = key, "config field type mismatch; skipped");
            None
        }
        None => None,
    }
}

/// Pre-env resolution result: either env mode or a profile awaiting its key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderSpec {
    Env {
        model: String,
    },
    Resolved {
        protocol: ProviderProtocol,
        base_url: String,
        api_key_env: String,
        api_key: Option<String>,
        auth_token_env: Option<String>,
        model: String,
        max_tokens: u32,
        reasoning_effort: Option<String>,
        context_window: Option<u32>,
    },
}

/// Pure precedence resolution: CLI flags beat the merged config files, which
/// beat built-in provider defaults. Env mode survives only when nothing
/// configures an explicit transport.
fn resolve_spec(
    provider_flag: Option<&str>,
    model_flag: Option<&str>,
    settings: &ProviderSettings,
) -> Result<ProviderSpec, String> {
    let name = provider_flag.or(settings.name.as_deref());
    let builtin = name.and_then(builtin_provider);

    if let Some(name) = name {
        if builtin.is_none()
            && settings.base_url.is_none()
            && settings.api_key_env.is_none()
            && settings.api_key.is_none()
        {
            return Err(format!(
                "unknown provider: {name} (available: deepseek, anthropic, or set [provider] base_url in config.toml)"
            ));
        }
    }

    if name.is_none()
        && settings.base_url.is_none()
        && settings.api_key_env.is_none()
        && settings.api_key.is_none()
    {
        let model = model_flag
            .map(str::to_string)
            .or(settings.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        return Ok(ProviderSpec::Env { model });
    }

    // Config layers beat built-in defaults per field so a proxy base_url or a
    // renamed key env can override a builtin provider without losing the rest.
    let base_url = settings
        .base_url
        .clone()
        .or_else(|| builtin.map(|provider| provider.base_url.to_string()))
        .ok_or("provider base_url missing: set [provider] base_url in config.toml")?;
    let inline_key = settings
        .api_key
        .clone()
        .filter(|key| !key.trim().is_empty());
    let api_key_env = settings
        .api_key_env
        .clone()
        .or_else(|| builtin.map(|provider| provider.api_key_env.to_string()))
        // An inline key alone can reach a custom endpoint, so fall back to an
        // empty env-var name (materialize then uses the inline value).
        .or_else(|| inline_key.as_ref().map(|_| String::new()))
        .ok_or("provider api_key_env missing: set [provider] api_key_env or [provider] api_key in config.toml")?;
    let model = model_flag
        .map(str::to_string)
        .or(settings.model.clone())
        .or_else(|| builtin.and_then(|provider| provider.model.map(str::to_string)))
        .ok_or("model required: pass --model or set [provider] model in config.toml")?;
    // Custom providers default to the Anthropic dialect so legacy config
    // files keep their original meaning; builtins carry their own protocol.
    let protocol = builtin.map_or_else(
        || settings.protocol.unwrap_or(ProviderProtocol::Anthropic),
        |provider| provider.protocol,
    );

    Ok(ProviderSpec::Resolved {
        protocol,
        base_url,
        api_key_env,
        api_key: inline_key,
        auth_token_env: settings.auth_token_env.clone(),
        model,
        max_tokens: settings.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        reasoning_effort: settings.reasoning_effort.clone(),
        context_window: settings.context_window,
    })
}

fn materialize(spec: ProviderSpec) -> Result<ProviderSelection, String> {
    // Credential-sourcing policy (guardrail): a key may originate from exactly
    // two places and nowhere else -- (1) the environment variable *named* by
    // `api_key_env` (the name, never the value, is what config files store and
    // export), or (2) an inline `[provider] api_key` in a local config file.
    // hf never scans keychains, browser stores, other tools' dotfiles, or shell
    // history, and `to_toml_string` emits neither source's value. Env wins when
    // it holds a non-empty value; the inline key is only the on-disk fallback.
    match spec {
        ProviderSpec::Env { model } => Ok(ProviderSelection::Env { model }),
        ProviderSpec::Resolved {
            protocol,
            base_url,
            api_key_env,
            api_key,
            auth_token_env,
            model,
            max_tokens,
            reasoning_effort,
            context_window,
        } => {
            // Industry-standard split: `api_key_env` (indirect, secret stays out
            // of the file) takes precedence over inline `[provider] api_key`,
            // which is only the on-disk fallback.
            let from_env = env::var(&api_key_env).unwrap_or_default();
            let api_key = if from_env.is_empty() {
                api_key.unwrap_or_default()
            } else {
                from_env
            };
            if api_key.is_empty() {
                return Err(if api_key_env.is_empty() {
                    "api key missing: set [provider] api_key in config.toml or export an api-key env var"
                        .to_string()
                } else {
                    format!(
                        "environment variable {api_key_env} is not set and no [provider] api_key is configured"
                    )
                });
            }
            let auth_token = auth_token_env
                .and_then(|name| env::var(name).ok())
                .filter(|token| !token.is_empty());
            Ok(ProviderSelection::Profile(ProviderProfile {
                protocol,
                base_url,
                api_key,
                auth_token,
                model,
                max_tokens,
                reasoning_effort,
                context_window,
            }))
        }
    }
}

/// Load one config file with full fault isolation: missing files yield an
/// empty layer; unreadable or unparseable files are skipped with a warning.
fn read_provider_file(path: &Path) -> ProviderSettings {
    let empty = ProviderSettings::default();
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return empty,
        Err(error) => {
            warn!(file = %path.display(), error = %error, "config file unreadable; skipped");
            return empty;
        }
    };
    match toml::from_str::<toml::Table>(&contents) {
        Ok(table) => ProviderSettings::from_table(&table, &path.display().to_string()),
        Err(error) => {
            warn!(file = %path.display(), error = %error, "config file unparseable; skipped");
            empty
        }
    }
}

/// Layered config file paths in merge order: user then project (project wins).
#[must_use]
pub fn config_file_paths(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".heartflow").join("config.toml"),
        cwd.join(".heartflow").join("config.toml"),
    ]
}

/// Merge user `~/.heartflow/config.toml` then project `.heartflow/config.toml`
/// (project wins per field).
#[must_use]
pub fn load_merged_settings(cwd: &Path, home: &Path) -> ProviderSettings {
    let mut settings = ProviderSettings::default();
    for path in config_file_paths(cwd, home) {
        settings.merge(read_provider_file(&path));
    }
    settings
}

/// One MCP server launch specification. A server is reached over stdio (spawn
/// `command`) or Streamable-HTTP (`url` is set); the presence of `url` selects
/// the transport so existing stdio entries load unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Streamable-HTTP endpoint; when present the server is reached over HTTP.
    pub url: Option<String>,
    /// Extra HTTP headers for the endpoint; values may hold `${VAR}` to expand.
    pub headers: BTreeMap<String, String>,
    /// Environment variable naming a bearer token for the HTTP endpoint.
    pub bearer_token_env: Option<String>,
    /// Treat every tool from this server as read-only, overriding what the
    /// server advertises, so they stay usable under `read-only`/`plan` modes.
    pub read_only: bool,
}

/// `[mcp.servers]` entries keyed by server name; project layers replace user
/// layers per server. Broken entries are skipped with a warning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpSettings {
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpSettings {
    #[must_use]
    pub fn from_table(table: &toml::Table, source: &str) -> Self {
        let mut settings = Self::default();
        let Some(servers) = table
            .get("mcp")
            .and_then(toml::Value::as_table)
            .and_then(|mcp| mcp.get("servers"))
            .and_then(toml::Value::as_table)
        else {
            return settings;
        };
        for (name, value) in servers {
            let Some(server_table) = value.as_table() else {
                warn!(file = %source, server = %name, "mcp server entry must be a table; skipped");
                continue;
            };
            let url = string_field(server_table, "url", source);
            let command = string_field(server_table, "command", source);
            // A server needs one of `url` (HTTP) or `command` (stdio).
            if url.is_none() && command.is_none() {
                warn!(file = %source, server = %name, "mcp server needs `command` or `url`; skipped");
                continue;
            }
            let args = string_array(server_table, "args", source);
            let mut env = BTreeMap::new();
            if let Some(env_table) = server_table.get("env").and_then(toml::Value::as_table) {
                for (key, value) in env_table {
                    if let Some(text) = value.as_str() {
                        env.insert(key.clone(), text.to_string());
                    } else {
                        warn!(
                            file = %source,
                            server = %name,
                            key = %key,
                            "mcp env value must be a string; skipped"
                        );
                    }
                }
            }
            let mut headers = BTreeMap::new();
            if let Some(header_table) = server_table.get("headers").and_then(toml::Value::as_table)
            {
                for (key, value) in header_table {
                    if let Some(text) = value.as_str() {
                        headers.insert(key.clone(), text.to_string());
                    } else {
                        warn!(
                            file = %source,
                            server = %name,
                            key = %key,
                            "mcp header value must be a string; skipped"
                        );
                    }
                }
            }
            let read_only = server_table
                .get("read_only")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            settings.servers.insert(
                name.clone(),
                McpServerConfig {
                    command: command.unwrap_or_default(),
                    args,
                    env,
                    url,
                    headers,
                    bearer_token_env: string_field(server_table, "bearer_token_env", source),
                    read_only,
                },
            );
        }
        settings
    }

    /// Later layers replace earlier ones per server name.
    pub fn merge(&mut self, source: Self) {
        for (name, config) in source.servers {
            self.servers.insert(name, config);
        }
    }

    /// Render as canonical config.toml text (`[mcp.servers.NAME]` tables).
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        for (name, config) in &self.servers {
            let key = toml_key(name);
            let _ = writeln!(out, "\n[mcp.servers.{key}]");
            if !config.command.is_empty() {
                let _ = writeln!(out, "command = {}", quote_toml_string(&config.command));
            }
            if !config.args.is_empty() {
                let rendered = config
                    .args
                    .iter()
                    .map(|arg| quote_toml_string(arg))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "args = [{rendered}]");
            }
            if let Some(url) = &config.url {
                let _ = writeln!(out, "url = {}", quote_toml_string(url));
            }
            if let Some(bearer) = &config.bearer_token_env {
                let _ = writeln!(out, "bearer_token_env = {}", quote_toml_string(bearer));
            }
            if config.read_only {
                let _ = writeln!(out, "read_only = true");
            }
            if !config.env.is_empty() {
                let _ = writeln!(out, "\n[mcp.servers.{key}.env]");
                for (key, value) in &config.env {
                    let _ = writeln!(out, "{key} = {}", quote_toml_string(value));
                }
            }
            if !config.headers.is_empty() {
                let _ = writeln!(out, "\n[mcp.servers.{key}.headers]");
                for (key, value) in &config.headers {
                    let _ = writeln!(out, "{key} = {}", quote_toml_string(value));
                }
            }
        }
        out
    }
}

/// Bare TOML key when safe, quoted string key otherwise.
fn toml_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        name.to_string()
    } else {
        quote_toml_string(name)
    }
}

fn string_array(table: &toml::Table, key: &str, source: &str) -> Vec<String> {
    match table.get(key).map(toml::Value::as_array) {
        Some(Some(entries)) => entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if let Some(text) = entry.as_str() {
                    Some(text.to_string())
                } else {
                    warn!(file = %source, field = key, index, "array entry must be a string; skipped");
                    None
                }
            })
            .collect(),
        Some(None) => {
            warn!(file = %source, field = key, "config field type mismatch; skipped");
            Vec::new()
        }
        None => Vec::new(),
    }
}

/// Load merged `[mcp.servers]` from user then project config files.
#[must_use]
pub fn load_merged_mcp(cwd: &Path, home: &Path) -> McpSettings {
    let mut settings = McpSettings::default();
    for path in config_file_paths(cwd, home) {
        settings.merge(read_mcp_file(&path));
    }
    settings
}

fn read_mcp_file(path: &Path) -> McpSettings {
    let empty = McpSettings::default();
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return empty,
        Err(error) => {
            warn!(file = %path.display(), error = %error, "config file unreadable; skipped");
            return empty;
        }
    };
    match toml::from_str::<toml::Table>(&contents) {
        Ok(table) => McpSettings::from_table(&table, &path.display().to_string()),
        Err(error) => {
            warn!(file = %path.display(), error = %error, "config file unparseable; skipped");
            empty
        }
    }
}

/// Load the provider selection from merged config files and CLI flags.
pub fn load_provider_selection(
    cwd: &Path,
    home: &Path,
    provider_flag: Option<&str>,
    model_flag: Option<&str>,
) -> Result<ProviderSelection, String> {
    let settings = load_merged_settings(cwd, home);
    materialize(resolve_spec(provider_flag, model_flag, &settings)?)
}

/// Detects on-disk config edits between REPL turns by comparing file mtimes.
/// The REPL can only act on a change at a turn boundary, so lightweight mtime
/// polling is used instead of a background watcher thread: same effect, no
/// extra dependency, no thread. Absent files are tracked as `None` so creation
/// is also detected.
#[derive(Debug, Clone)]
pub struct ConfigWatcher {
    paths: Vec<PathBuf>,
    stamps: Vec<Option<SystemTime>>,
}

impl ConfigWatcher {
    #[must_use]
    pub fn new(cwd: &Path, home: &Path) -> Self {
        let paths = config_file_paths(cwd, home);
        let stamps = paths
            .iter()
            .map(|path| modified_time(path))
            .collect::<Vec<_>>();
        Self { paths, stamps }
    }

    /// Returns true when any watched file's mtime differs from the last
    /// observation, refreshing the stored stamps as it goes.
    pub fn changed(&mut self) -> bool {
        let mut changed = false;
        for (path, stamp) in self.paths.iter().zip(&mut self.stamps) {
            let now = modified_time(path);
            if now != *stamp {
                *stamp = now;
                changed = true;
            }
        }
        changed
    }
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{
        materialize, resolve_spec, ProviderProfile, ProviderProtocol, ProviderSelection,
        ProviderSettings, ProviderSpec, CONFIG_VERSION, DEFAULT_MAX_TOKENS, DEFAULT_MODEL,
    };
    use std::fs;
    use std::path::{Path, PathBuf};

    fn settings(text: &str) -> ProviderSettings {
        let table: toml::Table = toml::from_str(text).expect("toml should parse");
        ProviderSettings::from_table(&table, "test.toml")
    }

    #[test]
    fn defaults_to_env_mode_with_default_model() {
        let spec = resolve_spec(None, None, &ProviderSettings::default()).expect("resolve");
        assert_eq!(
            spec,
            ProviderSpec::Env {
                model: DEFAULT_MODEL.to_string()
            }
        );
    }

    #[test]
    fn env_mode_accepts_model_override() {
        let spec = resolve_spec(None, Some("m2"), &ProviderSettings::default()).expect("resolve");
        assert_eq!(
            spec,
            ProviderSpec::Env {
                model: "m2".to_string()
            }
        );
    }

    #[test]
    fn deepseek_builtin_sets_native_endpoint_and_model() {
        let spec =
            resolve_spec(Some("deepseek"), None, &ProviderSettings::default()).expect("resolve");
        match spec {
            ProviderSpec::Resolved {
                protocol,
                base_url,
                api_key_env,
                model,
                max_tokens,
                ..
            } => {
                assert_eq!(protocol, ProviderProtocol::OpenAi);
                assert_eq!(base_url, "https://api.deepseek.com/v1");
                assert_eq!(api_key_env, "DEEPSEEK_API_KEY");
                assert_eq!(model, "deepseek-flash");
                assert_eq!(max_tokens, DEFAULT_MAX_TOKENS);
            }
            other @ ProviderSpec::Env { .. } => panic!("expected resolved profile, got {other:?}"),
        }
    }

    #[test]
    fn config_layers_override_builtin_base_url_and_key_env() {
        let spec = resolve_spec(
            Some("deepseek"),
            None,
            &settings(concat!(
                "[provider]\n",
                "base_url = \"https://proxy.local/v1\"\n",
                "api_key_env = \"PROXY_KEY\"\n"
            )),
        )
        .expect("proxy override should resolve");
        match spec {
            ProviderSpec::Resolved {
                base_url,
                api_key_env,
                ..
            } => {
                assert_eq!(base_url, "https://proxy.local/v1");
                assert_eq!(api_key_env, "PROXY_KEY");
            }
            other @ ProviderSpec::Env { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn protocol_field_overrides_custom_provider_dialect() {
        let parsed = settings(concat!(
            "[provider]\n",
            "name = \"deepseek\"\n",
            "protocol = \"anthropic\"\n",
            "base_url = \"https://api.deepseek.com/anthropic\"\n",
            "api_key_env = \"DEEPSEEK_API_KEY\"\n",
            "model = \"deepseek-flash\"\n"
        ));
        assert_eq!(parsed.protocol, Some(ProviderProtocol::Anthropic));

        let broken = settings("[provider]\nprotocol = \"grpc\"\n");
        assert_eq!(broken.protocol, None);
    }

    #[test]
    fn context_window_round_trips_through_parse_and_export() {
        let parsed = settings("[provider]\ncontext_window = 65536\n");
        assert_eq!(parsed.context_window, Some(65536));
        let exported = parsed.to_toml_string();
        assert!(
            exported.contains("context_window = 65536"),
            "export must carry context_window: {exported}"
        );
        // Merge keeps an existing window unless a later layer sets one.
        let mut merged = parsed.clone();
        merged.merge(settings("[provider]\nmodel = \"m\"\n"));
        assert_eq!(merged.context_window, Some(65536));
        merged.merge(settings("[provider]\ncontext_window = 131072\n"));
        assert_eq!(merged.context_window, Some(131_072));
    }

    #[test]
    fn project_file_overrides_user_file_and_flags_win_over_both() {
        let mut merged = settings(
            "[provider]\nname = \"deepseek\"\nmodel = \"deepseek-v4-pro\"\nmax_tokens = 16384\n",
        );
        merged.merge(settings("[provider]\nmodel = \"deepseek-flash\"\n"));
        let spec = resolve_spec(None, Some("deepseek-v4-pro"), &merged).expect("resolve");
        match spec {
            ProviderSpec::Resolved {
                model, max_tokens, ..
            } => {
                assert_eq!(model, "deepseek-v4-pro");
                assert_eq!(max_tokens, 16384);
            }
            other @ ProviderSpec::Env { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn custom_provider_without_builtin_requires_full_config() {
        let error = resolve_spec(Some("mimo"), None, &ProviderSettings::default())
            .expect_err("unknown provider should error");
        assert!(error.contains("unknown provider"));

        let spec = resolve_spec(
            Some("mimo"),
            None,
            &settings(concat!(
                "[provider]\n",
                "base_url = \"http://localhost:8000\"\n",
                "api_key_env = \"MIMO_KEY\"\n",
                "model = \"mimo-v2.5-pro\"\n"
            )),
        )
        .expect("full config should resolve");
        match spec {
            ProviderSpec::Resolved { base_url, .. } => {
                assert_eq!(base_url, "http://localhost:8000");
            }
            other @ ProviderSpec::Env { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn anthropic_builtin_requires_explicit_model() {
        let error = resolve_spec(Some("anthropic"), None, &ProviderSettings::default())
            .expect_err("anthropic needs a model");
        assert!(error.contains("model required"));
    }

    #[test]
    fn materialize_reports_missing_key_env() {
        let spec = ProviderSpec::Resolved {
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://example.invalid".to_string(),
            api_key_env: "HEARTFLOW_TEST_UNSET_KEY".to_string(),
            api_key: None,
            auth_token_env: None,
            model: "m".to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            context_window: None,
        };
        let error = materialize(spec).expect_err("missing key should error");
        assert!(error.contains("HEARTFLOW_TEST_UNSET_KEY"));
    }

    #[test]
    fn inline_api_key_is_parsed_but_not_exported() {
        let parsed = settings("[provider]\napi_key = \"sk-inline-secret\"\n");
        assert_eq!(parsed.api_key.as_deref(), Some("sk-inline-secret"));
        let exported = parsed.to_toml_string();
        assert!(
            !exported.contains("sk-inline-secret"),
            "export must redact inline keys"
        );
        assert!(
            !exported.contains("api_key ="),
            "export must omit the inline key field"
        );
    }

    #[test]
    fn inline_key_falls_back_when_env_unset() {
        let spec = ProviderSpec::Resolved {
            protocol: ProviderProtocol::OpenAi,
            base_url: "https://example.invalid".to_string(),
            api_key_env: "HEARTFLOW_TEST_UNSET_INLINE".to_string(),
            api_key: Some("sk-fallback".to_string()),
            auth_token_env: None,
            model: "m".to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            context_window: None,
        };
        match materialize(spec).expect("inline fallback should materialize") {
            ProviderSelection::Profile(profile) => assert_eq!(profile.api_key, "sk-fallback"),
            other @ ProviderSelection::Env { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn custom_endpoint_reachable_with_only_inline_key() {
        let resolved = resolve_spec(
            Some("custom"),
            Some("m"),
            &settings("[provider]\nbase_url = \"http://localhost:8000\"\napi_key = \"sk-abc\"\n"),
        )
        .expect("inline-only custom should resolve");
        match materialize(resolved).expect("materialize inline-only custom") {
            ProviderSelection::Profile(profile) => assert_eq!(profile.api_key, "sk-abc"),
            other @ ProviderSelection::Env { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn env_var_wins_over_inline_key() {
        // Prove precedence against an always-set ambient var rather than
        // mutating process env (which is unsafe in edition 2024 and racy).
        let ambient = "PATH";
        let Ok(value) = std::env::var(ambient) else {
            return; // no ambient env to prove precedence in this sandbox
        };
        if value.is_empty() {
            return;
        }
        let spec = ProviderSpec::Resolved {
            protocol: ProviderProtocol::OpenAi,
            base_url: "https://example.invalid".to_string(),
            api_key_env: ambient.to_string(),
            api_key: Some("sk-should-lose".to_string()),
            auth_token_env: None,
            model: "m".to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            context_window: None,
        };
        match materialize(spec).expect("env key should win") {
            ProviderSelection::Profile(profile) => {
                assert_ne!(profile.api_key, "sk-should-lose");
                assert_eq!(profile.api_key, value);
            }
            other @ ProviderSelection::Env { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn blank_inline_key_is_never_treated_as_a_credential() {
        // A whitespace-only inline key must not masquerade as a source: it is
        // filtered out, leaving no env name and no value, so resolution fails
        // rather than silently shipping an empty credential to the provider.
        let error = resolve_spec(
            Some("custom"),
            Some("m"),
            &settings("[provider]\nbase_url = \"http://localhost:8000\"\napi_key = \"   \"\n"),
        )
        .expect_err("blank inline key is not a usable credential source");
        assert!(
            error.contains("api_key_env missing"),
            "expected the missing-key guard to fire: {error}"
        );
    }

    #[test]
    fn selection_reports_and_updates_model() {
        let mut selection = ProviderSelection::Env {
            model: "a".to_string(),
        };
        assert_eq!(selection.model(), "a");
        selection.set_model("b");
        assert_eq!(selection.model(), "b");

        let mut selection = ProviderSelection::Profile(ProviderProfile {
            protocol: ProviderProtocol::OpenAi,
            base_url: "https://x".to_string(),
            api_key: "k".to_string(),
            auth_token: None,
            model: "old".to_string(),
            max_tokens: 4096,
            reasoning_effort: None,
            context_window: None,
        });
        selection.set_model("new");
        assert_eq!(selection.model(), "new");
    }

    #[test]
    fn broken_fields_are_skipped_while_valid_fields_survive() {
        let parsed = settings(concat!(
            "[provider]\n",
            "name = \"deepseek\"\n",
            "base_url = 42\n",
            "max_tokens = \"oops\"\n",
            "model = \"deepseek-flash\"\n"
        ));
        assert_eq!(parsed.name.as_deref(), Some("deepseek"));
        assert_eq!(parsed.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(parsed.base_url, None);
        assert_eq!(parsed.max_tokens, None);
    }

    #[test]
    fn many_simultaneously_broken_fields_still_isolate() {
        // Corrupt every field type at once except name/model: each bad field
        // independently degrades to None while the good subset survives, so one
        // poisoned line never takes down the whole provider block.
        let parsed = settings(concat!(
            "[provider]\n",
            "name = \"deepseek\"\n",
            "protocol = \"grpc\"\n",
            "base_url = 42\n",
            "api_key = true\n",
            "model = \"deepseek-flash\"\n",
            "max_tokens = \"oops\"\n",
            "reasoning_effort = 7\n",
            "context_window = -5\n",
        ));
        assert_eq!(parsed.name.as_deref(), Some("deepseek"));
        assert_eq!(parsed.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(parsed.protocol, None);
        assert_eq!(parsed.base_url, None);
        assert_eq!(parsed.api_key, None);
        assert_eq!(parsed.max_tokens, None);
        assert_eq!(parsed.reasoning_effort, None);
        assert_eq!(parsed.context_window, None);
    }

    #[test]
    fn unparseable_file_yields_empty_layer() {
        // A wholly corrupt file must not panic or half-parse: the fault-isolated
        // reader returns an empty layer so the rest of the config chain survives.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("hf-config-shock-{nanos}.toml"));
        std::fs::write(&path, "this is = = not valid toml [[[").expect("write garbage");
        assert_eq!(
            super::read_provider_file(&path),
            ProviderSettings::default()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_after_shock_only_carries_valid_fields() {
        let parsed = settings(concat!(
            "[provider]\n",
            "model = \"m\"\n",
            "context_window = 65536\n",
            "max_tokens = \"broken\"\n",
        ));
        let exported = parsed.to_toml_string();
        assert!(exported.contains("model = \"m\""));
        assert!(exported.contains("context_window = 65536"));
        assert!(
            !exported.contains("max_tokens"),
            "broken field must not be exported: {exported}"
        );
    }

    #[test]
    fn unknown_fields_and_future_versions_are_ignored() {
        let parsed = settings(concat!(
            "version = 99\n",
            "[provider]\n",
            "name = \"deepseek\"\n",
            "future_field = \"anything\"\n"
        ));
        assert_eq!(parsed.name.as_deref(), Some("deepseek"));
        assert_eq!(parsed.version, Some(99));
    }

    #[test]
    fn out_of_range_integer_is_skipped() {
        let parsed = settings("[provider]\nmax_tokens = 99999999999\n");
        assert_eq!(parsed.max_tokens, None);
    }

    #[test]
    fn mcp_servers_parse_with_defaults() {
        let table: toml::Table = toml::from_str(concat!(
            "[mcp.servers.echo]\n",
            "command = \"mcp_echo_server\"\n",
            "args = [\"--quiet\"]\n",
            "[mcp.servers.echo.env]\n",
            "ECHO_MODE = \"fast\"\n"
        ))
        .expect("toml should parse");
        let parsed = super::McpSettings::from_table(&table, "test.toml");
        let config = parsed.servers.get("echo").expect("echo server");
        assert_eq!(config.command, "mcp_echo_server");
        assert_eq!(config.args, vec!["--quiet".to_string()]);
        assert_eq!(
            config.env.get("ECHO_MODE").map(String::as_str),
            Some("fast")
        );
    }

    #[test]
    fn mcp_servers_without_command_are_skipped() {
        let table: toml::Table = toml::from_str(concat!(
            "[mcp.servers.broken]\n",
            "args = [\"--help\"]\n",
            "[mcp.servers.good]\n",
            "command = \"echo\"\n"
        ))
        .expect("toml should parse");
        let parsed = super::McpSettings::from_table(&table, "test.toml");
        assert_eq!(parsed.servers.len(), 1);
        assert!(parsed.servers.contains_key("good"));
    }

    #[test]
    fn mcp_project_layer_replaces_user_layer_per_server() {
        let mut merged: super::McpSettings = toml::from_str::<toml::Table>(concat!(
            "[mcp.servers.a]\ncommand = \"user-a\"\n",
            "[mcp.servers.b]\ncommand = \"user-b\"\n"
        ))
        .map(|table| super::McpSettings::from_table(&table, "user.toml"))
        .expect("toml should parse");
        let project: super::McpSettings =
            toml::from_str::<toml::Table>("[mcp.servers.a]\ncommand = \"project-a\"\n")
                .map(|table| super::McpSettings::from_table(&table, "project.toml"))
                .expect("toml should parse");
        merged.merge(project);
        assert_eq!(merged.servers["a"].command, "project-a");
        assert_eq!(merged.servers["b"].command, "user-b");
    }

    #[test]
    fn mcp_toml_roundtrip_preserves_servers() {
        let mut original = super::McpSettings::default();
        original.servers.insert(
            "my server".to_string(),
            super::McpServerConfig {
                command: "run.exe".to_string(),
                args: vec!["--flag".to_string()],
                env: [("KEY".to_string(), "value".to_string())]
                    .into_iter()
                    .collect(),
                ..super::McpServerConfig::default()
            },
        );
        let text = original.to_toml_string();
        let table: toml::Table = toml::from_str(&text).expect("rendered toml should parse");
        let parsed = super::McpSettings::from_table(&table, "roundtrip.toml");
        assert_eq!(parsed, original);
    }

    #[test]
    fn mcp_http_server_parses_and_round_trips() {
        let table: toml::Table = toml::from_str(concat!(
            "[mcp.servers.remote]\n",
            "url = \"https://example.com/mcp\"\n",
            "bearer_token_env = \"REMOTE_MCP_TOKEN\"\n",
            "read_only = true\n",
            "[mcp.servers.remote.headers]\n",
            "x-api-key = \"${KEY}\"\n",
        ))
        .expect("toml should parse");
        let parsed = super::McpSettings::from_table(&table, "test.toml");
        let config = parsed.servers.get("remote").expect("remote server");
        assert_eq!(config.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(config.command, "");
        assert_eq!(config.bearer_token_env.as_deref(), Some("REMOTE_MCP_TOKEN"));
        assert!(config.read_only);
        assert_eq!(
            config.headers.get("x-api-key").map(String::as_str),
            Some("${KEY}")
        );

        let text = parsed.to_toml_string();
        let reparsed_table: toml::Table =
            toml::from_str(&text).expect("rendered toml should parse");
        assert_eq!(
            super::McpSettings::from_table(&reparsed_table, "roundtrip.toml"),
            parsed
        );
    }

    #[test]
    fn mcp_server_without_command_or_url_is_skipped() {
        let table: toml::Table = toml::from_str(concat!(
            "[mcp.servers.bad]\n",
            "args = [\"--x\"]\n",
            "[mcp.servers.ok]\n",
            "command = \"c\"\n"
        ))
        .expect("toml should parse");
        let parsed = super::McpSettings::from_table(&table, "test.toml");
        assert_eq!(parsed.servers.len(), 1);
        assert!(parsed.servers.contains_key("ok"));
    }

    #[test]
    fn toml_roundtrip_preserves_fields() {
        let original = ProviderSettings {
            name: Some("deepseek".to_string()),
            protocol: Some(ProviderProtocol::OpenAi),
            base_url: Some("https://api.deepseek.com/v1".to_string()),
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            api_key: None,
            auth_token_env: None,
            model: Some("deepseek-v4-pro".to_string()),
            max_tokens: Some(16384),
            reasoning_effort: Some("high".to_string()),
            context_window: Some(65536),
            version: Some(CONFIG_VERSION),
        };
        let parsed = settings(&original.to_toml_string());
        assert_eq!(parsed, original);
    }

    #[test]
    fn toml_quotes_special_characters() {
        let original = ProviderSettings {
            base_url: Some("http://x/\"a\"\\b".to_string()),
            ..ProviderSettings::default()
        };
        let parsed = settings(&original.to_toml_string());
        assert_eq!(parsed.base_url, original.base_url);
    }

    #[test]
    fn unparseable_file_degrades_to_empty_layer() {
        let path = temp_path("broken");
        fs::write(&path, "[provider\nname =").expect("write broken toml");
        let parsed = super::read_provider_file(&path);
        assert_eq!(parsed, ProviderSettings::default());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_degrades_to_empty_layer() {
        let parsed = super::read_provider_file(&temp_path("absent"));
        assert_eq!(parsed, ProviderSettings::default());
    }

    #[test]
    fn config_file_paths_are_user_then_project() {
        let paths = super::config_file_paths(Path::new("/proj"), Path::new("/home"));
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("/home/.heartflow/config.toml"));
        assert_eq!(paths[1], PathBuf::from("/proj/.heartflow/config.toml"));
    }

    #[test]
    fn watcher_ignores_unchanged_and_flags_removal() {
        use super::ConfigWatcher;
        let base = std::env::temp_dir().join(format!("hf-watch-{}", std::process::id()));
        let proj = base.join("proj");
        let home = base.join("home");
        let cfg_dir = home.join(".heartflow");
        fs::create_dir_all(&cfg_dir).expect("create home cfg dir");
        let cfg = cfg_dir.join("config.toml");
        fs::write(&cfg, "[provider]\nmodel = \"a\"\n").expect("write cfg");

        let mut watcher = ConfigWatcher::new(&proj, &home);
        assert!(!watcher.changed(), "no edit yet");
        // Removal flips the mtime sentinel Some -> None regardless of clock
        // granularity, so the change is always observable.
        fs::remove_file(&cfg).expect("remove cfg");
        assert!(watcher.changed(), "removal must be detected");

        let _ = fs::remove_dir_all(&base);
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "heartflow-cfg-test-{}-{label}.toml",
            std::process::id()
        ))
    }
}
