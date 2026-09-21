//! Tool routing and MCP connection glue (cli-thinning). Holds the CLI's
//! [`ToolExecutor`] implementations — [`NativeToolExecutor`] for the built-in
//! tools and [`AgentToolExecutor`] wrapping it plus the connected MCP servers —
//! and the connect helpers that build a [`McpToolset`] from config. Extracted
//! from `main.rs` so the assembly layer stays focused on wiring.
//!
//! The toolset is shared behind an `Arc`: the multi-section shell connects
//! every server once and hands the same handle to each section, so N sections
//! cost one set of MCP server processes rather than N. [`McpClient`] serializes
//! requests per connection (internal mutex), so sharing stays correct even once
//! sections run turns concurrently.

use std::env;
use std::path::Path;
use std::sync::Arc;

use mcp::{HttpTransport, McpClient, McpTool, StdioTransport, Transport};
use runtime::{normalize_tool_schema, ToolError, ToolExecutor, ToolSpec};
use tools::{todo_tool_spec, TodoLedger};

use crate::config::{load_merged_mcp, McpServerConfig};
use crate::interact::{QuestionOption, UserQuestioner};

pub(crate) struct NativeToolExecutor {
    todo: Arc<TodoLedger>,
    questioner: Option<Arc<dyn UserQuestioner>>,
}

impl NativeToolExecutor {
    pub(crate) fn new(questioner: Option<Arc<dyn UserQuestioner>>) -> Self {
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
pub(crate) struct McpToolset {
    servers: Vec<McpServerTools>,
}

impl McpToolset {
    /// Namespaced names of every MCP tool that is safe to expose in
    /// `read-only`/`plan` modes: those the server annotates `readOnlyHint`, plus
    /// every tool of a server the config marks `read_only`.
    pub(crate) fn read_only_tool_names(&self) -> Vec<String> {
        self.servers
            .iter()
            .flat_map(|server| server.tools.iter().map(move |tool| (server, tool)))
            .filter(|(server, tool)| server.config_read_only || tool.read_only)
            .map(|(server, tool)| format!("mcp__{}__{}", server.name, tool.name))
            .collect()
    }
}

/// Routes tool calls between the native tools and connected MCP servers. The
/// toolset is shared (`Arc`) so the multi-section shell connects every server
/// once and every section routes through the same handles.
pub(crate) struct AgentToolExecutor {
    native: NativeToolExecutor,
    mcp: Arc<McpToolset>,
}

impl AgentToolExecutor {
    pub(crate) fn new(mcp: Arc<McpToolset>, questioner: Option<Arc<dyn UserQuestioner>>) -> Self {
        Self {
            native: NativeToolExecutor::new(questioner),
            mcp,
        }
    }

    /// Shared handle to the plan ledger. The task-loop orchestrator reads this
    /// to verify completion without going through the model.
    pub(crate) fn todo_ledger(&self) -> Arc<TodoLedger> {
        Arc::clone(&self.native.todo)
    }

    /// Namespaced read-only MCP tool names, so a rebuilt permission policy can
    /// keep them usable under `read-only`/`plan` after a mode switch.
    pub(crate) fn mcp_read_only_names(&self) -> Vec<String> {
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
pub(crate) fn connect_mcp_servers(cwd: &Path, home: &Path) -> McpToolset {
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
pub(crate) fn expand_env_vars(value: &str) -> String {
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
