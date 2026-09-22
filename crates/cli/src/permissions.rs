//! Permission policy resolution (cli-thinning): the mode string -> policy
//! mapping, the risky-command / plan-document prompt gates, the live-runtime
//! mode swap, and the hard-gate `BlockPrompter` used while planning. Extracted
//! verbatim from `main.rs` and re-exported crate-wide so existing call sites
//! and `super::` test paths keep resolving.

use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use runtime::{
    escapes_workspace, PermissionMode, PermissionPolicy, PermissionPromptDecision,
    PermissionPrompter, PermissionRequest,
};

use crate::AgentRuntime;

/// Default permission mode: from `HEARTFLOW_PERMISSION_MODE`, else
/// `workspace-write` when interactive and `full` for one-shot runs.
pub(crate) fn default_permission_mode(interactive: bool) -> String {
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
/// work unattended while stopping for anything that leaves the workspace — a
/// `bash` command that looks destructive, a write whose target resolves outside
/// the workspace root, or a network tool.
///
/// `mcp_read_only` names (from `McpToolset::read_only_tool_names`) are added to
/// the Allow set for the two Deny-default modes, so read-only MCP tools stay
/// usable in `read-only`/`plan` without opening up write-capable ones.
///
/// An unrecognised mode is reported and treated as `workspace-write`: silently
/// applying a policy the user did not ask for is the worst option, and
/// `workspace-write` is the narrowest of the modes they might have meant.
pub(crate) fn permission_policy_for_mode(mode: &str, mcp_read_only: &[String]) -> PermissionPolicy {
    // The workspace root is the directory the agent was started in; tool paths
    // resolve against it, which is what write confinement checks against.
    let workspace = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    permission_policy_for_mode_in(mode, mcp_read_only, &workspace)
}

/// [`permission_policy_for_mode`] with an explicit workspace root, so tests can
/// aim confinement at a temp directory instead of the process cwd.
pub(crate) fn permission_policy_for_mode_in(
    mode: &str,
    mcp_read_only: &[String],
    workspace: &Path,
) -> PermissionPolicy {
    let mode = match mode {
        "read-only" | "workspace-write" | "full" | "auto" | "plan" => mode,
        other => {
            eprintln!(
                "heartflow: unknown permission mode `{other}`; using `workspace-write`. \
                 Valid values are read-only, workspace-write, full (auto) and plan."
            );
            "workspace-write"
        }
    };

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
        // plan document itself. Writers route to `Prompt`; the gate auto-allows a
        // write that resolves into `.heartflow/plans/` and every other write
        // hits `BlockPrompter` (a hard gate). `bash` and everything else fall to
        // the `Deny` default.
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
            .with_prompt_gate(plan_gate_for(workspace)),
        // `workspace-write`, the default interactive mode. Writers are `Prompt`
        // rather than `Allow` so the gate sees them at all — an `Allow` tool
        // never consults the gate, which is how writes outside the workspace
        // went through unchecked while the mode name promised otherwise.
        _ => PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_mode("write_file", PermissionMode::Prompt)
            .with_tool_mode("edit_file", PermissionMode::Prompt)
            .with_tool_mode("apply_patch", PermissionMode::Prompt)
            .with_tool_mode("bash", PermissionMode::Prompt)
            // Network egress leaves the sandbox; ask like a dangerous command does.
            .with_tool_mode("web_fetch", PermissionMode::Prompt)
            .with_tool_mode("web_search", PermissionMode::Prompt)
            .with_tool_mode("generate_image", PermissionMode::Prompt)
            .with_prompt_gate(workspace_write_gate(workspace)),
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
pub(crate) fn set_runtime_mode_policy(runtime: &mut AgentRuntime, mode: &str) {
    let mcp_read_only = runtime.executor().mcp_read_only_names();
    runtime.set_permission_policy(permission_policy_for_mode(mode, &mcp_read_only));
}

/// Prompt gate for `workspace-write`. Returns `None` (run without asking) for
/// routine work, `Some(reason)` when the call leaves the workspace.
///
/// `bash` is judged by shape (`is_dangerous_command`); writes are judged by
/// *location* — a target resolving inside the workspace runs unattended, one
/// resolving outside stops for a human. Network tools always ask, since they
/// leave the sandbox by nature and have no "inside" to compare against.
fn workspace_write_gate(
    workspace: &Path,
) -> impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static {
    let workspace = workspace.to_path_buf();
    move |tool_name, input| {
        if tool_name == "bash" {
            // `dangerouslyDisableSandbox` is a *model-supplied* field, and the
            // only thing it turns off is `scrub_credential_env`. Left in the
            // unattended path it is a one-word escalation: `{"command":"ls",
            // "dangerouslyDisableSandbox":true}` hands the child the agent's own
            // `GITHUB_TOKEN` / `HF_API_KEY`. So it is treated exactly like a
            // destructive command — the shape of `command` is irrelevant.
            if sandbox_opt_out(input) {
                return Some(
                    "runs with the agent's credentials in its environment \
                     (dangerouslyDisableSandbox)"
                        .to_string(),
                );
            }
            return runtime::is_dangerous_command(&input_str_field(input, "command"))
                .then(|| "the command looks destructive".to_string());
        }

        let targets = write_targets(tool_name, input);
        if !targets.is_empty() {
            let escaped: Vec<String> = targets
                .into_iter()
                .filter(|target| escapes_workspace(&workspace, target))
                .collect();
            return (!escaped.is_empty()).then(|| {
                format!(
                    "{} resolves outside the workspace root ({})",
                    escaped.join(", "),
                    workspace.display()
                )
            });
        }

        // Fail closed: a writer whose input named no path at all (missing field,
        // or unparsable JSON) lands here and is confirmed rather than allowed.
        match tool_name {
            "web_fetch" | "web_search" => Some("network egress leaves the workspace".to_string()),
            "generate_image" => Some("generates an image through a remote API".to_string()),
            other => Some(format!("`{other}` is not covered by a workspace rule")),
        }
    }
}

/// Whether a `bash` input asks to skip credential scrubbing. Read as a strict
/// boolean so a swapped-out field type (a string `"true"`, a `null`) is not
/// mistaken for consent with the safe reading.
fn sandbox_opt_out(input: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|value| value.get("dangerouslyDisableSandbox").cloned())
        .and_then(|flag| flag.as_bool())
        .unwrap_or(false)
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

/// Prompt gate for `plan` mode. `None` (no confirmation) only for a write that
/// genuinely resolves into the plan directory; everything else asks, and
/// `BlockPrompter` then refuses.
///
/// The path is *resolved*, not substring-matched. The previous
/// `path.contains(".heartflow/plans/")` test accepted any input that merely
/// mentioned that directory: `.heartflow/plans/../../src/lib.rs` matched, a
/// backslash spelling slipped the separator check, and an `apply_patch` batch
/// could hide its real target while one change nominated the plans folder.
fn plan_gate_for(
    workspace: &Path,
) -> impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static {
    let workspace = workspace.to_path_buf();
    move |tool_name, input| {
        let plans = workspace.join(".heartflow").join("plans");
        let targets = write_targets(tool_name, input);
        let all_are_plan_docs = !targets.is_empty()
            && targets
                .iter()
                .all(|target| is_markdown(target) && resolves_into(&workspace, &plans, target));
        if all_are_plan_docs {
            return None;
        }
        Some(
            "planning mode writes only a Markdown plan under .heartflow/plans/; anything else \
             needs `/plan approve` first"
                .to_string(),
        )
    }
}

/// Whether `target` (spelled the way the model wrote it) resolves inside `dir`,
/// with a relative target taken against `workspace`.
fn resolves_into(workspace: &Path, dir: &Path, target: &str) -> bool {
    let raw = Path::new(target);
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        workspace.join(raw)
    };
    !escapes_workspace(dir, absolute)
}

fn is_markdown(target: &str) -> bool {
    Path::new(target)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

/// The paths a writer will touch, read out of its JSON input.
///
/// Empty for a non-writer — and also for a writer whose input does not name one
/// path per change, so an unexpected shape is never silently reduced to
/// "nothing to check": both gates turn an empty list into "ask".
fn write_targets(tool_name: &str, input: &str) -> Vec<String> {
    if !matches!(tool_name, "write_file" | "edit_file" | "apply_patch") {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return Vec::new();
    };
    match tool_name {
        "write_file" | "edit_file" => value
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        _ => {
            let Some(changes) = value.get("changes").and_then(serde_json::Value::as_array) else {
                return Vec::new();
            };
            let paths: Vec<String> = changes
                .iter()
                .filter_map(|change| change.get("path").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect();
            // One readable path per change, or nothing at all.
            if paths.len() == changes.len() {
                paths
            } else {
                Vec::new()
            }
        }
    }
}

/// Prompter used while planning: any action the plan policy routed to `Prompt`
/// (a write outside the plan document) is refused outright, which is what turns
/// the policy into a hard gate. Plan-document writes never reach here because
/// `plan_gate` auto-allows them.
pub(crate) struct BlockPrompter;

impl PermissionPrompter for BlockPrompter {
    fn decide<'a>(
        &'a mut self,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
        Box::pin(async move {
            PermissionPromptDecision::Deny {
                reason: format!(
                    "planning mode: `{}` is blocked. Only the plan document under .heartflow/plans/ may be written; get approval with `/plan approve` before making real changes.",
                    request.tool_name
                ),
            }
        })
    }
}
