use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use provider::config::{
    config_file_paths, load_merged_settings, materialize, quote_toml_string, resolve_spec,
    string_field, ProviderSelection,
};
use tracing::warn;

use crate::{keymap::keymap_file_paths, settings::settings_file_paths, theme::theme_file_paths};

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

/// One of the four on-disk configuration files heartflow layers user-then-project.
/// A single enum names them all so `hf config export SURFACE` and the hot-reload
/// [`ConfigWatcher`] stay in lockstep: adding a surface is one variant, not two
/// parallel lists that can drift.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSurface {
    /// Provider + MCP connection settings (`config.toml`).
    Config,
    /// Color palette (`theme.toml`).
    Theme,
    /// Key bindings (`keymap.toml`).
    Keymap,
    /// Shell behavior knobs (`settings.toml`).
    Settings,
}

/// Per-surface change flags from one [`ConfigWatcher::changed`] poll. Callers
/// reload only the surfaces that actually moved, so editing `theme.toml` never
/// rebuilds the provider runtime and editing `config.toml` never disturbs the
/// palette or key bindings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangedSurfaces {
    pub config: bool,
    pub theme: bool,
    pub keymap: bool,
    pub settings: bool,
}

impl ChangedSurfaces {
    /// True when at least one surface changed since the last poll.
    #[must_use]
    pub fn any(self) -> bool {
        self.config || self.theme || self.keymap || self.settings
    }
}

/// Detects on-disk config edits between REPL turns by comparing file mtimes.
/// The REPL can only act on a change at a turn boundary, so lightweight mtime
/// polling is used instead of a background watcher thread: same effect, no
/// extra dependency, no thread. Absent files are tracked as `None` so creation
/// is also detected. One watcher spans every [`ConfigSurface`] (config, theme,
/// keymap, settings — user and project layer each), tagging every path with its
/// surface so `changed` reports them independently.
#[derive(Debug, Clone)]
pub struct ConfigWatcher {
    entries: Vec<(ConfigSurface, PathBuf, Option<SystemTime>)>,
}

impl ConfigWatcher {
    #[must_use]
    pub fn new(cwd: &Path, home: &Path) -> Self {
        let mut watched: Vec<(ConfigSurface, PathBuf)> = config_file_paths(cwd, home)
            .into_iter()
            .map(|path| (ConfigSurface::Config, path))
            .collect();
        watched.extend(
            theme_file_paths(cwd, home)
                .into_iter()
                .map(|path| (ConfigSurface::Theme, path)),
        );
        watched.extend(
            keymap_file_paths(cwd, home)
                .into_iter()
                .map(|path| (ConfigSurface::Keymap, path)),
        );
        watched.extend(
            settings_file_paths(cwd, home)
                .into_iter()
                .map(|path| (ConfigSurface::Settings, path)),
        );
        let entries = watched
            .into_iter()
            .map(|(surface, path)| {
                let stamp = modified_time(&path);
                (surface, path, stamp)
            })
            .collect();
        Self { entries }
    }

    /// Poll every watched file's mtime, returning which surfaces changed since
    /// the last observation and refreshing the stored stamps as it goes. Only
    /// the moved surfaces are flagged, so a caller can reload a palette edit
    /// without paying for a provider/MCP rebuild.
    pub fn changed(&mut self) -> ChangedSurfaces {
        let mut changed = ChangedSurfaces::default();
        for (surface, path, stamp) in &mut self.entries {
            let now = modified_time(path);
            if now != *stamp {
                *stamp = now;
                match surface {
                    ConfigSurface::Config => changed.config = true,
                    ConfigSurface::Theme => changed.theme = true,
                    ConfigSurface::Keymap => changed.keymap = true,
                    ConfigSurface::Settings => changed.settings = true,
                }
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
    use std::fs;

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
        assert!(!watcher.changed().any(), "no edit yet");
        // Removal flips the mtime sentinel Some -> None regardless of clock
        // granularity, so the change is always observable.
        fs::remove_file(&cfg).expect("remove cfg");
        let changed = watcher.changed();
        assert!(changed.config, "removal must be detected");
        assert!(
            !changed.theme && !changed.keymap && !changed.settings,
            "only the config surface moved"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn watcher_reports_surfaces_independently() {
        use super::ConfigWatcher;
        let base = std::env::temp_dir().join(format!("hf-watch-surface-{}", std::process::id()));
        let proj = base.join("proj");
        let home = base.join("home");
        let cfg_dir = home.join(".heartflow");
        fs::create_dir_all(&cfg_dir).expect("create home cfg dir");
        let theme = cfg_dir.join("theme.toml");
        fs::write(&theme, "[theme]\n").expect("write theme");

        let mut watcher = ConfigWatcher::new(&proj, &home);
        assert!(!watcher.changed().any(), "baseline quiet");
        // Editing only theme.toml flags theme and nothing else, so a palette
        // change never triggers the provider/MCP rebuild reserved for config.
        fs::remove_file(&theme).expect("remove theme");
        let changed = watcher.changed();
        assert!(changed.theme, "theme edit detected");
        assert!(
            !changed.config && !changed.keymap && !changed.settings,
            "config/keymap/settings untouched"
        );

        let _ = fs::remove_dir_all(&base);
    }
}
