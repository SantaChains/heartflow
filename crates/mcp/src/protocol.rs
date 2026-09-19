use serde_json::{json, Value};

/// Protocol version this client speaks; servers may reply with their own.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// One tool advertised by an MCP server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// `annotations.readOnlyHint` from the server: the tool does not change
    /// state, so it is safe to expose even under `read-only`/`plan` modes.
    pub read_only: bool,
}

/// In-band tool outcome: `is_error` maps to a tool-result error, not a
/// transport failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolResult {
    pub text: String,
    pub is_error: bool,
}

#[must_use]
pub fn build_request(id: u64, method: &str, params: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[must_use]
pub fn build_notification(method: &str, params: &Value) -> Value {
    let mut notification = json!({"jsonrpc": "2.0", "method": method});
    if !params.is_null() {
        notification["params"] = params.clone();
    }
    notification
}

/// JSON-RPC error object from a response; `None` when the response carries a
/// result instead.
#[must_use]
pub fn response_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    Some(message.to_string())
}

/// Parse one entry of a `tools/list` result; entries without a name are
/// skipped (fault isolation for sloppy servers).
#[must_use]
pub fn parse_tool(entry: &Value) -> Option<McpTool> {
    let name = entry.get("name").and_then(Value::as_str)?;
    let description = entry
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let input_schema = entry
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let read_only = entry
        .get("annotations")
        .and_then(|annotations| annotations.get("readOnlyHint"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(McpTool {
        name: name.to_string(),
        description,
        input_schema,
        read_only,
    })
}

/// Flatten a `tools/call` result to plain text: text content blocks joined by
/// newlines, falling back to the raw result JSON.
#[must_use]
pub fn tool_result_text(result: &Value) -> String {
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        let text = blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return text;
        }
    }
    match result.get("structuredContent") {
        Some(structured) => structured.to_string(),
        None => result.to_string(),
    }
}
