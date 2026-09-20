# 接入 MCP 服务

heartflow 既是 MCP **客户端**（挂载外部工具），本身也是一个可被别的 agent 调用的终端工具。本页讲如何给 hf 挂上现成的文档类 MCP 服务，让 agent 在写代码时拉到最新 API 文档。

## 远程 MCP（Streamable-HTTP / SSE）

给 `[mcp.servers.NAME]` 填 `url`（而非 `command`）即走远程传输。值支持 `${ENV_VAR}` 展开：

```toml
# Context7：拉取库的最新版本文档与代码示例（https://context7.com）
[mcp.servers.context7]
url = "https://mcp.context7.com/mcp"
headers = { "CONTEXT7_API_KEY" = "${CONTEXT7_API_KEY}" }  # 可选：提高限额/私有仓库
read_only = true

# DeepWiki：对任意 GitHub 仓库做 RAG 问答与架构解读（https://deepwiki.com）
[mcp.servers.deepwiki]
url = "https://mcp.deepwiki.com/sse"
read_only = true
```

配置后在 REPL 用 `/mcp` 列出已连服务器与工具。`read_only = true` 让这两个纯查询型服务在 `read-only` / `/plan` 受限模式下也放行。

## 本地 stdio MCP

给 `command`（+`args`）即走本地进程：

```toml
[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

## 失败行为

MCP 握手（initialize + tools/list）失败按 200/400ms 退避重试 3 次（幂等）；工具调用本身不自动重试（非幂等危险），交由模型层决策。原生工具优先于同名 MCP 工具。
