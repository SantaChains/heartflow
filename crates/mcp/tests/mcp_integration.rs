use std::collections::BTreeMap;

use mcp::{McpClient, McpError, StdioTransport};
use serde_json::json;

#[test]
fn round_trips_against_echo_server() {
    let exe = env!("CARGO_BIN_EXE_mcp_echo_server");
    let transport =
        StdioTransport::spawn(exe, &[], &BTreeMap::new()).expect("echo server should spawn");
    let client = McpClient::connect(Box::new(transport)).expect("handshake should succeed");

    assert_eq!(client.server_info().name, "echo-server");
    assert_eq!(client.server_info().protocol_version, "2025-06-18");

    let tools = client.list_tools().expect("tools should list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let result = client
        .call_tool("echo", &json!({"text": "hello heartflow"}))
        .expect("call should succeed");
    assert_eq!(result.text, "hello heartflow");
    assert!(!result.is_error);

    let error = client
        .call_tool("missing", &json!({}))
        .expect_err("unknown tool should fail");
    assert!(matches!(error, McpError::Protocol(message) if message.contains("unknown tool")));
}
