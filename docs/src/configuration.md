# 配置

优先级从高到低：CLI 参数 > 项目 `.heartflow/config.toml` > 用户 `~/.heartflow/config.toml` > 内置 provider 表 > 环境变量。同名字段逐项覆盖，坏字段跳过并告警，单条配置不阻断启动。

REPL 运行期间编辑并保存任一 `config.toml`，下一回合会自动热重载（保留当前会话与权限模式）。环境异常可用 `hf doctor` 诊断，`hf doctor --fix` 应用安全修复。

```toml
version = 1

[provider.deepseek]
protocol = "openai"          # anthropic | openai(兼容别名 openai-compatible/openai_compat) | openai-responses(别名 responses)
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"   # 只写环境变量名；也可用 api_key 直接内联明文（导出时永不写出）
auth_token_env = "DEEPSEEK_AUTH_TOKEN"  # 可选：Bearer token 来源变量
model = "deepseek-chat"
max_tokens = 4096              # 缺省 4096
reasoning_effort = "high"      # 思考等级：openai 协议直传，anthropic 协议映射为 extended thinking 预算
context_window = 65536         # 模型上下文窗口 tokens，驱动回合内 >50% 预压缩（等价于 HEARTFLOW_AUTO_COMPACT_TOKENS，后者优先）

[provider.my-proxy]
protocol = "anthropic"
base_url = "https://my-proxy.example.com"
api_key_env = "MY_PROXY_TOKEN"

[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env = { "NODE_OPTIONS" = "--max-old-space-size=512" }  # 子进程环境变量

[mcp.servers.remote-search]            # 远程/网络型 MCP：给 url 即走 Streamable-HTTP/SSE（无需 command）
url = "https://mcp.example.com/stream" # 与 command 二选一；两者都缺则跳过并告警
headers = { "X-Tenant" = "acme" }      # 额外请求头；值支持 ${ENV_VAR} 展开
bearer_token_env = "SEARCH_MCP_TOKEN"  # 从此环境变量读 Bearer token（为空则不发）
read_only = true                       # 声明该 server 工具均只读：read-only/plan 模式下也放行
```

`[provider]` 的字段全部可选，未识别字段仅记 debug 日志，坏字段跳过而不阻断启动；`version` 高于当前 schema（1）时按现有 schema 解析并告警。

## 内置 provider

- `deepseek`：OpenAI 协议，`api_key_env = "DEEPSEEK_API_KEY"`，默认模型 `deepseek-v4-flash`。
- `anthropic`：Anthropic 协议，`api_key_env = "ANTHROPIC_API_KEY"`，基址可被 `ANTHROPIC_BASE_URL` 覆盖。
- 任意 `[provider.NAME]` 表项即自定义 provider；都未命中且无内置名时回退到环境变量选路，此时默认模型为 `mimo-v2.5-pro`。

## 终端配色（theme.toml）

配色以单一主题源为基准（Tokyo Night 冷色系），可按语义角色覆盖。文件位于 `~/.heartflow/theme.toml`（用户）与 `.heartflow/theme.toml`（项目，逐项胜出）。每个值取 `#RRGGBB`，坏值跳过并告警、绝不阻断启动；改后重启生效。

```toml
[theme]
heading = "#2ac3de"      # Markdown 标题
accent = "#7aa2f7"       # 提示符 / 活动 spinner / 补全高亮
muted = "#78829f"        # 次要文本（增量、空闲提示）
success = "#9ece6a"      # 完成
error = "#f7768e"        # 失败
# 其余可选：emphasis / strong / inline_code / link / quote
```
