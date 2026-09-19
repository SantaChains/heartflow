//! Minimal MCP stdio server used by integration tests and live smoke runs.
//! Speaks just enough JSON-RPC to exercise the client: initialize,
//! tools/list, tools/call, ping.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = value.get("id").cloned();

        let response = match (method, id.as_ref()) {
            ("initialize", Some(id)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "echo-server", "version": "0.1.0"}
                }
            }),
            ("ping", Some(id)) => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            ("tools/list", Some(id)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": [{
                    "name": "echo",
                    "description": "Echo back the provided text",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"]
                    }
                }]}
            }),
            ("tools/call", Some(id)) => {
                let name = value["params"]["name"].as_str().unwrap_or_default();
                if name == "echo" {
                    let text = value["params"]["arguments"]["text"]
                        .as_str()
                        .unwrap_or_default();
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": text}],
                            "isError": false
                        }
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32602, "message": format!("unknown tool: {name}")}
                    })
                }
            }
            (_, Some(id)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "method not found"}
            }),
            _ => continue,
        };
        if writeln!(out, "{response}").is_err() {
            break;
        }
        let _ = out.flush();
    }
}
