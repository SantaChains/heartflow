use std::fmt::{Display, Formatter};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::{
    build_notification, build_request, parse_tool, response_error, tool_result_text, McpTool,
    McpToolResult, PROTOCOL_VERSION,
};
use crate::transport::Transport;

/// Upper bound for one JSON-RPC round trip; servers must stay responsive.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum McpError {
    Io(io::Error),
    Protocol(String),
}

impl Display for McpError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Protocol(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for McpError {}

impl From<io::Error> for McpError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Server identity captured during the `initialize` handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
}

/// One MCP server connection. Requests are serialized per client; tool
/// execution happens on blocking threads.
pub struct McpClient {
    transport: Mutex<Box<dyn Transport>>,
    next_id: AtomicU64,
    server_info: McpServerInfo,
}

impl McpClient {
    /// Run the `initialize` handshake and announce readiness.
    pub fn connect(mut transport: Box<dyn Transport>) -> Result<Self, McpError> {
        let result = send_request(
            &mut transport,
            1,
            "initialize",
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "heartflow", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        let server_info = parse_server_info(&result);
        send_notification(&mut transport, "notifications/initialized")?;
        Ok(Self {
            transport: Mutex::new(transport),
            next_id: AtomicU64::new(2),
            server_info,
        })
    }

    #[must_use]
    pub fn server_info(&self) -> &McpServerInfo {
        &self.server_info
    }

    /// Advertised tools; entries without a name are skipped.
    pub fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        let result = self.request("tools/list", &json!({}))?;
        let entries = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(entries.iter().filter_map(parse_tool).collect())
    }

    /// Invoke one tool. `is_error` surfaces in-band as `Ok`, matching the
    /// protocol; only transport and JSON-RPC failures return `Err`.
    pub fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpToolResult, McpError> {
        let result = self.request("tools/call", &json!({"name": name, "arguments": arguments}))?;
        Ok(McpToolResult {
            text: tool_result_text(&result),
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn request(&self, method: &str, params: &Value) -> Result<Value, McpError> {
        let mut transport = self
            .transport
            .lock()
            .map_err(|_| McpError::Protocol("transport lock poisoned".to_string()))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        send_request(&mut transport, id, method, params)
    }
}

fn send_request(
    transport: &mut Box<dyn Transport>,
    id: u64,
    method: &str,
    params: &Value,
) -> Result<Value, McpError> {
    transport.send_line(&build_request(id, method, params).to_string())?;
    loop {
        let Some(line) = transport.recv_line(REQUEST_TIMEOUT)? else {
            return Err(McpError::Protocol(
                "server closed the connection".to_string(),
            ));
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let response_id = value.get("id").and_then(Value::as_u64);
        if response_id == Some(id) {
            if let Some(message) = response_error(&value) {
                return Err(McpError::Protocol(message));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
        if response_id.is_some() {
            if value.get("method").and_then(Value::as_str) == Some("ping") {
                transport.send_line(
                    &json!({"jsonrpc": "2.0", "id": value["id"], "result": {}}).to_string(),
                )?;
            } else if value.get("method").is_some() {
                transport.send_line(
                    &json!({
                        "jsonrpc": "2.0",
                        "id": value["id"],
                        "error": {"code": -32601, "message": "method not supported by heartflow"}
                    })
                    .to_string(),
                )?;
            }
        }
    }
}

fn send_notification(transport: &mut Box<dyn Transport>, method: &str) -> Result<(), McpError> {
    transport.send_line(&build_notification(method, &Value::Null).to_string())?;
    Ok(())
}

fn parse_server_info(result: &Value) -> McpServerInfo {
    let info = result.get("serverInfo");
    McpServerInfo {
        name: info
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        version: info
            .and_then(|info| info.get("version"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        protocol_version: result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crate::transport::ScriptedTransport;

    use super::{McpClient, McpError, PROTOCOL_VERSION};
    use serde_json::{json, Value};

    fn initialize_response() -> String {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "scripted", "version": "1.0.0"}
            }
        })
        .to_string()
    }

    fn connect_client(
        extra_incoming: Vec<String>,
    ) -> (McpClient, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let mut incoming = vec![initialize_response()];
        incoming.extend(extra_incoming);
        let (transport, sent) = ScriptedTransport::new(incoming);
        let client = McpClient::connect(Box::new(transport)).expect("handshake should succeed");
        (client, sent)
    }

    #[test]
    fn handshake_records_server_info_and_announces_ready() {
        let (client, _) = connect_client(Vec::new());
        assert_eq!(client.server_info().name, "scripted");
        assert_eq!(client.server_info().version, "1.0.0");
        assert_eq!(client.server_info().protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn list_tools_parses_and_skips_nameless_entries() {
        let (client, _) = connect_client(vec![json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"tools": [
                {"name": "echo", "description": "Echo text",
                 "inputSchema": {"type": "object"}},
                {"description": "missing name"}
            ]}
        })
        .to_string()]);

        let tools = client.list_tools().expect("tools should list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo text");
    }

    #[test]
    fn call_tool_joins_text_and_reports_is_error() {
        let (client, _) = connect_client(vec![json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ], "isError": true}
        })
        .to_string()]);

        let result = client
            .call_tool("echo", &json!({"text": "hi"}))
            .expect("call should stay in-band");
        assert_eq!(result.text, "first\nsecond");
        assert!(result.is_error);
    }

    #[test]
    fn json_rpc_error_maps_to_protocol_error() {
        let (client, _) = connect_client(vec![json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {"code": -32602, "message": "unknown tool"}
        })
        .to_string()]);

        let error = client
            .call_tool("missing", &json!({}))
            .expect_err("error response should fail");
        assert!(matches!(error, McpError::Protocol(message) if message == "unknown tool"));
    }

    #[test]
    fn server_requests_are_refused_and_pings_answered() {
        let (client, sent) = connect_client(vec![
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "sampling/createMessage",
                "params": {}
            })
            .to_string(),
            json!({"jsonrpc": "2.0", "id": 8, "method": "ping"}).to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo"}]}
            })
            .to_string(),
        ]);

        let tools = client.list_tools().expect("tools should list");
        assert_eq!(tools.len(), 1);

        let lines = sent.lock().expect("sent log lock");
        let refused = lines
            .iter()
            .find(|line| line.contains("-32601"))
            .expect("sampling request should be refused");
        let value: Value = serde_json::from_str(refused).expect("sent line is json");
        assert_eq!(value["id"], 7);
        let ping = lines
            .iter()
            .find(|line| line.contains("\"id\":8"))
            .expect("ping should be answered");
        let value: Value = serde_json::from_str(ping).expect("sent line is json");
        assert_eq!(value["result"], json!({}));
    }

    #[test]
    fn connect_fails_when_server_closes_early() {
        let (transport, _) = ScriptedTransport::new(Vec::new());
        let Err(error) = McpClient::connect(Box::new(transport)) else {
            panic!("closed transport should fail handshake");
        };
        assert!(matches!(error, McpError::Protocol(_)));
    }
}
