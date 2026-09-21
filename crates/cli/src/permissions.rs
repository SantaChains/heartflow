//! Permission policy resolution (cli-thinning): the mode string -> policy
//! mapping, the risky-command / plan-document prompt gates, the live-runtime
//! mode swap, and the hard-gate `BlockPrompter` used while planning. Extracted
//! verbatim from `main.rs` and re-exported crate-wide so existing call sites
//! and `super::` test paths keep resolving.

use std::env;
use std::future::Future;
use std::pin::Pin;

use runtime::{
    PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
    PermissionRequest,
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
/// commands but confirms only high-blast-radius ones (a `bash` command is
/// auto-allowed unless it looks destructive; `web_fetch` always confirms).
///
/// `mcp_read_only` names (from `McpToolset::read_only_tool_names`) are added to
/// the Allow set for the two Deny-default modes, so read-only MCP tools stay
/// usable in `read-only`/`plan` without opening up write-capable ones.
pub(crate) fn permission_policy_for_mode(mode: &str, mcp_read_only: &[String]) -> PermissionPolicy {
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
pub(crate) fn set_runtime_mode_policy(runtime: &mut AgentRuntime, mode: &str) {
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
