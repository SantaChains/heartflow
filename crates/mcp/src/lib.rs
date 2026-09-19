//! Model Context Protocol client over stdio and Streamable-HTTP (JSON-RPC 2.0).

mod client;
mod http;
mod protocol;
mod transport;

pub use client::{McpClient, McpError, McpServerInfo};
pub use http::HttpTransport;
pub use protocol::{McpTool, McpToolResult, PROTOCOL_VERSION};
pub use transport::{StdioTransport, Transport};
